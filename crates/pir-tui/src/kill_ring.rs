//! Port of `packages/tui/src/kill-ring.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - `push` takes a `String` (upstream `string`); `peek` returns `Option<&str>`
//!   instead of `string | undefined`. `KillRingPushOptions` mirrors the
//!   upstream `{ prepend: boolean; accumulate?: boolean }` argument shape;
//!   `accumulate` defaults to `false` via [`Default`].
//!
//! Ring buffer for Emacs-style kill/yank operations. Tracks killed (deleted)
//! text entries; consecutive kills can accumulate into a single entry.
//! Supports yank (paste most recent) and yank-pop (cycle through older
//! entries).

/// Push options for [`KillRing::push`] (upstream `{ prepend, accumulate? }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KillRingPushOptions {
    /// If accumulating, prepend (backward deletion) or append (forward
    /// deletion).
    pub prepend: bool,
    /// Merge with the most recent entry instead of creating a new one.
    pub accumulate: bool,
}

/// Ring buffer for Emacs-style kill/yank operations (upstream `KillRing`,
/// kill-ring.ts:8).
#[derive(Debug, Default)]
pub struct KillRing {
    ring: Vec<String>,
}

impl KillRing {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add text to the kill ring (kill-ring.ts:19-28).
    pub fn push(&mut self, text: String, opts: KillRingPushOptions) {
        if text.is_empty() {
            return;
        }

        if opts.accumulate && !self.ring.is_empty() {
            let last = self.ring.pop().unwrap_or_default();
            self.ring.push(if opts.prepend {
                text + &last
            } else {
                last + &text
            });
        } else {
            self.ring.push(text);
        }
    }

    /// Get most recent entry without modifying the ring (kill-ring.ts:31-33).
    pub fn peek(&self) -> Option<&str> {
        self.ring.last().map(String::as_str)
    }

    /// Move last entry to front (for yank-pop cycling, kill-ring.ts:36-41).
    pub fn rotate(&mut self) {
        if self.ring.len() > 1 {
            let last = self.ring.pop().unwrap_or_default();
            self.ring.insert(0, last);
        }
    }

    /// Number of entries in the ring (upstream `get length`,
    /// kill-ring.ts:43-45).
    pub fn len(&self) -> usize {
        self.ring.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }
}

#[cfg(test)]
mod tests {
    //! The upstream has no dedicated kill-ring test file; the behavior is
    //! exercised through `test/input.test.ts` (ported in
    //! `components/input.rs`). These unit tests pin the ring semantics
    //! directly (kill-ring.ts:19-45).

    use super::*;

    #[test]
    fn push_ignores_empty_text() {
        let mut ring = KillRing::new();
        ring.push(
            String::new(),
            KillRingPushOptions {
                prepend: false,
                accumulate: false,
            },
        );
        assert_eq!(ring.len(), 0);
    }

    #[test]
    fn push_accumulates_into_most_recent_entry() {
        let mut ring = KillRing::new();
        ring.push(
            "one".to_string(),
            KillRingPushOptions {
                prepend: false,
                accumulate: false,
            },
        );
        ring.push(
            "two".to_string(),
            KillRingPushOptions {
                prepend: false,
                accumulate: true,
            },
        );
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.peek(), Some("onetwo"));
    }

    #[test]
    fn accumulate_prepends_for_backward_deletions() {
        let mut ring = KillRing::new();
        ring.push(
            "three".to_string(),
            KillRingPushOptions {
                prepend: false,
                accumulate: false,
            },
        );
        ring.push(
            "two ".to_string(),
            KillRingPushOptions {
                prepend: true,
                accumulate: true,
            },
        );
        assert_eq!(ring.peek(), Some("two three"));
    }

    #[test]
    fn accumulate_without_existing_entries_creates_one() {
        let mut ring = KillRing::new();
        ring.push(
            "solo".to_string(),
            KillRingPushOptions {
                prepend: true,
                accumulate: true,
            },
        );
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.peek(), Some("solo"));
    }

    #[test]
    fn peek_returns_none_when_empty() {
        let ring = KillRing::new();
        assert_eq!(ring.peek(), None);
    }

    #[test]
    fn rotate_moves_last_entry_to_front() {
        let mut ring = KillRing::new();
        ring.push(
            "first".to_string(),
            KillRingPushOptions {
                prepend: false,
                accumulate: false,
            },
        );
        ring.push(
            "second".to_string(),
            KillRingPushOptions {
                prepend: false,
                accumulate: false,
            },
        );
        ring.push(
            "third".to_string(),
            KillRingPushOptions {
                prepend: false,
                accumulate: false,
            },
        );

        ring.rotate();
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.peek(), Some("second"));
    }

    #[test]
    fn rotate_does_nothing_with_one_or_zero_entries() {
        let mut ring = KillRing::new();
        ring.rotate();
        assert_eq!(ring.len(), 0);

        ring.push(
            "only".to_string(),
            KillRingPushOptions {
                prepend: false,
                accumulate: false,
            },
        );
        ring.rotate();
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.peek(), Some("only"));
    }

    #[test]
    fn rotation_cycles_through_all_entries() {
        let mut ring = KillRing::new();
        ring.push(
            "first".to_string(),
            KillRingPushOptions {
                prepend: false,
                accumulate: false,
            },
        );
        ring.push(
            "second".to_string(),
            KillRingPushOptions {
                prepend: false,
                accumulate: false,
            },
        );
        ring.push(
            "third".to_string(),
            KillRingPushOptions {
                prepend: false,
                accumulate: false,
            },
        );

        ring.rotate();
        assert_eq!(ring.peek(), Some("second"));
        ring.rotate();
        assert_eq!(ring.peek(), Some("first"));
        ring.rotate();
        assert_eq!(ring.peek(), Some("third"));
    }
}
