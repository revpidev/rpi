use std::sync::{Arc, Mutex};

use bytes::{Bytes, BytesMut};

use super::{McpTransportError, TransportEvents};
use crate::protocol::parse_json_rpc_message;
use crate::transport::stdio::StderrCallback;

pub(crate) struct JsonRpcLineReader {
    events: Arc<TransportEvents>,
    stdout_chunks: Vec<Bytes>,
    stdout_pending_bytes: usize,
    stderr: Vec<u8>,
    on_stderr: Option<StderrCallback>,
    max_message_bytes: usize,
    max_stderr_bytes: usize,
    pending_joins: usize,
}

impl JsonRpcLineReader {
    pub(crate) fn new(
        events: Arc<TransportEvents>,
        on_stderr: Option<StderrCallback>,
        max_message_bytes: usize,
        max_stderr_bytes: usize,
    ) -> Self {
        Self {
            events,
            stdout_chunks: Vec::new(),
            stdout_pending_bytes: 0,
            stderr: Vec::new(),
            on_stderr,
            max_message_bytes,
            max_stderr_bytes,
            pending_joins: 0,
        }
    }

    pub(crate) fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    pub(crate) fn read_stdout(&mut self, chunk: &[u8]) {
        let mut start = 0;
        while let Some(relative_newline) = chunk[start..].iter().position(|byte| *byte == b'\n') {
            let newline = start + relative_newline;
            let line = if self.stdout_chunks.is_empty() {
                Bytes::copy_from_slice(&chunk[start..newline])
            } else {
                self.stdout_chunks
                    .push(Bytes::copy_from_slice(&chunk[start..newline]));
                let mut joined =
                    BytesMut::with_capacity(self.stdout_pending_bytes + newline - start);
                for chunk in self.stdout_chunks.drain(..) {
                    joined.extend_from_slice(&chunk);
                }
                self.stdout_pending_bytes = 0;
                self.pending_joins += 1;
                joined.into()
            };
            start = newline + 1;
            self.process_line(&line);
        }
        if start < chunk.len() {
            self.stdout_chunks
                .push(Bytes::copy_from_slice(&chunk[start..]));
            self.stdout_pending_bytes += chunk.len() - start;
        }
        if self.stdout_pending_bytes > self.max_message_bytes {
            self.stdout_chunks.clear();
            self.stdout_pending_bytes = 0;
            self.events.emit_error(&McpTransportError::Other(format!(
                "MCP stdio message exceeds {} bytes",
                self.max_message_bytes
            )));
        }
    }

    fn process_line(&mut self, line: &[u8]) {
        if line.len() > self.max_message_bytes {
            self.events.emit_error(&McpTransportError::Other(format!(
                "MCP stdio message exceeds {} bytes",
                self.max_message_bytes
            )));
            return;
        }
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let text = String::from_utf8_lossy(line);
        if text.trim().is_empty() {
            return;
        }
        let parsed = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(parse_json_rpc_message);
        match parsed {
            Some(message) => self.events.emit_message(&message),
            None => self.events.emit_error(&McpTransportError::Other(format!(
                "MCP stdio server sent an invalid JSON-RPC message: {}",
                truncate(&text, 200)
            ))),
        }
    }

    pub(crate) fn read_stderr(&mut self, chunk: &[u8]) {
        self.stderr.extend_from_slice(chunk);
        if self.stderr.len() > self.max_stderr_bytes {
            let drop = self.stderr.len() - self.max_stderr_bytes;
            self.stderr.drain(..drop);
        }
        if let Some(on_stderr) = &self.on_stderr {
            on_stderr(&String::from_utf8_lossy(chunk));
        }
    }

    pub(crate) fn end(&mut self) {
        let mut pending = BytesMut::with_capacity(self.stdout_pending_bytes);
        for chunk in self.stdout_chunks.drain(..) {
            pending.extend_from_slice(&chunk);
        }
        self.stdout_pending_bytes = 0;
        if !String::from_utf8_lossy(&pending).trim().is_empty() {
            self.events.emit_error(&McpTransportError::Other(
                "MCP stdio server closed with an incomplete JSON-RPC message".to_owned(),
            ));
        }
    }
}

fn truncate(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((index, _)) => &text[..index],
        None => text,
    }
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::JsonRpcMessage;
    use crate::transport::DEFAULT_MAX_MESSAGE_BYTES;

    type TestReader = (
        Arc<Mutex<JsonRpcLineReader>>,
        Arc<Mutex<Vec<JsonRpcMessage>>>,
        Arc<Mutex<Vec<String>>>,
    );

    fn reader(max_message_bytes: usize) -> TestReader {
        let events = Arc::new(TransportEvents::default());
        let messages = Arc::new(Mutex::new(Vec::new()));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let messages_callback = messages.clone();
        let message_subscription = events.on_message(Arc::new(move |message| {
            messages_callback.lock().unwrap().push(message.clone());
        }));
        std::mem::forget(message_subscription);
        let errors_callback = errors.clone();
        let error_subscription = events.on_error(Arc::new(move |error| {
            errors_callback.lock().unwrap().push(error.to_string());
        }));
        std::mem::forget(error_subscription);
        (
            Arc::new(Mutex::new(JsonRpcLineReader::new(
                events,
                None,
                max_message_bytes,
                64 * 1024,
            ))),
            messages,
            errors,
        )
    }

    #[test]
    fn joins_a_large_message_once_when_its_newline_arrives() {
        let (reader, messages, errors) = reader(DEFAULT_MAX_MESSAGE_BYTES);
        let text = "x".repeat(1024 * 1024);
        let bytes = format!(
            "{}\n",
            serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"text":text}})
        );
        for chunk in bytes.as_bytes().chunks(4096) {
            lock(&reader).read_stdout(chunk);
        }
        assert_eq!(lock(&reader).pending_joins, 1);
        assert!(lock(&errors).is_empty());
        assert_eq!(lock(&messages).len(), 1);
    }

    #[test]
    fn handles_mixed_input_at_every_split() {
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"text\":\"héllo 🌍\"}}\n",
            "\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/x\"}\r\n",
            "not json\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}\n",
            "\n"
        );
        for chunk_size in 1..=input.len() {
            let (reader, messages, errors) = reader(DEFAULT_MAX_MESSAGE_BYTES);
            for chunk in input.as_bytes().chunks(chunk_size) {
                lock(&reader).read_stdout(chunk);
            }
            lock(&reader).end();
            assert_eq!(lock(&messages).len(), 3, "chunk size {chunk_size}");
            assert_eq!(lock(&errors).len(), 1, "chunk size {chunk_size}");
        }
    }

    #[test]
    fn drops_an_oversized_message_and_recovers() {
        let (reader, messages, errors) = reader(64);
        for chunk in "a".repeat(50).as_bytes().chunks(10) {
            lock(&reader).read_stdout(chunk);
        }
        let next = "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}";
        for chunk in format!("{}\n{next}\n", "a".repeat(50))
            .as_bytes()
            .chunks(10)
        {
            lock(&reader).read_stdout(chunk);
        }
        assert!(lock(&errors)[0].contains("exceeds 64 bytes"));
        assert_eq!(lock(&messages).len(), 1);
    }

    #[test]
    fn reports_incomplete_stdout_and_keeps_stderr_tail() {
        let (reader, _, errors) = reader(DEFAULT_MAX_MESSAGE_BYTES);
        for chunk in b"{\"jsonrpc\":\"2.0\",".chunks(4) {
            lock(&reader).read_stdout(chunk);
        }
        lock(&reader).read_stderr(b"prefix");
        lock(&reader).read_stderr(&vec![b'x'; 64 * 1024]);
        lock(&reader).end();
        assert!(
            lock(&errors)
                .iter()
                .any(|error| error.contains("incomplete"))
        );
        assert_eq!(lock(&reader).stderr().len(), 64 * 1024);
    }
}
