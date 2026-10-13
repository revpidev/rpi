//! Port of `packages/tui/src/program-status.ts` @ pi 1.1.0 (abe508e1b).
//!
//! Program Status Protocol (OSC 7501): a program tells the terminal whether
//! it is idle, working, blocked on the user, done, or failed. Only the root
//! record is supported.
//!
//! Spec: <https://www.superlogical.com/rex/docs/build/program-status>
//!
//! Intentional differences: none. `Buffer.byteLength` becomes
//! [`str::len`] and JS `for...of` code-point iteration becomes
//! [`char::len_utf8`]; the control-character replacement collapses each run
//! to one space exactly like the upstream `/[...]+/g` regex.

use base64::Engine as _;

/// `ProgramStatusState` (program-status.ts:8-10 states plus `clear`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramStatusState {
    Idle,
    Working,
    Blocked,
    Done,
    /// Upstream spells the failed state `error` (program-status.ts:9).
    Error,
    /// `clear` removes the status instead of reporting one.
    Clear,
}

impl ProgramStatusState {
    /// The wire token (`ProgramStatus["state"]`).
    pub fn as_str(self) -> &'static str {
        match self {
            ProgramStatusState::Idle => "idle",
            ProgramStatusState::Working => "working",
            ProgramStatusState::Blocked => "blocked",
            ProgramStatusState::Done => "done",
            ProgramStatusState::Error => "error",
            ProgramStatusState::Clear => "clear",
        }
    }
}

/// `ProgramStatus["kind"]` (program-status.ts:12): what a blocked program
/// waits for. Ignored outside the `blocked` state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramStatusKind {
    Permission,
    Question,
    Auth,
}

impl ProgramStatusKind {
    fn as_str(self) -> &'static str {
        match self {
            ProgramStatusKind::Permission => "permission",
            ProgramStatusKind::Question => "question",
            ProgramStatusKind::Auth => "auth",
        }
    }
}

/// `ProgramStatus` (program-status.ts:8-17). `app` is omitted on the wire
/// unless [`is_valid_app_name`] accepts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramStatus {
    pub state: ProgramStatusState,
    /// Stable program name, `[A-Za-z0-9_.+-]{1,32}`. Other values are
    /// omitted.
    pub app: Option<String>,
    /// What a blocked program waits for. Omitted for other states.
    pub kind: Option<ProgramStatusKind>,
    /// One human-readable line. Control characters become spaces; longer
    /// text is cut to the spec limit.
    pub message: Option<String>,
}

impl ProgramStatus {
    /// `{ state: "idle" }` — the resting status before any run.
    pub fn idle() -> Self {
        ProgramStatus {
            state: ProgramStatusState::Idle,
            app: None,
            kind: None,
            message: None,
        }
    }

    /// `{ state: "done" }` — the default outcome of a run.
    pub fn done() -> Self {
        ProgramStatus {
            state: ProgramStatusState::Done,
            app: None,
            kind: None,
            message: None,
        }
    }

    /// `{ state: "working" }` with an optional message (session name /
    /// `"Compacting context"`).
    pub fn working(message: Option<String>) -> Self {
        ProgramStatus {
            state: ProgramStatusState::Working,
            app: None,
            kind: None,
            message,
        }
    }

    /// `{ state: "error"; message }`.
    pub fn error(message: String) -> Self {
        ProgramStatus {
            state: ProgramStatusState::Error,
            app: None,
            kind: None,
            message: Some(message),
        }
    }

    /// `{ state: "blocked"; kind; message }`.
    pub fn blocked(kind: ProgramStatusKind, message: String) -> Self {
        ProgramStatus {
            state: ProgramStatusState::Blocked,
            app: None,
            kind: Some(kind),
            message: Some(message),
        }
    }

    /// `{ state: "clear" }` — removes the status on the terminal.
    pub fn clear() -> Self {
        ProgramStatus {
            state: ProgramStatusState::Clear,
            app: None,
            kind: None,
            message: None,
        }
    }
}

/// Feature detection query. A supporting terminal replies with the same
/// body (program-status.ts:19).
pub const PROGRAM_STATUS_QUERY: &str = "\x1b]7501;?\x1b\\";

/// The reply to [`PROGRAM_STATUS_QUERY`]. Later spec revisions may add pairs
/// after the `?` (program-status.ts:22-26).
pub fn is_program_status_reply(sequence: &str) -> bool {
    let Some(body) = sequence.strip_prefix("\x1b]7501;?") else {
        return false;
    };
    let middle = body
        .strip_suffix("\x1b\\")
        .or_else(|| body.strip_suffix('\x07'));
    matches!(middle, Some(middle) if !middle.contains(['\x07', '\x1b']))
}

/// `APP_PATTERN` (program-status.ts:27).
fn is_valid_app_name(app: &str) -> bool {
    (1..=32).contains(&app.len())
        && app
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-'))
}

/// `CONTROL_CHARACTERS` (program-status.ts:28): C0 and C1 controls.
fn is_control_character(c: char) -> bool {
    matches!(c, '\u{0000}'..='\u{001f}' | '\u{007f}'..='\u{009f}')
}

/// JS `String.prototype.trim` whitespace: the `WhiteSpace` and
/// `LineTerminator` sets (`\uFEFF` is trimmed by JS but is not
/// `char::is_whitespace` in Rust).
fn is_js_trim_whitespace(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

/// `/[\u0000-\u001f\u007f-\u009f]+/g` with a `" "` replacement: every run of
/// control characters collapses to one space (program-status.ts:47).
fn replace_control_character_runs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_run = false;
    for c in text.chars() {
        if is_control_character(c) {
            if !in_run {
                out.push(' ');
                in_run = true;
            }
        } else {
            out.push(c);
            in_run = false;
        }
    }
    out
}

/// JS `String.prototype.trim`.
fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_trim_whitespace)
}

/// `truncateUtf8` (program-status.ts:32-41): cut at a UTF-8 boundary whose
/// byte length stays within `max_bytes`.
fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut bytes = 0;
    let mut end = 0;
    for c in text.chars() {
        let size = c.len_utf8();
        if bytes + size > max_bytes {
            break;
        }
        bytes += size;
        end += size;
    }
    &text[..end]
}

/// Decoded `msg` limit (program-status.ts:30): its base64 encoding stays
/// under the 2732-byte encoded limit.
const MAX_MESSAGE_BYTES: usize = 2048;

/// `formatProgramStatus` (program-status.ts:46-53). Terminals discard
/// reports whose text contains control characters, so they are replaced.
pub fn format_program_status(status: &ProgramStatus) -> String {
    let mut pairs = vec![format!("state={}", status.state.as_str())];
    if let Some(app) = &status.app
        && is_valid_app_name(app)
    {
        pairs.push(format!("app={app}"));
    }
    if status.state == ProgramStatusState::Blocked
        && let Some(kind) = status.kind
    {
        pairs.push(format!("kind={}", kind.as_str()));
    }
    let replaced = replace_control_character_runs(status.message.as_deref().unwrap_or(""));
    let message = truncate_utf8(js_trim(&replaced), MAX_MESSAGE_BYTES);
    if !message.is_empty() {
        pairs.push(format!(
            "msg={}",
            base64::engine::general_purpose::STANDARD.encode(message.as_bytes())
        ));
    }
    format!("\x1b]7501;{}\x1b\\", pairs.join(":"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_message(sequence: &str) -> Option<String> {
        let message = sequence
            .split(":msg=")
            .nth(1)
            .and_then(|rest| rest.strip_suffix("\x1b\\"))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(message)
            .expect("base64");
        Some(String::from_utf8(bytes).expect("utf8"))
    }

    // program-status.test.ts: "encodes state, app, kind, and a base64 message"
    #[test]
    fn encodes_state_app_kind_and_a_base64_message() {
        assert_eq!(
            format_program_status(&ProgramStatus {
                state: ProgramStatusState::Blocked,
                app: Some("pi".into()),
                kind: Some(ProgramStatusKind::Permission),
                message: Some("Allow bash?".into()),
            }),
            format!(
                "\x1b]7501;state=blocked:app=pi:kind=permission:msg={}\x1b\\",
                base64::engine::general_purpose::STANDARD.encode("Allow bash?")
            )
        );
        assert_eq!(
            format_program_status(&ProgramStatus::clear()),
            "\x1b]7501;state=clear\x1b\\"
        );
    }

    // program-status.test.ts: "omits kind outside blocked, invalid app names, and empty messages"
    #[test]
    fn omits_kind_outside_blocked_invalid_app_names_and_empty_messages() {
        assert_eq!(
            format_program_status(&ProgramStatus {
                state: ProgramStatusState::Working,
                app: Some("my app".into()),
                kind: Some(ProgramStatusKind::Auth),
                message: Some(" \n ".into()),
            }),
            "\x1b]7501;state=working\x1b\\"
        );
        assert!(
            format_program_status(&ProgramStatus {
                state: ProgramStatusState::Idle,
                app: Some("a".repeat(32)),
                kind: None,
                message: None,
            })
            .contains(&format!(":app={}\x1b", "a".repeat(32)))
        );
        assert!(
            !format_program_status(&ProgramStatus {
                state: ProgramStatusState::Idle,
                app: Some("a".repeat(33)),
                kind: None,
                message: None,
            })
            .contains("app=")
        );
    }

    // program-status.test.ts: "replaces control characters, which make terminals discard the report"
    #[test]
    fn replaces_control_characters_which_make_terminals_discard_the_report() {
        let sequence = format_program_status(&ProgramStatus {
            state: ProgramStatusState::Error,
            app: None,
            kind: None,
            message: Some("first\nsecond\x1b[31m\u{009b}third\t".into()),
        });
        assert_eq!(
            decode_message(&sequence).as_deref(),
            Some("first second [31m third")
        );
    }

    // program-status.test.ts: "cuts long messages at a UTF-8 boundary within the spec limits"
    #[test]
    fn cuts_long_messages_at_a_utf8_boundary_within_the_spec_limits() {
        let sequence = format_program_status(&ProgramStatus {
            state: ProgramStatusState::Working,
            app: Some("pi".into()),
            kind: None,
            message: Some("é".repeat(2000)),
        });
        let message = decode_message(&sequence).expect("msg");
        assert_eq!(message, "é".repeat(1024));
        assert!(message.len() <= 2048);
        assert!(sequence.len() <= 4096);
    }

    // program-status.test.ts: "accepts the query echo with either terminator and future pairs"
    #[test]
    fn accepts_the_query_echo_with_either_terminator_and_future_pairs() {
        assert!(is_program_status_reply("\x1b]7501;?\x1b\\"));
        assert!(is_program_status_reply("\x1b]7501;?\x07"));
        assert!(is_program_status_reply("\x1b]7501;?version=2\x1b\\"));
        assert!(!is_program_status_reply("\x1b]7501;state=idle\x1b\\"));
        assert!(!is_program_status_reply("\x1b]11;rgb:0000/0000/0000\x07"));
    }
}
