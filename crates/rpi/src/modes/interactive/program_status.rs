//! Port of `packages/coding-agent/src/modes/interactive/program-status-reporter.ts`
//! @ pi 1.1.0 (abe508e1b).
//!
//! Reports interactive-mode state to the terminal (OSC 7501): `working`
//! during agent runs and compaction, `blocked` while a dialog waits for the
//! user, then `done`, `error`, or `idle` once the run settles. Messages are
//! limited to the session name, dialog titles, and the first line of errors;
//! prompts and assistant output are never reported.
//!
//! The interactive layer is the single status authority (02 §1.3 裁决 3):
//! `rpi-tui` only carries the OSC 7501 probe and emission
//! (`rpi_tui::program_status`).
//!
//! Intentional differences: none. The upstream reporter consumes
//! `AgentSessionEvent`s directly; this port exposes one method per event the
//! interactive drain already handles (`UiCommand` arms), and dedups by
//! structural equality instead of `JSON.stringify`.

use rpi_ai::types::{AssistantMessage, StopReason};
use rpi_tui::program_status::{ProgramStatus, ProgramStatusKind, ProgramStatusState};

use crate::config::APP_NAME;
use crate::core::compaction_runner::CompactionReason;

/// `BlockedStatus` (program-status-reporter.ts:5-9): what a blocked program
/// waits for plus the one-line dialog title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedStatus {
    pub kind: ProgramStatusKind,
    pub message: String,
}

/// `firstLine` (program-status-reporter.ts:11-13): the first line of an
/// error, or `"Error"` when empty. Never the rest of the assistant output.
fn first_line(text: Option<&str>) -> String {
    let line = text
        .and_then(|text| text.split('\n').next())
        .map(str::trim)
        .unwrap_or("");
    if line.is_empty() {
        "Error".to_owned()
    } else {
        line.to_owned()
    }
}

/// `ProgramStatusReporter` (program-status-reporter.ts:19-111).
#[derive(Debug)]
pub struct ProgramStatusReporter {
    run_active: bool,
    compacting: bool,
    /// Outcome of the current run, reported once it settles.
    run_result: ProgramStatus,
    /// Status while no run is active.
    resting_status: ProgramStatus,
    /// Open dialogs by source, in the order they opened. The most recent one
    /// is reported.
    blocked: Vec<(String, BlockedStatus)>,
    /// The last status sent to the terminal (upstream `lastReport`).
    last_report: Option<ProgramStatus>,
}

impl Default for ProgramStatusReporter {
    fn default() -> Self {
        Self::new()
    }
}

impl ProgramStatusReporter {
    pub fn new() -> Self {
        ProgramStatusReporter {
            run_active: false,
            compacting: false,
            run_result: ProgramStatus::done(),
            resting_status: ProgramStatus::idle(),
            blocked: Vec::new(),
            last_report: None,
        }
    }

    /// `agent_start` (program-status-reporter.ts:36-39).
    pub fn handle_agent_start(&mut self) {
        self.run_active = true;
        self.run_result = ProgramStatus::done();
    }

    /// `message_end` (program-status-reporter.ts:40-46): the latest response
    /// decides the outcome, so a retried error is replaced by its successful
    /// retry.
    pub fn handle_message_end(&mut self, message: &AssistantMessage) {
        self.run_result = if message.stop_reason == StopReason::Error {
            ProgramStatus::error(first_line(message.error_message.as_deref()))
        } else {
            ProgramStatus::done()
        };
    }

    /// `compaction_start` (program-status-reporter.ts:47-49).
    pub fn handle_compaction_start(&mut self) {
        self.compacting = true;
    }

    /// `compaction_end` (program-status-reporter.ts:50-65).
    pub fn handle_compaction_end(
        &mut self,
        reason: CompactionReason,
        aborted: bool,
        error_message: Option<&str>,
    ) {
        self.compacting = false;
        if self.run_active {
            // A failed recovery compaction ends the run unless a later
            // response succeeds.
            if aborted {
                self.run_result = ProgramStatus::idle();
            } else if let Some(error_message) = error_message {
                self.run_result = ProgramStatus::error(first_line(Some(error_message)));
            }
        } else if aborted {
            self.resting_status = ProgramStatus::idle();
        } else if reason == CompactionReason::Manual {
            self.resting_status = match error_message {
                Some(error_message) => ProgramStatus::error(first_line(Some(error_message))),
                None => ProgramStatus::done(),
            };
        }
    }

    /// `agent_settled` (program-status-reporter.ts:66-69): a cancelled run
    /// reports idle.
    pub fn handle_agent_settled(&mut self, aborted: bool) {
        self.run_active = false;
        self.resting_status = if aborted {
            ProgramStatus::idle()
        } else {
            self.run_result.clone()
        };
    }

    /// `session_info_changed` (program-status-reporter.ts:70-72) is handled
    /// by the caller re-reporting: the session name is part of working and
    /// done reports and `report()` dedups by content.

    /// Report `blocked` for a dialog until it is cleared with `None`.
    /// Reopening a source replaces it (program-status-reporter.ts:88-92).
    pub fn set_blocked(&mut self, source: &str, status: Option<BlockedStatus>) {
        self.blocked.retain(|(key, _)| key != source);
        if let Some(status) = status {
            self.blocked.push((source.to_owned(), status));
        }
    }

    /// Forget the previous session's run, for example after switching
    /// sessions (program-status-reporter.ts:95-101).
    pub fn reset(&mut self) {
        self.run_active = false;
        self.compacting = false;
        self.run_result = ProgramStatus::done();
        self.resting_status = ProgramStatus::idle();
    }

    /// `report()` (program-status-reporter.ts:103-109): returns the status to
    /// send, or `None` when it equals the last one sent. The caller forwards
    /// a `Some` to [`rpi_tui::terminal::Terminal::set_program_status`].
    pub fn report(&mut self, session_name: Option<&str>) -> Option<ProgramStatus> {
        let mut status = self.current_status(session_name);
        status.app = Some(APP_NAME.to_owned());
        if self.last_report.as_ref() == Some(&status) {
            return None;
        }
        self.last_report = Some(status.clone());
        Some(status)
    }

    /// `currentStatus()` (program-status-reporter.ts:111-138).
    fn current_status(&self, session_name: Option<&str>) -> ProgramStatus {
        if let Some((_, blocked)) = self.blocked.last() {
            return ProgramStatus::blocked(blocked.kind, blocked.message.clone());
        }
        if self.compacting {
            return ProgramStatus::working(Some("Compacting context".to_owned()));
        }
        let status = if self.run_active {
            ProgramStatus::working(None)
        } else {
            self.resting_status.clone()
        };
        if matches!(
            status.state,
            ProgramStatusState::Working | ProgramStatusState::Done
        ) {
            let mut status = status;
            status.message = session_name.map(str::to_owned);
            return status;
        }
        status
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_ai::types::{
        AssistantContent, AssistantMessage, AssistantRole, StopReason, TextContent, Usage,
    };

    fn assistant_end(stop_reason: StopReason, error_message: Option<&str>) -> AssistantMessage {
        AssistantMessage {
            role: AssistantRole::Assistant,
            content: vec![AssistantContent::Text(TextContent {
                text: "secret assistant output".into(),
                text_signature: None,
            })],
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            model: "m".into(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            thinking_level: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason,
            error_message: error_message.map(str::to_owned),
            timestamp: 0,
            deferred: None,
            end_turn: None,
            raw_stop_reason: None,
            duration_ms: None,
        }
    }

    struct Harness {
        reporter: ProgramStatusReporter,
        reports: Vec<ProgramStatus>,
        session_name: Option<String>,
    }

    impl Harness {
        fn new(session_name: Option<&str>) -> Self {
            Harness {
                reporter: ProgramStatusReporter::new(),
                reports: Vec::new(),
                session_name: session_name.map(str::to_owned),
            }
        }

        fn report(&mut self) {
            if let Some(status) = self.reporter.report(self.session_name.as_deref()) {
                self.reports.push(status);
            }
        }

        fn send_message_end(&mut self, message: &AssistantMessage) {
            self.reporter.handle_message_end(message);
            self.report();
        }

        fn send_settled(&mut self, aborted: bool) {
            self.reporter.handle_agent_settled(aborted);
            self.report();
        }

        fn last(&self) -> &ProgramStatus {
            self.reports.last().expect("at least one report")
        }
    }

    // program-status-reporter.test.ts: "reports idle, working during a run, and done once it settles"
    #[test]
    fn reports_idle_working_during_a_run_and_done_once_it_settles() {
        let mut harness = Harness::new(Some("Fix login"));
        harness.report();
        assert_eq!(harness.last().state, ProgramStatusState::Idle);
        assert_eq!(harness.last().app.as_deref(), Some(APP_NAME));

        harness.reporter.handle_agent_start();
        harness.report();
        assert_eq!(harness.last().state, ProgramStatusState::Working);
        assert_eq!(harness.last().message.as_deref(), Some("Fix login"));

        harness.send_message_end(&assistant_end(StopReason::ToolUse, None));
        harness.send_message_end(&assistant_end(StopReason::Stop, None));
        assert_eq!(harness.last().state, ProgramStatusState::Working);
        assert_eq!(harness.last().message.as_deref(), Some("Fix login"));

        harness.send_settled(false);
        assert_eq!(harness.last().state, ProgramStatusState::Done);
        assert_eq!(harness.last().message.as_deref(), Some("Fix login"));
        assert!(!format!("{:?}", harness.reports).contains("secret assistant output"));
    }

    // program-status-reporter.test.ts: "reports only the outcome of the run: retried errors, final errors, and aborts"
    #[test]
    fn reports_only_the_outcome_of_the_run() {
        let mut harness = Harness::new(None);
        harness.reporter.handle_agent_start();
        harness.send_message_end(&assistant_end(StopReason::Error, Some("overloaded")));
        harness.send_message_end(&assistant_end(StopReason::Stop, None));
        harness.send_settled(false);
        assert_eq!(harness.last().state, ProgramStatusState::Done);

        harness.reporter.handle_agent_start();
        harness.send_message_end(&assistant_end(
            StopReason::Error,
            Some("Invalid API key\n{details}"),
        ));
        harness.send_settled(false);
        assert_eq!(harness.last().state, ProgramStatusState::Error);
        assert_eq!(harness.last().message.as_deref(), Some("Invalid API key"));

        harness.reporter.handle_agent_start();
        harness.send_message_end(&assistant_end(StopReason::Aborted, None));
        harness.send_settled(true);
        assert_eq!(harness.last().state, ProgramStatusState::Idle);

        // Aborted after a successful response, for example in an
        // agent_before_settle hook or a retry delay.
        harness.reporter.handle_agent_start();
        harness.send_message_end(&assistant_end(StopReason::Stop, None));
        harness.send_settled(true);
        assert_eq!(harness.last().state, ProgramStatusState::Idle);
    }

    // program-status-reporter.test.ts: "reports a failed recovery compaction as the run's error unless a later response succeeds"
    #[test]
    fn reports_a_failed_recovery_compaction_as_the_runs_error() {
        let mut harness = Harness::new(None);
        harness.reporter.handle_agent_start();
        harness.send_message_end(&assistant_end(StopReason::Length, None));
        harness.reporter.handle_compaction_start();
        harness.report();
        harness.reporter.handle_compaction_end(
            CompactionReason::Overflow,
            false,
            Some("Compaction failed\nstack"),
        );
        harness.report();
        harness.send_settled(false);
        assert_eq!(harness.last().state, ProgramStatusState::Error);
        assert_eq!(harness.last().message.as_deref(), Some("Compaction failed"));

        harness.reporter.handle_agent_start();
        harness.reporter.handle_compaction_start();
        harness.reporter.handle_compaction_end(
            CompactionReason::Threshold,
            false,
            Some("Compaction failed"),
        );
        harness.send_message_end(&assistant_end(StopReason::Stop, None));
        harness.send_settled(false);
        assert_eq!(harness.last().state, ProgramStatusState::Done);
    }

    // program-status-reporter.test.ts: "reports compaction inside a run and the result of a manual compaction"
    #[test]
    fn reports_compaction_inside_a_run_and_the_result_of_a_manual_compaction() {
        let mut harness = Harness::new(Some("Session"));
        harness.reporter.handle_agent_start();
        harness.reporter.handle_compaction_start();
        harness.report();
        assert_eq!(harness.last().state, ProgramStatusState::Working);
        assert_eq!(harness.last().message.as_deref(), Some("Compacting context"));
        harness.reporter.handle_compaction_end(CompactionReason::Threshold, false, None);
        harness.report();
        assert_eq!(harness.last().state, ProgramStatusState::Working);
        assert_eq!(harness.last().message.as_deref(), Some("Session"));
        harness.send_message_end(&assistant_end(StopReason::Stop, None));
        harness.send_settled(false);

        harness.reporter.handle_compaction_start();
        harness.reporter.handle_compaction_end(CompactionReason::Manual, false, None);
        harness.report();
        assert_eq!(harness.last().state, ProgramStatusState::Done);
        assert_eq!(harness.last().message.as_deref(), Some("Session"));

        harness.reporter.handle_compaction_start();
        harness.reporter.handle_compaction_end(
            CompactionReason::Manual,
            false,
            Some("No model"),
        );
        harness.report();
        assert_eq!(harness.last().state, ProgramStatusState::Error);
        assert_eq!(harness.last().message.as_deref(), Some("No model"));

        harness.reporter.handle_compaction_start();
        harness.reporter.handle_compaction_end(CompactionReason::Manual, true, None);
        harness.report();
        assert_eq!(harness.last().state, ProgramStatusState::Idle);
    }

    // program-status-reporter.test.ts: "reports the most recent open dialog and the underlying state once all close"
    #[test]
    fn reports_the_most_recent_open_dialog_and_the_underlying_state_once_all_close() {
        let mut harness = Harness::new(None);
        harness.reporter.handle_agent_start();
        harness.reporter.set_blocked(
            "extension-selector",
            Some(BlockedStatus {
                kind: ProgramStatusKind::Permission,
                message: "Allow bash?".into(),
            }),
        );
        harness.report();
        harness.reporter.set_blocked(
            "login",
            Some(BlockedStatus {
                kind: ProgramStatusKind::Auth,
                message: "Log in to Anthropic".into(),
            }),
        );
        harness.report();
        assert_eq!(harness.last().state, ProgramStatusState::Blocked);
        assert_eq!(harness.last().kind, Some(ProgramStatusKind::Auth));
        assert_eq!(harness.last().message.as_deref(), Some("Log in to Anthropic"));

        // The run settles while the selector is still open.
        harness.reporter.set_blocked("login", None);
        harness.report();
        harness.send_message_end(&assistant_end(StopReason::Stop, None));
        harness.send_settled(false);
        assert_eq!(harness.last().state, ProgramStatusState::Blocked);
        assert_eq!(harness.last().kind, Some(ProgramStatusKind::Permission));
        assert_eq!(harness.last().message.as_deref(), Some("Allow bash?"));

        // Reopening a source replaces its dialog instead of stacking a
        // second one.
        harness.reporter.set_blocked(
            "extension-selector",
            Some(BlockedStatus {
                kind: ProgramStatusKind::Question,
                message: "Pick one".into(),
            }),
        );
        harness.report();
        assert_eq!(harness.last().kind, Some(ProgramStatusKind::Question));
        assert_eq!(harness.last().message.as_deref(), Some("Pick one"));
        harness.reporter.set_blocked("extension-selector", None);
        harness.report();
        assert_eq!(harness.last().state, ProgramStatusState::Done);
    }

    // program-status-reporter.test.ts: "sends each status once and follows session name changes"
    #[test]
    fn sends_each_status_once_and_follows_session_name_changes() {
        let mut harness = Harness::new(Some("Old"));
        harness.reporter.handle_agent_start();
        harness.send_message_end(&assistant_end(StopReason::ToolUse, None));
        harness.report();
        assert_eq!(harness.reports.len(), 1);

        harness.session_name = Some("New".to_owned());
        // `session_info_changed` (program-status-reporter.ts:70-72): the
        // caller re-reports; the session name is part of the report.
        harness.report();
        assert_eq!(harness.last().state, ProgramStatusState::Working);
        assert_eq!(harness.last().message.as_deref(), Some("New"));
    }

    // program-status-reporter.test.ts: "returns to idle when the session is replaced"
    #[test]
    fn returns_to_idle_when_the_session_is_replaced() {
        let mut harness = Harness::new(None);
        harness.reporter.handle_agent_start();
        harness.send_message_end(&assistant_end(StopReason::Stop, None));
        harness.send_settled(false);
        harness.reporter.reset();
        harness.report();
        assert_eq!(harness.last().state, ProgramStatusState::Idle);
    }
}