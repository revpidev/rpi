//! `cli/` — mirrors `packages/coding-agent/src/cli/` (design §12 mapping).

pub mod args;
pub mod auth_check;
pub mod auth_command;
pub mod auth_login;
pub mod auth_logout;
pub mod auth_terminal;
pub mod config_command;
pub mod credential_print;
pub mod diagnostics;
pub mod file_processor;
pub mod initial_message;
pub mod list_models;
pub mod mcp_command;
pub mod package_command;
pub mod progress;
pub mod run_auth;
pub mod session_picker;
