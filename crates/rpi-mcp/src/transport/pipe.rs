use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use tokio::sync::Mutex as AsyncMutex;

use super::lines::{JsonRpcLineReader, lock};
use super::{DEFAULT_MAX_MESSAGE_BYTES, McpTransport, McpTransportError, TransportEvents};

const DEFAULT_MAX_STDERR_BYTES: usize = 64 * 1024;
const DEFAULT_CLOSE_TIMEOUT_MS: u64 = 2_000;

#[async_trait]
pub trait McpPipe: Send + Sync {
    async fn write(&self, data: &[u8]) -> Result<(), McpTransportError>;
    fn close_stdin(&self);
    fn close(&self);
    fn exited(&self) -> BoxFuture<'static, Result<(), McpTransportError>>;
}

pub type McpPipeOutputHandler = Arc<dyn Fn(&[u8]) + Send + Sync>;

#[derive(Clone)]
pub struct McpPipeHandlers {
    pub on_stdout: McpPipeOutputHandler,
    pub on_stderr: McpPipeOutputHandler,
}

pub type PipeOpenFn = Arc<
    dyn Fn(McpPipeHandlers) -> BoxFuture<'static, Result<Arc<dyn McpPipe>, McpTransportError>>
        + Send
        + Sync,
>;

#[derive(Clone)]
pub struct PipeTransportOptions {
    pub open: PipeOpenFn,
    pub max_message_bytes: Option<usize>,
    pub max_stderr_bytes: Option<usize>,
    pub on_stderr: Option<super::stdio::StderrCallback>,
    pub close_timeout_ms: Option<u64>,
}

impl PipeTransportOptions {
    pub fn new(open: PipeOpenFn) -> Self {
        Self {
            open,
            max_message_bytes: None,
            max_stderr_bytes: None,
            on_stderr: None,
            close_timeout_ms: None,
        }
    }
}

pub struct PipeTransport {
    options: PipeTransportOptions,
    events: Arc<TransportEvents>,
    lines: Arc<Mutex<JsonRpcLineReader>>,
    pipe: Arc<AsyncMutex<Option<Arc<dyn McpPipe>>>>,
    started: AtomicBool,
    closed: Arc<AtomicBool>,
    ended: Arc<AtomicBool>,
}

impl PipeTransport {
    pub fn new(options: PipeTransportOptions) -> Self {
        let events = Arc::new(TransportEvents::default());
        let lines = JsonRpcLineReader::new(
            events.clone(),
            options.on_stderr.clone(),
            options
                .max_message_bytes
                .unwrap_or(DEFAULT_MAX_MESSAGE_BYTES),
            options.max_stderr_bytes.unwrap_or(DEFAULT_MAX_STDERR_BYTES),
        );
        Self {
            options,
            events,
            lines: Arc::new(Mutex::new(lines)),
            pipe: Arc::new(AsyncMutex::new(None)),
            started: AtomicBool::new(false),
            closed: Arc::new(AtomicBool::new(false)),
            ended: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn stderr(&self) -> String {
        lock(&self.lines).stderr()
    }

    fn finish(
        events: Arc<TransportEvents>,
        lines: Arc<Mutex<JsonRpcLineReader>>,
        closed: Arc<AtomicBool>,
        ended: Arc<AtomicBool>,
        result: Result<(), McpTransportError>,
    ) {
        if let Err(error) = result
            && !closed.load(Ordering::SeqCst)
        {
            events.emit_error(&error);
        }
        ended.store(true, Ordering::SeqCst);
        lock(&lines).end();
        events.emit_close();
    }
}

#[async_trait]
impl McpTransport for PipeTransport {
    async fn start(&self) -> Result<(), McpTransportError> {
        if self.started.swap(true, Ordering::SeqCst) {
            return Err(McpTransportError::Other(
                "MCP pipe transport already started".to_owned(),
            ));
        }
        if self.closed.load(Ordering::SeqCst) {
            return Err(McpTransportError::ConnectionClosed);
        }
        let lines = self.lines.clone();
        let stderr_lines = lines.clone();
        let pipe = (self.options.open)(McpPipeHandlers {
            on_stdout: Arc::new(move |bytes| lock(&lines).read_stdout(bytes)),
            on_stderr: Arc::new(move |bytes| lock(&stderr_lines).read_stderr(bytes)),
        })
        .await?;
        if self.closed.load(Ordering::SeqCst) {
            pipe.close();
            return Err(McpTransportError::ConnectionClosed);
        }
        *self.pipe.lock().await = Some(pipe.clone());
        let events = self.events.clone();
        let lines = self.lines.clone();
        let closed = self.closed.clone();
        let ended = self.ended.clone();
        let exited = pipe.exited();
        let pipe_closed = self.pipe.clone();
        tokio::spawn(async move {
            let result = exited.await;
            pipe_closed.lock().await.take();
            Self::finish(events, lines, closed, ended, result);
        });
        Ok(())
    }

    async fn send(&self, message: serde_json::Value) -> Result<(), McpTransportError> {
        if self.closed.load(Ordering::SeqCst) || self.ended.load(Ordering::SeqCst) {
            return Err(McpTransportError::ConnectionClosed);
        }
        let pipe = self.pipe.lock().await.clone();
        let Some(pipe) = pipe else {
            return Err(McpTransportError::ConnectionClosed);
        };
        let payload = format!("{message}\n");
        pipe.write(payload.as_bytes()).await
    }

    async fn close(&self) -> Result<(), McpTransportError> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let pipe = self.pipe.lock().await.clone();
        let Some(pipe) = pipe else {
            self.events.emit_close();
            return Ok(());
        };
        if self.ended.load(Ordering::SeqCst) {
            return Ok(());
        }
        pipe.close_stdin();
        let timeout = Duration::from_millis(
            self.options
                .close_timeout_ms
                .unwrap_or(DEFAULT_CLOSE_TIMEOUT_MS),
        );
        let exited = tokio::time::timeout(timeout, pipe.exited()).await;
        if exited.is_err() {
            pipe.close();
            let _ = pipe.exited().await;
        }
        Ok(())
    }

    fn stderr_tail(&self) -> Option<String> {
        Some(self.stderr())
    }

    fn events(&self) -> Arc<TransportEvents> {
        self.events.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::JsonRpcMessage;

    struct FakePipe {
        writes: Mutex<Vec<u8>>,
        stdin_closed: AtomicBool,
        exited_flag: Arc<AtomicBool>,
        exited_notify: Arc<tokio::sync::Notify>,
    }

    impl FakePipe {
        fn new() -> Self {
            Self {
                writes: Mutex::new(Vec::new()),
                stdin_closed: AtomicBool::new(false),
                exited_flag: Arc::new(AtomicBool::new(false)),
                exited_notify: Arc::new(tokio::sync::Notify::new()),
            }
        }

        fn exit(&self) {
            self.exited_flag.store(true, Ordering::SeqCst);
            self.exited_notify.notify_waiters();
        }
    }

    #[async_trait]
    impl McpPipe for FakePipe {
        async fn write(&self, data: &[u8]) -> Result<(), McpTransportError> {
            lock(&self.writes).extend_from_slice(data);
            Ok(())
        }

        fn close_stdin(&self) {
            self.stdin_closed.store(true, Ordering::SeqCst);
        }

        fn close(&self) {
            self.exit();
        }

        fn exited(&self) -> BoxFuture<'static, Result<(), McpTransportError>> {
            let exited = self.exited_flag.clone();
            let notify = self.exited_notify.clone();
            Box::pin(async move {
                while !exited.load(Ordering::SeqCst) {
                    notify.notified().await;
                }
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn pipe_uses_shared_framing_and_closes_the_program() {
        let pipe = Arc::new(FakePipe::new());
        let handlers = Arc::new(Mutex::new(None::<McpPipeHandlers>));
        let options = PipeTransportOptions {
            open: Arc::new({
                let pipe = pipe.clone();
                let handlers = handlers.clone();
                move |next| {
                    *lock(&handlers) = Some(next);
                    let pipe = pipe.clone();
                    Box::pin(async move { Ok(pipe as Arc<dyn McpPipe>) })
                }
            }),
            close_timeout_ms: Some(0),
            ..PipeTransportOptions::new(Arc::new(|_| unreachable!()))
        };
        let transport = PipeTransport::new(options);
        let messages = Arc::new(Mutex::new(Vec::<JsonRpcMessage>::new()));
        let messages_callback = messages.clone();
        let subscription = transport.events().on_message(Arc::new(move |message| {
            lock(&messages_callback).push(message.clone());
        }));
        std::mem::forget(subscription);
        transport.start().await.expect("start");
        let handlers = lock(&handlers).clone().expect("handlers");
        (handlers.on_stdout)(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n");
        (handlers.on_stderr)(b"warning");
        transport
            .send(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"ping"}))
            .await
            .expect("send");
        assert_eq!(lock(&messages).len(), 1);
        assert_eq!(transport.stderr(), "warning");
        assert!(lock(&pipe.writes).ends_with(b"\n"));
        transport.close().await.expect("close");
        assert!(pipe.stdin_closed.load(Ordering::SeqCst));
        assert!(pipe.exited_flag.load(Ordering::SeqCst));
    }
}
