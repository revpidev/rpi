//! Missing-session-cwd handling (header `cwd` no longer exists on disk).
//!
//! Port of `packages/coding-agent/src/core/session-cwd.ts` @ pi 0.82.1
//! (2efa728).

use std::path::Path;

use thiserror::Error;

use crate::core::session_manager::SessionManager;

/// `SessionCwdIssue` (session-cwd.ts:3-7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCwdIssue {
    pub session_file: Option<String>,
    pub session_cwd: String,
    pub fallback_cwd: String,
}

/// `getMissingSessionCwdIssue` (session-cwd.ts:14-33).
pub fn get_missing_session_cwd_issue(
    session_manager: &SessionManager,
    fallback_cwd: &Path,
) -> Option<SessionCwdIssue> {
    let session_file = session_manager.get_session_file()?;
    let session_cwd = session_manager.get_cwd();
    if session_cwd.as_os_str().is_empty() || session_cwd.exists() {
        return None;
    }
    Some(SessionCwdIssue {
        session_file: Some(session_file.to_string_lossy().into_owned()),
        session_cwd: session_cwd.to_string_lossy().into_owned(),
        fallback_cwd: fallback_cwd.to_string_lossy().into_owned(),
    })
}

/// `formatMissingSessionCwdError` (session-cwd.ts:35-38).
pub fn format_missing_session_cwd_error(issue: &SessionCwdIssue) -> String {
    let session_file = issue
        .session_file
        .as_ref()
        .map(|file| format!("\nSession file: {file}"))
        .unwrap_or_default();
    format!(
        "Stored session working directory does not exist: {}{session_file}\nCurrent working directory: {}",
        issue.session_cwd, issue.fallback_cwd
    )
}

/// `formatMissingSessionCwdPrompt` (session-cwd.ts:40-42).
pub fn format_missing_session_cwd_prompt(issue: &SessionCwdIssue) -> String {
    format!(
        "cwd from session file does not exist\n{}\n\ncontinue in current cwd\n{}",
        issue.session_cwd, issue.fallback_cwd
    )
}

/// `MissingSessionCwdError` (session-cwd.ts:44-52).
#[derive(Debug, Error)]
#[error("{}", format_missing_session_cwd_error(.issue))]
pub struct MissingSessionCwdError {
    pub issue: SessionCwdIssue,
}

/// `assertSessionCwdExists` (session-cwd.ts:54-59).
pub fn assert_session_cwd_exists(
    session_manager: &SessionManager,
    fallback_cwd: &Path,
) -> Result<(), MissingSessionCwdError> {
    match get_missing_session_cwd_issue(session_manager, fallback_cwd) {
        Some(issue) => Err(MissingSessionCwdError { issue }),
        None => Ok(()),
    }
}
