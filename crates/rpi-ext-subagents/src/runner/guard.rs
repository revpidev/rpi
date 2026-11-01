//! Long-running guard primitives (pi-subagents `src/runs/shared/long-running-guard.ts`
//! @ v0.83.0 `3a63e0ad`): mutating-tool classification, shell target
//! extraction, failure escalation and active-long-running thresholds.
//!
//! rpi has no intercom/provider-process controller, so only the local child
//! stream face is ported. The parsing helpers are byte-for-byte in spirit:
//! quoted/escaped shell text is ignored for mutation detection, `2>&1` and
//! `/dev/null` are not files, and `tee` accepts flags before its first file.

use regex::Regex;
use std::sync::LazyLock;

use super::control::ResolvedControlConfig;

/// One failed mutating tool attempt (`FailedMutatingAttempt`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedMutatingAttempt {
    pub tool: String,
    pub path: Option<String>,
    pub error: String,
    pub ts: u64,
}

/// Mutating failure state (`createMutatingFailureState`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MutatingFailureState {
    pub consecutive_failures: usize,
    pub last_failure_at: Option<u64>,
    pub recent_failures: Vec<FailedMutatingAttempt>,
    pub last_mutating_path: Option<String>,
    pub repeated_path_failures: usize,
}

/// Long-running notice metrics (`LongRunningNoticeMetrics`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LongRunningMetrics {
    pub started_at: u64,
    pub now: u64,
    pub turns: u64,
    pub tokens: u64,
}

/// Trigger reason (`LongRunningTriggerReason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub enum LongRunningTriggerReason {
    TimeThreshold,
    TurnThreshold,
    TokenThreshold,
}

impl LongRunningTriggerReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TimeThreshold => "time_threshold",
            Self::TurnThreshold => "turn_threshold",
            Self::TokenThreshold => "token_threshold",
        }
    }
}

/// `MUTATING_BASH_PATTERNS` (`long-running-guard.ts:38-51`).
static MUTATING_BASH_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(^|[;&|()\s])rm\s+",
        r"(^|[;&|()\s])mv\s+",
        r"(^|[;&|()\s])cp\s+",
        r"(^|[;&|()\s])mkdir\s+",
        r"(^|[;&|()\s])touch\s+",
        r"(^|[;&|()\s])git\s+apply\b",
        r"(^|[;&|()\s])patch\s+",
        r"(^|[;&|()\s])sed\s+[^\n;&|]*\s-i\b",
        r"(^|[;&|()\s])perl\s+[^\n;&|]*\s-pi\b",
        r"(^|[;&|()]|\n)\s*tee\s+[^|&;]+",
        r"\b(writeFile|writeFileSync|appendFile|appendFileSync)\b",
        r"\bwrite_text\s*\(",
        r#"\bopen\s*\([^)]*,\s*["'][wa]"#,
    ]
    .iter()
    .map(|pattern| Regex::new(pattern).expect("mutating bash pattern"))
    .collect()
});

fn direct_path(args: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    for key in ["path", "file", "filename", "target", "cwd"] {
        if let Some(value) = args.get(key).and_then(serde_json::Value::as_str)
            && !value.trim().is_empty()
        {
            return Some(value.trim().to_string());
        }
    }
    None
}

/// `resolveCurrentPath` (`long-running-guard.ts:55-69`).
pub fn resolve_current_path(
    tool_name: Option<&str>,
    args: Option<&serde_json::Value>,
) -> Option<String> {
    let tool_name = tool_name?;
    let args = args?;
    if let Some(object) = args.as_object()
        && let Some(path) = direct_path(object)
    {
        return Some(path);
    }
    if tool_name == "bash" {
        let command = args.get("command").and_then(serde_json::Value::as_str)?;
        return first_written_file(command);
    }
    None
}

/// Reads one shell word at `start`, removing quotes/escapes (`readShellWord`).
fn read_shell_word(chars: &[char], start: usize) -> (String, usize) {
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut i = start;
    while i < chars.len() {
        let ch = chars[i];
        if let Some(active) = quote {
            if ch == active {
                quote = None;
            } else if ch == '\\' && active == '"' {
                let next = chars.get(i + 1).copied();
                if next.is_some_and(|c| matches!(c, '$' | '`' | '"' | '\\' | '\n')) {
                    i += 1;
                    word.push(chars[i]);
                } else {
                    word.push(ch);
                }
            } else {
                word.push(ch);
            }
            i += 1;
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
        } else if ch == '\\' && i + 1 < chars.len() {
            i += 1;
            word.push(chars[i]);
        } else if ch.is_whitespace() || matches!(ch, ';' | '&' | '|' | '(' | ')' | '<' | '>') {
            break;
        } else {
            word.push(ch);
        }
        i += 1;
    }
    (word, i)
}

/// First unquoted `>`, `>>` or `tee` target, skipping `>&1` and `/dev/null`
/// (`firstWrittenFile`, long-running-guard.ts:76-114).
fn first_written_file(command: &str) -> Option<String> {
    let chars: Vec<char> = command.chars().collect();
    let mut quote: Option<char> = None;
    let mut i = 0usize;
    while i < chars.len() {
        let ch = chars[i];
        if let Some(active) = quote {
            if ch == active {
                quote = None;
            } else if ch == '\\' && active == '"' {
                i += 1;
            }
            i += 1;
            continue;
        }
        if ch == '\'' || ch == '"' {
            quote = Some(ch);
            i += 1;
            continue;
        }
        if ch == '\\' {
            i += 2;
            continue;
        }
        let is_tee = chars[i..].starts_with(&['t', 'e', 'e'])
            && (i == 0 || matches!(chars[i - 1], ' ' | '\t' | '|' | ';' | '&' | '('))
            && chars.get(i + 3).is_some_and(|c| c.is_whitespace());
        if ch != '>' && !is_tee {
            i += 1;
            continue;
        }
        let mut cursor = if is_tee {
            i + 3
        } else if chars.get(i + 1) == Some(&'>') {
            i + 2
        } else {
            i + 1
        };
        loop {
            while cursor < chars.len() && matches!(chars[cursor], ' ' | '\t') {
                cursor += 1;
            }
            if chars.get(cursor) == Some(&'&') {
                break;
            }
            let (word, end) = read_shell_word(&chars, cursor);
            cursor = end;
            if !word.is_empty() && word != "/dev/null" && !(is_tee && word.starts_with('-')) {
                return Some(word);
            }
            if !is_tee || word.is_empty() {
                break;
            }
        }
        i = cursor.saturating_sub(1);
        i += 1;
    }
    None
}

/// `hasUnquotedFileRedirection` (`long-running-guard.ts:116-143`).
fn has_unquoted_file_redirection(command: &str) -> bool {
    let chars: Vec<char> = command.chars().collect();
    let mut in_single = false;
    let mut in_double = false;
    for (i, ch) in chars.iter().copied().enumerate() {
        if ch == '\'' && !in_double {
            in_single = !in_single;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            continue;
        }
        if in_single || in_double || ch != '>' {
            continue;
        }
        if i > 0 && chars[i - 1] == '-' {
            continue;
        }
        let is_double = chars.get(i + 1) == Some(&'>');
        let mut cursor = i + usize::from(is_double) + 1;
        while cursor < chars.len() && chars[cursor].is_whitespace() {
            cursor += 1;
        }
        let Some(target) = chars.get(cursor).copied() else {
            continue;
        };
        if matches!(target, '&' | '|' | ';' | '(' | ')') {
            continue;
        }
        let rest: String = chars[cursor..].iter().collect();
        if rest.starts_with("/dev/null")
            && rest
                .chars()
                .nth("/dev/null".len())
                .is_none_or(|c| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')'))
        {
            continue;
        }
        return true;
    }
    false
}

/// `unquotedShellText` (`long-running-guard.ts:145-170`).
fn unquoted_shell_text(command: &str) -> String {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut result = String::new();
    for ch in command.chars() {
        if escaped {
            result.push(if in_single || in_double { '_' } else { ch });
            escaped = false;
            continue;
        }
        if ch == '\\' && !in_single {
            escaped = true;
            if in_double {
                result.push('_');
            } else {
                result.push(ch);
            }
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            continue;
        }
        result.push(if in_single || in_double { '_' } else { ch });
    }
    result
}

/// `hasMutatingGitCommand` (`long-running-guard.ts:172-182`).
fn has_mutating_git_command(command: &str) -> bool {
    static GIT_MUTATION: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"^git\s+(?:(?:(?:-C|--git-dir|--work-tree)\s+\S+|(?:--git-dir|--work-tree)=\S+|--paginate)\s+)*(?:add|commit|push)\b",
        )
        .expect("git mutation regex")
    });
    static ECHO_PREFIX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^(?:echo|printf)\b").expect("echo regex"));
    for segment in unquoted_shell_text(command).split(['&', '|', ';', '(', ')', '\n']) {
        let trimmed = segment.trim();
        if trimmed.is_empty() || ECHO_PREFIX.is_match(trimmed) {
            continue;
        }
        if GIT_MUTATION.is_match(trimmed) {
            return true;
        }
    }
    false
}

/// `isMutatingBashCommand` (`long-running-guard.ts:184-188`).
pub fn is_mutating_bash_command(command: &str) -> bool {
    has_unquoted_file_redirection(command)
        || has_mutating_git_command(command)
        || MUTATING_BASH_PATTERNS
            .iter()
            .any(|pattern| pattern.is_match(command))
}

/// `isMutatingTool` (`long-running-guard.ts:202-212`).
pub fn is_mutating_tool(
    tool_name: Option<&str>,
    args: Option<&serde_json::Value>,
    mutation_tools: &[String],
) -> bool {
    let Some(tool_name) = tool_name else {
        return false;
    };
    if mutation_tools.iter().any(|name| name == tool_name) {
        return true;
    }
    if tool_name == "edit" || tool_name == "write" {
        return true;
    }
    if tool_name == "cursor" {
        let title = args
            .and_then(|args| args.get("activityTitle"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        static CURSOR_MUTATION: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(?i)^Cursor (?:edit|write)\b").expect("cursor regex"));
        return CURSOR_MUTATION.is_match(title);
    }
    if tool_name != "bash" {
        return false;
    }
    let command = args
        .and_then(|args| args.get("command"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if command.trim().is_empty() {
        return false;
    }
    is_mutating_bash_command(command)
}

/// `didMutatingToolFail` (`long-running-guard.ts:214-216` @ v0.83): upstream
/// checks `result.isError === true`; rpi's child `tool_execution_end` carries
/// that boolean directly, so a string `"true"` can never be mistaken for one.
pub fn did_mutating_tool_fail(is_error: bool) -> bool {
    is_error
}

/// `nextLongRunningTrigger` (`long-running-guard.ts:218-230`): time first,
/// then optional turn/token thresholds.
pub fn next_long_running_trigger(
    config: &ResolvedControlConfig,
    metrics: &LongRunningMetrics,
) -> Option<LongRunningTriggerReason> {
    if metrics.now.saturating_sub(metrics.started_at) >= config.active_notice_after_ms {
        return Some(LongRunningTriggerReason::TimeThreshold);
    }
    if let Some(turns) = config.active_notice_after_turns
        && metrics.turns >= turns
    {
        return Some(LongRunningTriggerReason::TurnThreshold);
    }
    if let Some(tokens) = config.active_notice_after_tokens
        && metrics.tokens >= tokens
    {
        return Some(LongRunningTriggerReason::TokenThreshold);
    }
    None
}

/// `resetMutatingFailureState` (`long-running-guard.ts:232-239`).
pub fn reset_mutating_failure_state(state: &mut MutatingFailureState) {
    *state = MutatingFailureState::default();
}

/// `recordMutatingFailure` (`long-running-guard.ts:241-...`), 5-minute window.
pub fn record_mutating_failure(
    state: &mut MutatingFailureState,
    attempt: FailedMutatingAttempt,
    window_ms: u64,
) {
    if state
        .last_failure_at
        .is_some_and(|last| attempt.ts.saturating_sub(last) > window_ms)
    {
        reset_mutating_failure_state(state);
    }
    state.consecutive_failures += 1;
    state.last_failure_at = Some(attempt.ts);
    if let Some(path) = &attempt.path {
        if state.last_mutating_path.as_deref() == Some(path.as_str()) {
            state.repeated_path_failures += 1;
        } else {
            state.repeated_path_failures = 1;
            state.last_mutating_path = Some(path.clone());
        }
    }
    state.recent_failures.push(attempt);
    let keep = state.recent_failures.len().saturating_sub(3);
    state.recent_failures.drain(..keep);
}

/// `shouldEscalateMutatingFailures` (`long-running-guard.ts`).
pub fn should_escalate_mutating_failures(state: &MutatingFailureState, threshold: u64) -> bool {
    state.consecutive_failures >= threshold as usize
        || state.repeated_path_failures >= threshold as usize
}

/// `summarizeRecentMutatingFailures` (`long-running-guard.ts`).
pub fn summarize_recent_mutating_failures(state: &MutatingFailureState) -> String {
    state
        .recent_failures
        .iter()
        .rev()
        .take(3)
        .map(|failure| {
            let target = failure
                .path
                .as_deref()
                .map(|path| format!("({path})"))
                .unwrap_or_default();
            format!("{}{}: {}", failure.tool, target, failure.error)
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_mutating_bash_commands() {
        for command in [
            "echo hi > out.txt",
            "rm -rf build",
            "git -C repo commit -m x",
            "git add .",
            "sed -e s/a/b/ -i file",
            "tee run.log",
        ] {
            assert!(is_mutating_bash_command(command), "{command}");
        }
        for command in [
            "echo hi",
            "cat file 2>&1",
            "echo hi > /dev/null",
            "echo 'a > b'",
            "git status",
        ] {
            assert!(!is_mutating_bash_command(command), "{command}");
        }
    }

    #[test]
    fn classifies_mutating_tools() {
        assert!(is_mutating_tool(Some("edit"), None, &[]));
        assert!(is_mutating_tool(Some("write"), None, &[]));
        assert!(is_mutating_tool(
            Some("custom"),
            None,
            &["custom".to_string()]
        ));
        assert!(is_mutating_tool(
            Some("cursor"),
            Some(&json!({"activityTitle": "Cursor edit"})),
            &[]
        ));
        assert!(is_mutating_tool(
            Some("cursor"),
            Some(&json!({"activityTitle": "cursor write"})),
            &[]
        ));
        assert!(!is_mutating_tool(Some("read"), None, &[]));
        assert!(!is_mutating_tool(
            Some("bash"),
            Some(&json!({"command": "ls"})),
            &[]
        ));
    }

    #[test]
    fn extracts_written_files_with_quotes_and_fd_duplication() {
        assert_eq!(
            resolve_current_path(
                Some("bash"),
                Some(&json!({"command": "echo x > '/tmp/a b'"}))
            )
            .as_deref(),
            Some("/tmp/a b")
        );
        assert_eq!(
            resolve_current_path(
                Some("bash"),
                Some(&json!({"command": "cmd 2>&1 > out.log"}))
            ),
            Some("out.log".to_string())
        );
        assert_eq!(
            resolve_current_path(Some("bash"), Some(&json!({"command": "tee -a run.log"}))),
            Some("run.log".to_string())
        );
        assert_eq!(
            resolve_current_path(Some("bash"), Some(&json!({"command": "cmd > /dev/null"}))),
            None
        );
    }

    #[test]
    fn failure_state_escalates_and_resets() {
        let mut state = MutatingFailureState::default();
        for ts in [1, 2] {
            record_mutating_failure(
                &mut state,
                FailedMutatingAttempt {
                    tool: "edit".to_string(),
                    path: Some("a".to_string()),
                    error: "boom".to_string(),
                    ts,
                },
                300_000,
            );
        }
        assert!(!should_escalate_mutating_failures(&state, 3));
        record_mutating_failure(
            &mut state,
            FailedMutatingAttempt {
                tool: "edit".to_string(),
                path: Some("a".to_string()),
                error: "boom".to_string(),
                ts: 3,
            },
            300_000,
        );
        assert!(should_escalate_mutating_failures(&state, 3));
        assert!(summarize_recent_mutating_failures(&state).contains("edit(a)"));
        reset_mutating_failure_state(&mut state);
        assert_eq!(state.consecutive_failures, 0);
    }
}
