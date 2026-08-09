//! Startup diagnostics (requirements §3.1 diagnostics system).
//!
//! Args/settings parsing produces warning/error diagnostics; any error
//! diagnostic aborts startup with exit code 1. Settings-sourced diagnostics
//! carry a scope prefix (`global` / `project`, settings-manager.ts).

use std::fmt;

/// `"info" | "warning" | "error"` (args.ts:54, agent-session-services.ts:26).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticLevel {
    Info,
    Warning,
    Error,
}

impl DiagnosticLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            DiagnosticLevel::Info => "info",
            DiagnosticLevel::Warning => "warning",
            DiagnosticLevel::Error => "error",
        }
    }
}

impl fmt::Display for DiagnosticLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One diagnostic. `scope` is set for settings-sourced diagnostics
/// (`global` / `project`); args diagnostics have no scope (args.ts:54).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub level: DiagnosticLevel,
    pub scope: Option<String>,
    pub message: String,
}

impl Diagnostic {
    pub fn warning(message: impl Into<String>) -> Self {
        Diagnostic {
            level: DiagnosticLevel::Warning,
            scope: None,
            message: message.into(),
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Diagnostic {
            level: DiagnosticLevel::Error,
            scope: None,
            message: message.into(),
        }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.scope {
            Some(scope) => write!(f, "{}: {}", scope, self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// True when any diagnostic is an error (startup aborts, exit 1).
pub fn has_error(diagnostics: &[Diagnostic]) -> bool {
    diagnostics
        .iter()
        .any(|d| d.level == DiagnosticLevel::Error)
}
