//! `VirtualTerminal` — frame-recording terminal sink for TUI tests
//! (coding-standards §12.5; requirements §11.1). Reused by T11/T12.
//!
//! Components under test never write to a real terminal; they emit ANSI text
//! that a test feeds into `VirtualTerminal`. Assertions run against:
//! - recorded frames (`frames`),
//! - frames with CSI 2026 synchronized-output jitter removed
//!   (`sanitized_frames`) — Pi wraps redraws in `ESC[?2026h` / `ESC[?2026l`,
//!   whose presence is timing-dependent and excluded from parity diffs,
//! - plain rendered text with ANSI escape sequences stripped (`plain_text`).

/// CSI 2026 synchronized-output begin marker (upstream redraw wrapping).
pub const SYNC_BEGIN: &str = "\x1b[?2026h";
/// CSI 2026 synchronized-output end marker.
pub const SYNC_END: &str = "\x1b[?2026l";

/// Records terminal output frame by frame.
#[derive(Debug, Default)]
pub struct VirtualTerminal {
    frames: Vec<String>,
    current: String,
}

impl VirtualTerminal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append output to the current frame.
    pub fn write(&mut self, data: &str) {
        self.current.push_str(data);
    }

    /// Commit the current frame (if non-empty) and start a new one.
    pub fn end_frame(&mut self) {
        if !self.current.is_empty() {
            self.frames.push(std::mem::take(&mut self.current));
        }
    }

    /// All committed frames plus any pending partial frame.
    pub fn frames(&self) -> Vec<String> {
        let mut all = self.frames.clone();
        if !self.current.is_empty() {
            all.push(self.current.clone());
        }
        all
    }

    /// Frames with CSI 2026 sync markers removed (the stable subset used for
    /// parity diffs against Pi virtual-terminal output).
    pub fn sanitized_frames(&self) -> Vec<String> {
        self.frames().iter().map(|f| sanitize_frame(f)).collect()
    }

    /// Concatenated sanitized output of all frames.
    pub fn sanitized_output(&self) -> String {
        self.sanitized_frames().concat()
    }

    /// Plain text: sanitized output with remaining ANSI escape sequences
    /// stripped (CSI `ESC [ … final`, plus two-byte `ESC X` sequences).
    pub fn plain_text(&self) -> String {
        strip_ansi(&self.sanitized_output())
    }

    pub fn frame_count(&self) -> usize {
        self.frames().len()
    }

    pub fn clear(&mut self) {
        self.frames.clear();
        self.current.clear();
    }
}

/// Remove CSI 2026 synchronized-output markers from a frame.
pub fn sanitize_frame(frame: &str) -> String {
    frame.replace(SYNC_BEGIN, "").replace(SYNC_END, "")
}

/// Strip ANSI escape sequences (CSI and simple two-byte escapes). OSC and
/// other multi-byte sequences are out of scope for component snapshot tests.
pub fn strip_ansi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            match bytes.get(i + 1) {
                Some(b'[') => {
                    // CSI: ESC [ params… final-byte (0x40–0x7E)
                    let mut j = i + 2;
                    while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                        j += 1;
                    }
                    i = (j + 1).min(bytes.len());
                    continue;
                }
                Some(_) => {
                    i += 2;
                    continue;
                }
                None => {
                    i += 1;
                    continue;
                }
            }
        }
        // Invariant: `i` is on a char boundary — it starts at 0 and advances
        // by whole chars or ASCII-only escape spans.
        let ch = input[i..].chars().next().expect("i is a char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vt_records_frames() {
        let mut vt = VirtualTerminal::new();
        vt.write("hello");
        vt.end_frame();
        vt.write("wor");
        vt.write("ld");
        vt.end_frame();
        assert_eq!(vt.frames(), vec!["hello".to_owned(), "world".to_owned()]);
        assert_eq!(vt.frame_count(), 2);
    }

    #[test]
    fn test_vt_pending_frame_included() {
        let mut vt = VirtualTerminal::new();
        vt.write("partial");
        assert_eq!(vt.frames(), vec!["partial".to_owned()]);
    }

    #[test]
    fn test_vt_sanitize_removes_sync_markers() {
        let mut vt = VirtualTerminal::new();
        vt.write(&format!("{SYNC_BEGIN}line{SYNC_END}"));
        vt.end_frame();
        assert_eq!(vt.sanitized_frames(), vec!["line".to_owned()]);
        assert_eq!(vt.frames()[0], format!("{SYNC_BEGIN}line{SYNC_END}"));
    }

    #[test]
    fn test_strip_ansi_csi_and_simple() {
        assert_eq!(strip_ansi("\x1b[1;31mred\x1b[0m plain"), "red plain");
        assert_eq!(strip_ansi("a\x1b[2Kb"), "ab");
        assert_eq!(strip_ansi("\x1bMscroll"), "scroll");
        assert_eq!(strip_ansi("keep 中文"), "keep 中文");
    }
}
