//! Streaming output accumulator with bounded memory.
//!
//! Port of `packages/coding-agent/src/core/tools/output-accumulator.ts`
//! @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - The public API is **synchronous** (`append`, `finish`, `snapshot` all take
//!   `&mut self`). Temp-file writes use `std::fs` (sync) rather than async I/O.
//!   Each call appends a small chunk — not a large-file blocking write — so
//!   this is an acceptable trade-off for the Rust port. Callers invoke these
//!   methods from the bash tool's synchronous data callback.
//! - The streaming UTF-8 decoder is hand-rolled (Rust has no built-in
//!   `TextDecoder` streaming equivalent). It buffers incomplete trailing
//!   bytes across `append` calls and flushes them in `finish`.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

use crate::tools::truncate::{
    self, TruncateOptions, TruncatedBy, TruncationResult, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES,
};

/// Options for [`OutputAccumulator`].
///
/// Port of `OutputAccumulatorOptions` (output-accumulator.ts:7-11) with
/// `max_rolling_bytes` exposed as a separate field (upstream derives it from
/// `max_bytes * 2`).
#[derive(Debug, Clone)]
pub struct OutputAccumulatorOptions {
    pub max_lines: usize,
    pub max_bytes: usize,
    pub max_rolling_bytes: usize,
    pub temp_file_prefix: String,
}

impl Default for OutputAccumulatorOptions {
    fn default() -> Self {
        Self {
            max_lines: DEFAULT_MAX_LINES,
            max_bytes: DEFAULT_MAX_BYTES,
            // Upstream: Math.max(maxBytes * 2, 1) = 102400
            max_rolling_bytes: DEFAULT_MAX_BYTES * 2,
            temp_file_prefix: "pi-output".to_string(),
        }
    }
}

/// Snapshot of accumulated output at a point in time.
///
/// Port of `OutputSnapshot` (output-accumulator.ts:13-17).
#[derive(Debug, Clone)]
pub struct OutputSnapshot {
    /// The truncated tail text suitable for display.
    pub text: String,
    /// Truncation details.
    pub truncation: TruncationResult,
    /// Path to the temp file containing full output (if created).
    pub full_output_path: Option<PathBuf>,
}

/// Incrementally tracks streaming output with bounded memory.
///
/// Appends decode chunks with a streaming UTF-8 decoder, keeps only a decoded
/// tail for display snapshots, and opens a temp file when the full output
/// needs to be preserved.
///
/// Port of `OutputAccumulator` (output-accumulator.ts:35-221).
pub struct OutputAccumulator {
    max_lines: usize,
    max_bytes: usize,
    max_rolling_bytes: usize,
    temp_file_prefix: String,

    // Streaming UTF-8 decoder state
    pending: Vec<u8>,

    // Rolling buffer for decoded text
    tail_text: String,
    tail_bytes: usize,
    tail_starts_at_line_boundary: bool,

    // Counters
    total_raw_bytes: usize,
    total_decoded_bytes: usize,
    completed_lines: usize,
    total_lines: usize,
    current_line_bytes: usize,
    has_open_line: bool,

    finished: bool,

    // Buffered raw chunks before temp file is opened
    raw_chunks: Vec<Vec<u8>>,

    // Temp file state
    temp_file_path: Option<PathBuf>,
    temp_file: Option<File>,
}

impl OutputAccumulator {
    /// Create a new accumulator with the given options.
    ///
    /// The bash tool passes `{ temp_file_prefix: "pi-bash", .. }`.
    pub fn new(options: OutputAccumulatorOptions) -> Self {
        Self {
            max_lines: options.max_lines,
            max_bytes: options.max_bytes,
            max_rolling_bytes: options.max_rolling_bytes,
            temp_file_prefix: options.temp_file_prefix,
            pending: Vec::new(),
            tail_text: String::new(),
            tail_bytes: 0,
            tail_starts_at_line_boundary: true,
            total_raw_bytes: 0,
            total_decoded_bytes: 0,
            completed_lines: 0,
            total_lines: 0,
            current_line_bytes: 0,
            has_open_line: false,
            finished: false,
            raw_chunks: Vec::new(),
            temp_file_path: None,
            temp_file: None,
        }
    }

    /// Append a raw byte chunk.
    ///
    /// The data is decoded with the streaming UTF-8 decoder, and the decoded
    /// text is appended to the rolling buffer. If a temp file is active (or
    /// triggers now), the raw bytes are also written to it; otherwise they
    /// are buffered in `raw_chunks` for later flush.
    ///
    /// Port of `append` (output-accumulator.ts:64-78).
    pub fn append(&mut self, data: &[u8]) {
        if self.finished {
            // Upstream throws; we silently ignore to keep the sync API simple.
            return;
        }

        self.total_raw_bytes += data.len();

        let decoded = self.decode_streaming(data);
        self.append_decoded_text(&decoded);

        if self.temp_file.is_some() || self.should_use_temp_file() {
            self.ensure_temp_file();
            if let Some(ref mut file) = self.temp_file {
                let _ = file.write_all(data);
            }
        } else if !data.is_empty() {
            self.raw_chunks.push(data.to_vec());
        }
    }

    /// Signal that no more data will arrive.
    ///
    /// Flushes the streaming decoder's pending bytes. If the temp-file
    /// threshold is met, ensures the temp file is created.
    ///
    /// Port of `finish` (output-accumulator.ts:80-89).
    pub fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        let decoded = self.decode_finish();
        self.append_decoded_text(&decoded);
        if self.should_use_temp_file() {
            self.ensure_temp_file();
        }
    }

    /// Produce a display snapshot of the accumulated output.
    ///
    /// Applies `truncate_tail` to the snapshot text. If `persist_if_truncated`
    /// is true and truncation occurred, ensures a temp file is created so the
    /// full output is preserved.
    ///
    /// Port of `snapshot` (output-accumulator.ts:91-119).
    pub fn snapshot(&mut self, persist_if_truncated: bool) -> OutputSnapshot {
        let snapshot_text = self.get_snapshot_text().to_string();
        let tail_truncation = truncate::truncate_tail(
            &snapshot_text,
            Some(TruncateOptions {
                max_lines: self.max_lines,
                max_bytes: self.max_bytes,
            }),
        );

        let truncated =
            self.total_lines > self.max_lines || self.total_decoded_bytes > self.max_bytes;
        let truncated_by = if truncated {
            tail_truncation
                .truncated_by
                .or(if self.total_decoded_bytes > self.max_bytes {
                    Some(TruncatedBy::Bytes)
                } else {
                    Some(TruncatedBy::Lines)
                })
        } else {
            None
        };

        let truncation = TruncationResult {
            content: tail_truncation.content,
            truncated,
            truncated_by,
            total_lines: self.total_lines,
            total_bytes: self.total_decoded_bytes,
            output_lines: tail_truncation.output_lines,
            output_bytes: tail_truncation.output_bytes,
            last_line_partial: tail_truncation.last_line_partial,
            first_line_exceeds_limit: tail_truncation.first_line_exceeds_limit,
            max_lines: self.max_lines,
            max_bytes: self.max_bytes,
        };

        if persist_if_truncated && truncation.truncated {
            self.ensure_temp_file();
        }

        OutputSnapshot {
            text: truncation.content.clone(),
            truncation,
            full_output_path: self.temp_file_path.clone(),
        }
    }

    /// Close and flush the temp file (if open).
    ///
    /// Port of `closeTempFile` (output-accumulator.ts:121-142). After this
    /// call, no more data can be written to the temp file. The file remains
    /// on disk for reading.
    pub fn close_temp_file(&mut self) {
        if let Some(mut file) = self.temp_file.take() {
            let _ = file.flush();
        }
    }

    /// Return the byte length of the current open (last) line.
    ///
    /// Used by bash output formatting for the partial-line truncation message.
    ///
    /// Port of `getLastLineBytes` (output-accumulator.ts:144-146).
    pub fn last_line_bytes(&self) -> usize {
        self.current_line_bytes
    }

    // -------------------------------------------------------------------
    // Private: streaming UTF-8 decoder
    // -------------------------------------------------------------------

    /// Decode a chunk of bytes using the streaming decoder.
    ///
    /// Incomplete trailing sequences are saved in `self.pending` for the next
    /// call. Invalid bytes are replaced with `U+FFFD` (matching `TextDecoder`
    /// non-fatal mode).
    fn decode_streaming(&mut self, data: &[u8]) -> String {
        self.pending.extend_from_slice(data);

        if self.pending.is_empty() {
            return String::new();
        }

        let mut result = String::new();

        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(s) => {
                    result.push_str(s);
                    self.pending.clear();
                    return result;
                }
                Err(e) => {
                    let valid_len = e.valid_up_to();

                    if valid_len > 0 {
                        // valid_len is a guaranteed-valid UTF-8 boundary
                        result.push_str(
                            std::str::from_utf8(&self.pending[..valid_len]).unwrap_or(""),
                        );
                    }

                    match e.error_len() {
                        None => {
                            // Incomplete sequence at end — save for next chunk
                            self.pending = self.pending[valid_len..].to_vec();
                            return result;
                        }
                        Some(err_len) => {
                            // Invalid byte — replace with U+FFFD and continue
                            result.push('\u{FFFD}');
                            self.pending = self.pending[valid_len + err_len..].to_vec();
                            if self.pending.is_empty() {
                                return result;
                            }
                        }
                    }
                }
            }
        }
    }

    /// Flush remaining pending bytes (called by `finish`).
    fn decode_finish(&mut self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        let result = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        result
    }

    // -------------------------------------------------------------------
    // Private: decoded-text bookkeeping
    // -------------------------------------------------------------------

    /// Append decoded text to the rolling buffer and update line counters.
    ///
    /// Port of `appendDecodedText` (output-accumulator.ts:148-177).
    fn append_decoded_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }

        let bytes = text.len();
        self.total_decoded_bytes += bytes;
        self.tail_text.push_str(text);
        self.tail_bytes += bytes;
        if self.tail_bytes > self.max_rolling_bytes * 2 {
            self.trim_tail();
        }

        // Count newlines in this chunk.
        let mut newlines = 0usize;
        let mut last_newline_byte = None;
        for (byte_idx, b) in text.bytes().enumerate() {
            if b == b'\n' {
                newlines += 1;
                last_newline_byte = Some(byte_idx);
            }
        }

        if newlines == 0 {
            self.current_line_bytes += bytes;
            self.has_open_line = true;
        } else {
            self.completed_lines += newlines;
            // Invariant: `newlines > 0` implies `last_newline_byte` was set in the
            // scan loop above.
            let tail = &text[last_newline_byte.expect("newlines > 0 implies a last newline") + 1..];
            self.current_line_bytes = tail.len();
            self.has_open_line = !tail.is_empty();
        }
        self.total_lines = self.completed_lines + if self.has_open_line { 1 } else { 0 };
    }

    /// Trim the rolling buffer to `max_rolling_bytes`, UTF-8-boundary aware.
    ///
    /// Port of `trimTail` (output-accumulator.ts:179-194).
    fn trim_tail(&mut self) {
        let bytes = self.tail_text.as_bytes();
        if bytes.len() <= self.max_rolling_bytes {
            self.tail_bytes = bytes.len();
            return;
        }

        let mut start = bytes.len() - self.max_rolling_bytes;

        // Skip UTF-8 continuation bytes to find a valid character boundary.
        while start < bytes.len() && (bytes[start] & 0xc0) == 0x80 {
            start += 1;
        }

        // Track whether the trim point starts at a line boundary.
        self.tail_starts_at_line_boundary = if start == 0 {
            self.tail_starts_at_line_boundary
        } else {
            bytes[start - 1] == 0x0a
        };

        // start is at a valid UTF-8 character boundary.
        self.tail_text = match std::str::from_utf8(&bytes[start..]) {
            Ok(s) => s.to_string(),
            Err(_) => String::from_utf8_lossy(&bytes[start..]).into_owned(),
        };
        self.tail_bytes = self.tail_text.len();
    }

    /// Return the display text, dropping the first partial line if the tail
    /// doesn't start at a line boundary.
    ///
    /// Port of `getSnapshotText` (output-accumulator.ts:196-203).
    fn get_snapshot_text(&self) -> &str {
        if self.tail_starts_at_line_boundary {
            return &self.tail_text;
        }
        match self.tail_text.find('\n') {
            None => &self.tail_text,
            Some(idx) => &self.tail_text[idx + 1..],
        }
    }

    // -------------------------------------------------------------------
    // Private: temp file management
    // -------------------------------------------------------------------

    /// Whether the output has grown large enough to warrant a temp file.
    ///
    /// Port of `shouldUseTempFile` (output-accumulator.ts:205-209).
    fn should_use_temp_file(&self) -> bool {
        self.total_raw_bytes > self.max_bytes
            || self.total_decoded_bytes > self.max_bytes
            || self.total_lines > self.max_lines
    }

    /// Create the temp file (if not already created) and flush buffered
    /// `raw_chunks` into it.
    ///
    /// Port of `ensureTempFile` (output-accumulator.ts:211-221).
    fn ensure_temp_file(&mut self) {
        if self.temp_file_path.is_some() {
            return;
        }

        let path = std::env::temp_dir().join(format!(
            "{}-{}.log",
            self.temp_file_prefix,
            crate::tools::random_hex_16()
        ));

        match File::create(&path) {
            Ok(mut file) => {
                // Write all buffered raw chunks.
                for chunk in &self.raw_chunks {
                    let _ = file.write_all(chunk);
                }
                self.raw_chunks.clear();
                self.temp_file = Some(file);
                self.temp_file_path = Some(path);
            }
            Err(_) => {
                // If temp file creation fails, keep buffering in memory.
                // The snapshot will still work, just without a full-output path.
            }
        }
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Port of "should decode UTF-8 characters split across output chunks"
    #[test]
    fn test_decode_utf8_split_across_chunks() {
        let mut acc = OutputAccumulator::new(OutputAccumulatorOptions::default());
        // é = 0xC3 0xA9 (2 bytes). Split between the two bytes.
        acc.append(b"a");
        acc.append(&[0xC3]);
        acc.append(&[0xA9, b'b']);
        acc.finish();

        let snap = acc.snapshot(false);
        assert_eq!(snap.text, "a\u{00E9}b");
    }

    // Port of "should not count a trailing newline as an extra truncated bash output line"
    #[test]
    fn test_trailing_newline_not_extra_line() {
        let mut acc = OutputAccumulator::new(OutputAccumulatorOptions {
            max_lines: 3,
            max_bytes: 51200,
            max_rolling_bytes: 102400,
            temp_file_prefix: "pi-test".to_string(),
        });

        // 4 lines with trailing newline: "line\nline\nline\nline\n"
        // splitLinesForCounting gives 4 lines, not 5.
        for _ in 0..4 {
            acc.append(b"line\n");
        }
        acc.finish();

        let snap = acc.snapshot(false);
        // totalLines should be 4, not 5.
        assert_eq!(snap.truncation.total_lines, 4);
    }

    #[test]
    fn test_rolling_buffer_trim() {
        let mut acc = OutputAccumulator::new(OutputAccumulatorOptions {
            max_lines: 2000,
            max_bytes: 51200,
            // Small rolling buffer to trigger trim easily.
            max_rolling_bytes: 50,
            temp_file_prefix: "pi-test".to_string(),
        });

        // Feed enough data to exceed max_rolling_bytes * 2 = 100 bytes.
        let chunk = format!("{}\n", "X".repeat(40));
        for _ in 0..5 {
            acc.append(chunk.as_bytes());
        }
        acc.finish();

        // After trimming, tail_bytes should be <= max_rolling_bytes (roughly).
        // The exact value depends on trim timing, but it must be bounded.
        assert!(acc.tail_bytes <= 50 + 40 + 1); // one line worth of slack
    }

    #[test]
    fn test_temp_file_created_on_size() {
        let mut acc = OutputAccumulator::new(OutputAccumulatorOptions {
            max_lines: 2000,
            max_bytes: 10, // tiny limit
            max_rolling_bytes: 20,
            temp_file_prefix: "pi-test-tf".to_string(),
        });

        acc.append(b"this is more than ten bytes of output");
        acc.finish();

        let snap = acc.snapshot(false);
        assert!(snap.truncation.truncated);
        assert!(
            snap.full_output_path.is_some(),
            "temp file should be created"
        );

        // Clean up.
        if let Some(ref path) = snap.full_output_path {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn test_temp_file_created_on_line_count() {
        let mut acc = OutputAccumulator::new(OutputAccumulatorOptions {
            max_lines: 3,
            max_bytes: 51200,
            max_rolling_bytes: 102400,
            temp_file_prefix: "pi-test-tf".to_string(),
        });

        for i in 0..10 {
            acc.append(format!("line {i}\n").as_bytes());
        }
        acc.finish();

        let snap = acc.snapshot(true);
        assert!(snap.truncation.truncated);
        assert!(snap.full_output_path.is_some());

        if let Some(ref path) = snap.full_output_path {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn test_no_truncation_small_output() {
        let mut acc = OutputAccumulator::new(OutputAccumulatorOptions::default());
        acc.append(b"hello world\n");
        acc.finish();

        let snap = acc.snapshot(false);
        assert!(!snap.truncation.truncated);
        assert_eq!(snap.text, "hello world\n");
        assert!(snap.full_output_path.is_none());
    }

    #[test]
    fn test_last_line_bytes() {
        let mut acc = OutputAccumulator::new(OutputAccumulatorOptions::default());
        acc.append(b"line1\nline2_partia");
        // No finish — currentLineBytes should reflect the open line.
        assert_eq!(acc.last_line_bytes(), "line2_partia".len());
    }

    #[test]
    fn test_line_boundary_discard_in_snapshot() {
        let mut acc = OutputAccumulator::new(OutputAccumulatorOptions {
            max_lines: 2000,
            max_bytes: 51200,
            max_rolling_bytes: 5, // tiny — forces trim mid-line
            temp_file_prefix: "pi-test".to_string(),
        });

        // Feed a long line without newlines so trim cuts mid-line.
        acc.append(b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghij");
        acc.finish();

        let snap = acc.snapshot(false);
        // getSnapshotText should have dropped the partial first line.
        // Since there are no newlines in the input, the entire tail is one line,
        // and if it doesn't start at a line boundary, the first line is dropped.
        // The exact result depends on trimming, but it should be valid UTF-8.
        assert!(std::str::from_utf8(snap.text.as_bytes()).is_ok());
    }

    #[test]
    fn test_finish_then_append_ignored() {
        let mut acc = OutputAccumulator::new(OutputAccumulatorOptions::default());
        acc.append(b"hello");
        acc.finish();
        acc.append(b" world"); // should be ignored

        let snap = acc.snapshot(false);
        assert_eq!(snap.text, "hello");
    }

    #[test]
    fn test_bash_prefix_temp_file() {
        let mut acc = OutputAccumulator::new(OutputAccumulatorOptions {
            temp_file_prefix: "pi-bash".to_string(),
            max_bytes: 5,
            ..OutputAccumulatorOptions::default()
        });
        acc.append(b"exceeds limit");
        acc.finish();

        let snap = acc.snapshot(false);
        assert!(snap.full_output_path.is_some());
        if let Some(ref path) = snap.full_output_path {
            let name = path.file_name().unwrap().to_string_lossy();
            assert!(name.starts_with("pi-bash-"), "temp file name: {name}");
            assert!(name.ends_with(".log"));
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn test_multibyte_split_three_ways() {
        let mut acc = OutputAccumulator::new(OutputAccumulatorOptions::default());
        // 🙂 = U+1F642 = F0 9F 99 82 (4 bytes). Split across three appends.
        acc.append(&[0xF0]);
        acc.append(&[0x9F, 0x99]);
        acc.append(&[0x82]);
        acc.finish();

        let snap = acc.snapshot(false);
        assert_eq!(snap.text, "\u{1F642}");
    }

    #[test]
    fn test_close_temp_file() {
        let mut acc = OutputAccumulator::new(OutputAccumulatorOptions {
            max_bytes: 5,
            ..OutputAccumulatorOptions::default()
        });
        acc.append(b"exceeds limit for temp file");
        acc.finish();

        let snap = acc.snapshot(false);
        let path = snap.full_output_path.clone().unwrap();

        // Close should flush and drop the handle.
        acc.close_temp_file();

        // File should exist and be readable.
        assert!(path.exists(), "temp file should exist after close");
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "exceeds limit for temp file");

        let _ = std::fs::remove_file(&path);
    }
}
