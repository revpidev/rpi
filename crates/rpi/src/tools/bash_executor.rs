//! Port of `packages/coding-agent/src/core/bash-executor.ts` @ pi 0.82.1 (2efa728).
//!
//! User-facing bash execution (`!` / `!!` commands). This is a **separate
//! path** from the bash tool — it sanitises output (stripAnsi +
//! sanitizeBinaryOutput), has no timeout, and does not inject `RPI_*`
//! session env vars.
//!
//! Intentional differences:
//! - Uses `&[u8]` from `BashOperations::exec` directly; the streaming UTF-8
//!   decoder is hand-rolled (no `TextDecoder` in Rust).
//! - Rolling-buffer byte counts use UTF-8 bytes (Rust `str::len`) rather than
//!   JS `string.length` (UTF-16 code units). The threshold is approximate and
//!   only affects memory bounding, not correctness.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::tools::bash::{BashExecError, BashExecOptions, BashOperations};
use crate::tools::random_hex_16;
use crate::tools::sanitize::{sanitize_binary_output, strip_ansi};
use crate::tools::truncate::{self, TruncateOptions, DEFAULT_MAX_BYTES};

/// Result of a user bash execution (bash-executor.ts:29-40).
pub struct BashResult {
    /// Combined stdout + stderr output (sanitized, possibly truncated).
    pub output: String,
    /// Process exit code (`None` if killed/cancelled).
    pub exit_code: Option<i32>,
    /// Whether the command was cancelled via signal.
    pub cancelled: bool,
    /// Whether the output was truncated.
    pub truncated: bool,
    /// Path to temp file containing full output, if created.
    pub full_output_path: Option<PathBuf>,
}

/// Callback for streaming sanitized output chunks.
pub type OnChunkCallback = Box<dyn Fn(&str) + Send + Sync>;

/// Options for [`execute_bash`].
pub struct BashExecutorOptions {
    /// Callback for streaming sanitized output chunks.
    pub on_chunk: Option<OnChunkCallback>,
    /// Cancellation signal.
    pub signal: CancellationToken,
}

/// `DEFAULT_MAX_BYTES * 2` = 100 KB rolling-buffer ceiling (bash-executor.ts:58).
const MAX_OUTPUT_BYTES: usize = DEFAULT_MAX_BYTES * 2;

/// Mutable state shared between the `on_data` callback and the outer function.
struct OutputState {
    output_chunks: Vec<String>,
    output_bytes: usize,
    total_bytes: usize,
    temp_file_path: Option<PathBuf>,
    temp_file: Option<File>,
    /// Streaming UTF-8 decoder state.
    pending: Vec<u8>,
}

impl OutputState {
    /// Ensure a temp file exists, flushing buffered chunks into it
    /// (bash-executor.ts:64-74).
    fn ensure_temp_file(&mut self) {
        if self.temp_file_path.is_some() {
            return;
        }
        let id = random_hex_16();
        let path = std::env::temp_dir().join(format!("pi-bash-{id}.log"));
        match File::create(&path) {
            Ok(mut file) => {
                for chunk in &self.output_chunks {
                    let _ = file.write_all(chunk.as_bytes());
                }
                self.temp_file = Some(file);
                self.temp_file_path = Some(path);
            }
            Err(_) => {
                // If temp file creation fails, keep buffering in memory.
            }
        }
    }

    /// Process one raw data chunk: decode, sanitize, buffer, and optionally
    /// write to temp file (bash-executor.ts:78-105).
    fn on_data(&mut self, data: &[u8], on_chunk: Option<&(dyn Fn(&str) + Send + Sync)>) {
        self.total_bytes += data.len();

        // Streaming UTF-8 decode (bash-executor.ts:76, 82).
        self.pending.extend_from_slice(data);
        let decoded = decode_streaming(&mut self.pending);
        if decoded.is_empty() {
            return;
        }

        // Sanitize: strip ANSI, remove binary garbage, drop \r (bash-executor.ts:82).
        let text = sanitize_binary_output(&strip_ansi(&decoded)).replace('\r', "");

        // Start writing to temp file if exceeds threshold (bash-executor.ts:85-87).
        if self.total_bytes > DEFAULT_MAX_BYTES {
            self.ensure_temp_file();
        }

        if let Some(ref mut file) = self.temp_file {
            let _ = file.write_all(text.as_bytes());
        }

        // Rolling buffer (bash-executor.ts:93-99).
        self.output_chunks.push(text.clone());
        self.output_bytes += text.len();
        while self.output_bytes > MAX_OUTPUT_BYTES && self.output_chunks.len() > 1 {
            let removed = self.output_chunks.remove(0);
            self.output_bytes = self.output_bytes.saturating_sub(removed.len());
        }

        // Stream to callback (bash-executor.ts:101-104).
        if let Some(cb) = on_chunk {
            cb(&text);
        }
    }
}

/// Execute a bash command using the given [`BashOperations`] (bash-executor.ts:50-155).
///
/// - **No timeout** — `BashExecOptions::timeout` is `None`.
/// - **No `RPI_*` injection** — `env` is `None`; the operations backend uses
///   its own `get_shell_env`.
/// - On abort: returns `BashResult { cancelled: true, .. }` instead of `Err`.
/// - On other errors: propagates the [`BashExecError`].
pub async fn execute_bash(
    command: &str,
    cwd: &Path,
    operations: &dyn BashOperations,
    options: BashExecutorOptions,
) -> Result<BashResult, BashExecError> {
    let state = Arc::new(Mutex::new(OutputState {
        output_chunks: Vec::new(),
        output_bytes: 0,
        total_bytes: 0,
        temp_file_path: None,
        temp_file: None,
        pending: Vec::new(),
    }));

    let on_chunk = Arc::new(options.on_chunk);

    // Build on_data as an `Fn` closure using interior mutability.
    let state_for_cb = Arc::clone(&state);
    let on_chunk_for_cb = Arc::clone(&on_chunk);
    let on_data = move |data: Vec<u8>| {
        let mut st = state_for_cb.lock().unwrap_or_else(|e| e.into_inner());
        st.on_data(&data, on_chunk_for_cb.as_deref());
    };

    // Execute (bash-executor.ts:107-111 — no timeout, no env).
    let exec_result = operations
        .exec(
            command,
            cwd,
            BashExecOptions {
                signal: options.signal.clone(),
                timeout: None,
                env: None,
            },
            &on_data,
        )
        .await;

    // Extract state (we need owned values for the final result).
    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());

    // Flush remaining pending bytes from the streaming decoder.
    if !st.pending.is_empty() {
        let decoded = String::from_utf8_lossy(&st.pending).into_owned();
        st.pending.clear();
        if !decoded.is_empty() {
            // Actually handle the decoded remainder directly.
            let text = sanitize_binary_output(&strip_ansi(&decoded)).replace('\r', "");
            if st.total_bytes > DEFAULT_MAX_BYTES {
                st.ensure_temp_file();
            }
            if let Some(ref mut file) = st.temp_file {
                let _ = file.write_all(text.as_bytes());
            }
            st.output_chunks.push(text.clone());
            st.output_bytes += text.len();
            while st.output_bytes > MAX_OUTPUT_BYTES && st.output_chunks.len() > 1 {
                let removed = st.output_chunks.remove(0);
                st.output_bytes = st.output_bytes.saturating_sub(removed.len());
            }
            if let Some(ref cb) = on_chunk.as_ref() {
                cb(&text);
            }
        }
    }

    // Build final result (bash-executor.ts:113-129).
    let full_output: String = st.output_chunks.drain(..).collect();
    let truncation_result = truncate::truncate_tail(&full_output, Some(TruncateOptions::default()));

    if truncation_result.truncated {
        st.ensure_temp_file();
    }

    // Flush temp file.
    if let Some(mut file) = st.temp_file.take() {
        let _ = file.flush();
    }

    let cancelled = options.signal.is_cancelled();
    let full_output_path = st.temp_file_path.take();

    match exec_result {
        Ok(exit_code) => Ok(BashResult {
            output: if truncation_result.truncated {
                truncation_result.content
            } else {
                full_output
            },
            exit_code: if cancelled { None } else { exit_code },
            cancelled,
            truncated: truncation_result.truncated,
            full_output_path,
        }),
        Err(BashExecError::Aborted) => {
            // Abort → return cancelled result (bash-executor.ts:130-148).
            Ok(BashResult {
                output: if truncation_result.truncated {
                    truncation_result.content
                } else {
                    full_output
                },
                exit_code: None,
                cancelled: true,
                truncated: truncation_result.truncated,
                full_output_path,
            })
        }
        Err(e) => Err(e),
    }
}

/// Streaming UTF-8 decoder: decode the valid prefix of `pending`, leaving
/// incomplete trailing bytes for the next call. Invalid bytes become U+FFFD.
///
/// Returns the decoded string and modifies `pending` in place.
fn decode_streaming(pending: &mut Vec<u8>) -> String {
    if pending.is_empty() {
        return String::new();
    }

    let mut result = String::new();

    loop {
        match std::str::from_utf8(pending) {
            Ok(s) => {
                result.push_str(s);
                pending.clear();
                return result;
            }
            Err(e) => {
                let valid_len = e.valid_up_to();
                if valid_len > 0 {
                    result.push_str(std::str::from_utf8(&pending[..valid_len]).unwrap_or(""));
                }
                match e.error_len() {
                    None => {
                        // Incomplete sequence at end — save for next chunk.
                        *pending = pending[valid_len..].to_vec();
                        return result;
                    }
                    Some(err_len) => {
                        result.push('\u{FFFD}');
                        *pending = pending[valid_len + err_len..].to_vec();
                        if pending.is_empty() {
                            return result;
                        }
                    }
                }
            }
        }
    }
}
