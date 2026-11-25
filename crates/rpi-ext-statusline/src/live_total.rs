//! Items-mode live session total overlay (V19-07 / ADR-0049 decision 8).
//!
//! `live-total-tokens` is the authoritative baseline plus every finished
//! message the baseline does not count yet plus the message in flight:
//!
//! ```text
//! sessionStats.total
//! + pending_provider_exact_output
//! + settled_local_exact_output
//! + settled_estimated_output
//! + current_provisional_output
//! ```
//!
//! It is never "the previous frame plus the new delta", and the current
//! message is never represented by two terms at once: `message_end` moves
//! the provisional count out (`current_open = false`) and settles it
//! exactly once —
//!
//! 1. Provider terminal `usage.output > 0` → [`LiveTotalState::pending_provider_exact`]
//!    (the host persists that usage; abort/error messages take this path too);
//! 2. otherwise the official full tokenizer → `settled_local_exact`
//!    (computed off the dispatch thread, see [`Settlement::AwaitingLocal`]);
//! 3. otherwise, for a message that completed normally, the final heuristic
//!    → `settled_estimated` (keeps `~`); an aborted/errored message's final
//!    estimate is dropped (v0.1.8 semantics).
//!
//! Pending entries are cleared by the snapshot's `assistant_messages`
//! WATERMARK, never by trigger category: each entry remembers
//! `pre_persist_assistant_messages + 1`, read once inside the `message_end`
//! extension callback (the host persists the message only after that
//! callback returns). A failed terminal probe leaves the watermark unknown:
//! the field is hidden until a boundary read issued after that message
//! (`turn_end`/`agent_settled`/`session_compact`) clears it best-effort.
//! Snapshots never touch the local settled sums — the host does not persist
//! them — and the current message is never changed by a snapshot.

use std::collections::VecDeque;

use crate::session_stats::AppliedRequest;

/// One finished assistant message whose exact provider output the applied
/// baseline does not count yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingProviderExact {
    /// `None`: the terminal watermark probe failed. The entry stays queued
    /// (its output is real) but `live-total-tokens` is hidden until a
    /// boundary read issued after this message clears it best-effort — the
    /// combination "probe failed + persistence failed" cannot be told apart,
    /// so the value may be lost; that risk is diagnosed, not papered over.
    pub persist_after_assistant: Option<u64>,
    pub output: u64,
    /// The request generation current when the entry landed. Only a boundary
    /// read issued AFTER it may clear an unknown watermark.
    pub request_generation_at_add: u64,
    /// Applied snapshots this entry survived while its watermark says it
    /// should have been covered (diagnosed at 3).
    pub survived_snapshots: u32,
    pub diagnosed: bool,
}

/// The raw material the refresh loop needs to settle a finished message
/// locally (copied once at `message_end`: the next `message_start` resets the
/// live buffers, and the tokenizer run must stay off the dispatch thread).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwaitingLocal {
    pub text: String,
    pub thinking: String,
    pub toolcall: String,
    /// `stopReason` is neither `aborted` nor `error` and no `errorMessage`
    /// is set: only then may a final heuristic be kept as `settled_estimated`.
    pub normal_completion: bool,
    pub message_generation: u64,
    /// `true` for a provider-exact message: the refresh loop only compares
    /// the incremental finalize with the official pass (R5.3.5) and settles
    /// nothing locally — the provider total is the value of record.
    pub diagnostic_only: bool,
}

/// What `message_end` decided for the finished message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settlement {
    /// Provider exact output landed in the pending queue; the dispatch path
    /// must run the one terminal counts-only probe now (before returning to
    /// the host).
    ProviderExact,
    /// Awaiting the official tokenizer pass on the refresh loop.
    AwaitingLocal,
    /// Nothing to settle (not an assistant message, command mode, or the
    /// `live-total-tokens` field is not selected).
    None,
}

/// The outcome of a local settlement (official full tokenizer or heuristic).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalSettlement {
    /// Official bundled tokenizer count (no `~`).
    Exact(u64),
    /// Final heuristic kept for a normally completed message (keeps `~`).
    Estimated(u64),
    /// Aborted/errored message without any exact source: the final estimate
    /// is dropped.
    Dropped,
}

/// Read-side projection for one rendered frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LiveTotalView {
    /// Σ `pending_provider_exact.output` (every entry, known or unknown).
    pub pending_output: u64,
    pub settled_local_exact: u64,
    pub settled_estimated: u64,
    /// A message is in flight or awaits its local settlement: its provisional
    /// output (carried by the frame's live view) is the `current` term.
    pub current_open: bool,
    /// An unknown-watermark entry is queued: omit `live-total-tokens`.
    pub hidden: bool,
}

/// Applied snapshots an entry may survive against its watermark before the
/// stall is diagnosed (R5.2.8: 3).
pub const STALL_DIAGNOSIS_AFTER: u32 = 3;

/// The overlay state. Pure data: every mutation is driven by the engine
/// (dispatch thread for the O(1) bookkeeping, refresh loop for settlements)
/// and the counter for `current` lives with the refresh loop.
#[derive(Debug, Default)]
pub struct LiveTotalState {
    session_generation: u64,
    message_generation: u64,
    pending_provider_exact: VecDeque<PendingProviderExact>,
    settled_local_exact: u64,
    settled_estimated: u64,
    current_open: bool,
    /// Finished messages whose local settlement the refresh loop still owes
    /// (normally one; a queue so a stalled loop never loses a message).
    awaiting_local: VecDeque<AwaitingLocal>,
}

impl LiveTotalState {
    /// session/new/resume/fork/reload: a new session generation and none of
    /// the previous session's state.
    pub fn begin_session(&mut self) -> u64 {
        self.session_generation = self.session_generation.wrapping_add(1);
        self.message_generation = 0;
        self.pending_provider_exact.clear();
        self.settled_local_exact = 0;
        self.settled_estimated = 0;
        self.current_open = false;
        self.awaiting_local.clear();
        self.session_generation
    }

    /// `message_start` (or the first delta's `begin_stream`): a new message
    /// generation; the baseline, earlier pending entries and the local
    /// settled sums are untouched.
    pub fn begin_message(&mut self) -> u64 {
        self.message_generation = self.message_generation.wrapping_add(1);
        self.current_open = true;
        self.message_generation
    }

    /// `message_end`: move the provisional out and settle it exactly once.
    /// A provider-exact total is final at once (`current_open` drops); a
    /// local settlement keeps the message CURRENT until the refresh loop has
    /// run the official tokenizer — the provisional stays in the total, so
    /// the row never dips between `message_end` and the settlement.
    pub fn on_message_end(
        &mut self,
        provider_output: Option<u64>,
        awaiting: AwaitingLocal,
        request_generation_at_add: u64,
    ) -> Settlement {
        if let Some(output) = provider_output.filter(|output| *output > 0) {
            self.current_open = false;
            self.pending_provider_exact.push_back(PendingProviderExact {
                persist_after_assistant: None,
                output,
                request_generation_at_add,
                survived_snapshots: 0,
                diagnosed: false,
            });
            // The official comparison still runs for the incremental
            // counter (diagnostic only; nothing settles locally).
            let mut awaiting = awaiting;
            awaiting.diagnostic_only = true;
            self.awaiting_local.push_back(awaiting);
            return Settlement::ProviderExact;
        }
        self.awaiting_local.push_back(awaiting);
        Settlement::AwaitingLocal
    }

    /// Record the ONE terminal counts-only probe for the entry that
    /// [`Self::on_message_end`] just queued. `Some(pre)` is
    /// `pre_persist_assistant_messages`; `None` means the probe failed and
    /// the watermark stays unknown (the field is hidden until a boundary
    /// read recovers it). Never guesses from the pending count.
    pub fn record_probe(&mut self, pre_persist_assistant: Option<u64>) {
        let Some(entry) = self.pending_provider_exact.back_mut() else {
            return;
        };
        match pre_persist_assistant {
            Some(pre) => entry.persist_after_assistant = Some(pre.saturating_add(1)),
            None => {
                tracing::warn!(
                    "rpi-statusline: terminal counts-only probe failed; live-total-tokens hidden \
                     until the next boundary snapshot (the message's exact output may be lost \
                     if the host also failed to persist it)"
                );
            }
        }
    }

    /// Take the finished message awaiting its local settlement (the refresh
    /// loop computes the official count outside the engine lock).
    pub fn take_awaiting_local(&mut self) -> Option<AwaitingLocal> {
        self.awaiting_local.pop_front()
    }

    /// Apply the local settlement computed for the last [`Self::take_awaiting_local`].
    /// `generation` is the message the settlement belongs to: the current
    /// flag only drops when that message is still the current one (a newer
    /// `message_start` that landed while the tokenizer ran keeps its own
    /// provisional in the total).
    pub fn settle_local(&mut self, settlement: LocalSettlement, generation: u64) {
        if self.awaiting_local.is_empty() && generation == self.message_generation {
            self.current_open = false;
        }
        match settlement {
            LocalSettlement::Exact(output) => {
                self.settled_local_exact = self.settled_local_exact.saturating_add(output);
            }
            LocalSettlement::Estimated(output) => {
                self.settled_estimated = self.settled_estimated.saturating_add(output);
            }
            LocalSettlement::Dropped => {}
        }
    }

    /// An applied snapshot: clear every pending entry its `assistant_messages`
    /// watermark covers. Unknown-watermark entries are cleared only by a
    /// boundary read issued after the entry landed (`turn_end` /
    /// `agent_settled` / `session_compact` — events that follow the host's
    /// persistence); a failed, stale or unapplied reply never reaches this
    /// method. Entries that survive [`STALL_DIAGNOSIS_AFTER`] applied
    /// snapshots against their own watermark are diagnosed once.
    pub fn on_snapshot_applied(&mut self, assistant_messages: u64, applied: &AppliedRequest) {
        if applied.session_generation != self.session_generation {
            return;
        }
        let boundary_after = applied.trigger.is_boundary();
        let mut recovered = false;
        self.pending_provider_exact
            .retain_mut(|entry| match entry.persist_after_assistant {
                Some(persist_after) => {
                    if persist_after <= assistant_messages {
                        return false;
                    }
                    entry.survived_snapshots = entry.survived_snapshots.saturating_add(1);
                    if entry.survived_snapshots >= STALL_DIAGNOSIS_AFTER && !entry.diagnosed {
                        entry.diagnosed = true;
                        tracing::warn!(
                            persist_after_assistant = persist_after,
                            snapshot_assistant_messages = assistant_messages,
                            survived = entry.survived_snapshots,
                            "rpi-statusline: pending exact output not covered by 3 applied \
                             snapshots; the host may have failed to persist the message"
                        );
                    }
                    true
                }
                None => {
                    let issued_after = boundary_after
                        && applied.request_generation > entry.request_generation_at_add;
                    if issued_after {
                        recovered = true;
                        return false;
                    }
                    entry.survived_snapshots = entry.survived_snapshots.saturating_add(1);
                    if entry.survived_snapshots >= STALL_DIAGNOSIS_AFTER && !entry.diagnosed {
                        entry.diagnosed = true;
                        tracing::warn!(
                            survived = entry.survived_snapshots,
                            "rpi-statusline: pending exact output with an unknown watermark \
                             survived 3 applied snapshots; waiting for a boundary read"
                        );
                    }
                    true
                }
            });
        if recovered {
            tracing::info!(
                "rpi-statusline: unknown-watermark pending entry cleared by a boundary snapshot; \
                 live-total-tokens shown again"
            );
        }
    }

    /// The frame projection.
    pub fn view(&self) -> LiveTotalView {
        LiveTotalView {
            pending_output: self
                .pending_provider_exact
                .iter()
                .fold(0u64, |sum, entry| sum.saturating_add(entry.output)),
            settled_local_exact: self.settled_local_exact,
            settled_estimated: self.settled_estimated,
            // A diagnostic-only entry (provider-exact message) never keeps
            // a message current.
            current_open: self.current_open
                || self
                    .awaiting_local
                    .iter()
                    .any(|awaiting| !awaiting.diagnostic_only),
            hidden: self
                .pending_provider_exact
                .iter()
                .any(|entry| entry.persist_after_assistant.is_none()),
        }
    }

    pub fn session_generation(&self) -> u64 {
        self.session_generation
    }

    pub fn message_generation(&self) -> u64 {
        self.message_generation
    }

    /// Whether a finished message still awaits its local settlement.
    pub fn awaiting_local(&self) -> bool {
        !self.awaiting_local.is_empty()
    }

    pub fn pending_len(&self) -> usize {
        self.pending_provider_exact.len()
    }

    pub fn pending(&self) -> &[PendingProviderExact] {
        self.pending_provider_exact.as_slices().0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_stats::StatsTrigger;

    fn applied(request_generation: u64, trigger: StatsTrigger) -> AppliedRequest {
        AppliedRequest {
            session_generation: 1,
            request_generation,
            trigger,
        }
    }

    fn awaiting(normal: bool, generation: u64) -> AwaitingLocal {
        AwaitingLocal {
            text: "t".into(),
            thinking: String::new(),
            toolcall: String::new(),
            normal_completion: normal,
            message_generation: generation,
            diagnostic_only: false,
        }
    }

    /// A usage-less message settles locally exactly once; the next
    /// `message_start` keeps the settled sum (R5.2.6/R5.2.7).
    #[test]
    fn usage_less_message_settles_once_and_survives_the_next_message() {
        let mut state = LiveTotalState::default();
        state.begin_session();
        assert_eq!(state.begin_message(), 1);
        assert!(state.view().current_open);
        assert_eq!(
            state.on_message_end(None, awaiting(true, 1), 0),
            Settlement::AwaitingLocal
        );
        let pending = state.take_awaiting_local().expect("awaiting");
        assert_eq!(pending.message_generation, 1);
        assert!(!state.awaiting_local(), "taken: nothing else queued");
        // Settle: local exact lands without `~`; the view no longer counts a
        // current message.
        state.settle_local(LocalSettlement::Exact(11), 1);
        let view = state.view();
        assert_eq!((view.settled_local_exact, view.settled_estimated), (11, 0));
        assert!(!view.current_open);
        assert_eq!(state.begin_message(), 2);
        assert_eq!(
            state.view().settled_local_exact,
            11,
            "not deleted by message_start"
        );
        state.on_message_end(None, awaiting(true, 2), 0);
        state.take_awaiting_local();
        state.settle_local(LocalSettlement::Estimated(5), 2);
        let view = state.view();
        assert_eq!((view.settled_local_exact, view.settled_estimated), (11, 5));
        // An aborted message without any exact source drops its estimate.
        state.begin_message();
        state.on_message_end(None, awaiting(false, 3), 0);
        state.take_awaiting_local();
        state.settle_local(LocalSettlement::Dropped, 3);
        assert_eq!(state.view().settled_estimated, 5);
        // But its official tokenizer result still settles as exact.
        state.begin_message();
        state.on_message_end(None, awaiting(false, 4), 0);
        state.take_awaiting_local();
        state.settle_local(LocalSettlement::Exact(7), 4);
        assert_eq!(state.view().settled_local_exact, 18);
        // A new session clears every generation.
        state.begin_session();
        assert_eq!(state.view(), LiveTotalView::default());
        assert_eq!(state.message_generation(), 0);
    }

    /// Provider exact totals queue with the probed watermark and are cleared
    /// by `persist_after <= assistant_messages` — never by trigger category.
    #[test]
    fn provider_exact_pending_is_cleared_by_the_watermark() {
        let mut state = LiveTotalState::default();
        state.begin_session();
        // Message 1 ends with usage 40; the probe reads 3 persisted
        // assistant messages → covered by the snapshot at 4.
        state.begin_message();
        assert_eq!(
            state.on_message_end(Some(40), awaiting(true, 1), 0),
            Settlement::ProviderExact
        );
        state.record_probe(Some(3));
        // Message 2 ends with usage 7 before any snapshot.
        state.begin_message();
        state.on_message_end(Some(7), awaiting(true, 2), 0);
        state.record_probe(Some(3));
        let view = state.view();
        assert_eq!(view.pending_output, 47);
        assert!(!view.hidden);
        // A snapshot that already ran BEFORE the persistence (3) covers
        // nothing — the bidirectional race "message_end → persist → interval
        // already contains usage must not double count" is the other side:
        // once the snapshot counts the message the pending part is gone.
        state.on_snapshot_applied(3, &applied(1, StatsTrigger::Interval));
        assert_eq!(state.view().pending_output, 47);
        state.on_snapshot_applied(4, &applied(2, StatsTrigger::Interval));
        let view = state.view();
        assert_eq!(view.pending_output, 0, "both entries persist after 4");
        // Partial coverage: only the entries whose watermark is reached.
        state.begin_message();
        state.on_message_end(Some(1), awaiting(true, 3), 0);
        state.record_probe(Some(4));
        state.begin_message();
        state.on_message_end(Some(2), awaiting(true, 4), 0);
        state.record_probe(Some(5));
        state.on_snapshot_applied(5, &applied(3, StatsTrigger::TurnEnd));
        assert_eq!(state.view().pending_output, 2);
        state.on_snapshot_applied(6, &applied(4, StatsTrigger::AgentSettled));
        assert_eq!(state.view().pending_output, 0);
        // 0/missing usage never queues an exact 0.
        state.begin_message();
        assert_eq!(
            state.on_message_end(Some(0), awaiting(true, 5), 0),
            Settlement::AwaitingLocal
        );
    }

    /// A failed terminal probe hides the field and is recovered only by a
    /// boundary read issued after the message; a read issued before it (even
    /// a boundary one) and non-boundary reads leave it hidden.
    #[test]
    fn unknown_watermark_hides_until_a_later_boundary_read() {
        let mut state = LiveTotalState::default();
        state.begin_session();
        // Read #1 is in flight when the message ends.
        let request_generation_at_add = 1;
        state.begin_message();
        state.on_message_end(Some(9), awaiting(true, 1), request_generation_at_add);
        state.record_probe(None);
        let view = state.view();
        assert!(view.hidden, "unknown watermark hides live-total-tokens");
        assert_eq!(view.pending_output, 9);
        // The in-flight read applies (non-boundary, issued before): nothing.
        state.on_snapshot_applied(10, &applied(1, StatsTrigger::Interval));
        assert!(state.view().hidden);
        // A boundary read that was ALSO issued before the message: nothing.
        state.on_snapshot_applied(10, &applied(1, StatsTrigger::TurnEnd));
        assert!(state.view().hidden);
        // A non-boundary read issued after: nothing.
        state.on_snapshot_applied(10, &applied(5, StatsTrigger::ConfigSaved));
        assert!(state.view().hidden);
        // A boundary read issued after the message: best-effort recovery.
        state.on_snapshot_applied(10, &applied(6, StatsTrigger::AgentSettled));
        let view = state.view();
        assert!(!view.hidden);
        assert_eq!(view.pending_output, 0);
    }

    /// An entry that survives three applied snapshots against its own
    /// watermark is diagnosed (stall) — and stays queued (its output is real).
    #[test]
    fn stalled_entries_survive_and_are_diagnosed_once() {
        let mut state = LiveTotalState::default();
        state.begin_session();
        state.begin_message();
        state.on_message_end(Some(5), awaiting(true, 1), 0);
        state.record_probe(Some(2));
        let mut generation = 0;
        for _ in 0..STALL_DIAGNOSIS_AFTER {
            generation += 1;
            state.on_snapshot_applied(2, &applied(generation, StatsTrigger::Interval));
        }
        let view = state.view();
        assert_eq!(view.pending_output, 5);
        assert!(state.pending()[0].diagnosed);
        assert_eq!(state.pending()[0].survived_snapshots, STALL_DIAGNOSIS_AFTER);
        // A snapshot from a previous session generation clears nothing.
        state.on_snapshot_applied(
            9,
            &AppliedRequest {
                session_generation: 0,
                request_generation: 99,
                trigger: StatsTrigger::TurnEnd,
            },
        );
        assert_eq!(state.view().pending_output, 5);
        state.on_snapshot_applied(3, &applied(99, StatsTrigger::TurnEnd));
        assert_eq!(state.view().pending_output, 0);
    }

    /// V19-07 review P2-1: a settlement that lands after the NEXT message
    /// started (the tokenizer ran outside the engine lock) must not drop the
    /// new message's provisional from the total.
    #[test]
    fn late_settlement_keeps_the_next_message_current() {
        let mut state = LiveTotalState::default();
        state.begin_session();
        state.begin_message();
        state.on_message_end(None, awaiting(true, 1), 0);
        let taken = state.take_awaiting_local().expect("awaiting");
        assert_eq!(taken.message_generation, 1);
        // The next message starts before the settlement lands.
        state.begin_message();
        assert!(state.view().current_open);
        state.settle_local(LocalSettlement::Exact(5), taken.message_generation);
        let view = state.view();
        assert_eq!(view.settled_local_exact, 5, "the old message still settles");
        assert!(view.current_open, "the new message stays current");
        // Settling the current message itself drops the flag.
        state.on_message_end(None, awaiting(true, 2), 0);
        state.take_awaiting_local();
        state.settle_local(LocalSettlement::Exact(3), 2);
        assert!(!state.view().current_open);
        assert_eq!(state.view().settled_local_exact, 8);
    }

    /// Provider-exact messages queue a diagnostic-only settlement.
    #[test]
    fn provider_exact_messages_queue_a_diagnostic_only_settlement() {
        let mut state = LiveTotalState::default();
        state.begin_session();
        state.begin_message();
        assert_eq!(
            state.on_message_end(Some(9), awaiting(true, 1), 0),
            Settlement::ProviderExact
        );
        assert!(state.awaiting_local());
        let taken = state.take_awaiting_local().expect("diagnostic");
        assert!(taken.diagnostic_only);
        assert!(!state.view().current_open);
    }

    #[test]
    fn stats_trigger_classification() {
        for reason in [
            "session_start",
            "session_compact",
            "turn_end",
            "agent_settled",
            "interval",
            "config_saved",
        ] {
            assert!(StatsTrigger::for_reason(reason).is_some(), "{reason}");
        }
        assert!(StatsTrigger::for_reason("message_end").is_none());
        assert!(StatsTrigger::TurnEnd.is_boundary());
        assert!(StatsTrigger::AgentSettled.is_boundary());
        assert!(StatsTrigger::SessionCompact.is_boundary());
        assert!(!StatsTrigger::Interval.is_boundary());
        assert!(!StatsTrigger::ConfigSaved.is_boundary());
        assert!(!StatsTrigger::SessionStart.is_boundary());
    }
}
