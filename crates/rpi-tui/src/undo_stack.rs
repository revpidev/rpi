//! Port of `packages/tui/src/undo-stack.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - `structuredClone(state)` becomes `state.clone()` with a `S: Clone`
//!   bound (the upstream TS states are plain value objects).
//! - `len()`/`is_empty()` replace the `get length` getter.
//!
//! Generic undo stack with clone-on-push semantics. Stores deep clones of
//! state snapshots; popped snapshots are returned directly (no re-cloning)
//! since they are already detached.

/// Generic undo stack with clone-on-push semantics (upstream `UndoStack`,
/// undo-stack.ts:7).
pub struct UndoStack<S> {
    stack: Vec<S>,
}

impl<S> Default for UndoStack<S> {
    fn default() -> Self {
        Self { stack: Vec::new() }
    }
}

impl<S: Clone> UndoStack<S> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a clone of the given state onto the stack (undo-stack.ts:11-13).
    pub fn push(&mut self, state: S) {
        self.stack.push(state.clone());
    }

    /// Pop and return the most recent snapshot, or `None` if empty
    /// (undo-stack.ts:16-18).
    pub fn pop(&mut self) -> Option<S> {
        self.stack.pop()
    }

    /// Remove all snapshots (undo-stack.ts:21-23).
    pub fn clear(&mut self) {
        self.stack.clear();
    }

    /// Number of snapshots on the stack (upstream `get length`,
    /// undo-stack.ts:25-27).
    pub fn len(&self) -> usize {
        self.stack.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }
}

#[cfg(test)]
mod tests {
    //! The upstream has no dedicated undo-stack test file; the behavior is
    //! exercised through `test/input.test.ts` (ported in
    //! `components/input.rs`). These unit tests pin the stack semantics
    //! directly (undo-stack.ts:11-27).

    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct State {
        value: String,
        cursor: usize,
    }

    #[test]
    fn push_stores_a_clone() {
        let mut stack = UndoStack::new();
        let mut state = State {
            value: "hello".to_string(),
            cursor: 5,
        };
        stack.push(state.clone());

        // Mutating the original after push must not affect the snapshot.
        state.value = "mutated".to_string();
        assert_eq!(
            stack.pop(),
            Some(State {
                value: "hello".to_string(),
                cursor: 5
            })
        );
    }

    #[test]
    fn pop_returns_most_recent_snapshot() {
        let mut stack = UndoStack::new();
        stack.push(State {
            value: "first".to_string(),
            cursor: 0,
        });
        stack.push(State {
            value: "second".to_string(),
            cursor: 6,
        });

        assert_eq!(
            stack.pop(),
            Some(State {
                value: "second".to_string(),
                cursor: 6
            })
        );
        assert_eq!(
            stack.pop(),
            Some(State {
                value: "first".to_string(),
                cursor: 0
            })
        );
        assert_eq!(stack.pop(), None);
    }

    #[test]
    fn clear_removes_all_snapshots() {
        let mut stack = UndoStack::new();
        stack.push(State {
            value: "a".to_string(),
            cursor: 1,
        });
        stack.push(State {
            value: "b".to_string(),
            cursor: 2,
        });
        assert_eq!(stack.len(), 2);

        stack.clear();
        assert_eq!(stack.len(), 0);
        assert_eq!(stack.pop(), None);
    }

    #[test]
    fn len_counts_snapshots() {
        let mut stack = UndoStack::new();
        assert_eq!(stack.len(), 0);
        stack.push(State {
            value: "a".to_string(),
            cursor: 1,
        });
        assert_eq!(stack.len(), 1);
        stack.pop();
        assert_eq!(stack.len(), 0);
    }
}
