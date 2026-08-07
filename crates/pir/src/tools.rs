//! Built-in tools support modules.
//!
//! This module groups the shared infrastructure used by the built-in tools
//! (read, write, edit, bash, and the optional grep/find/ls): truncation, path
//! resolution, file-mutation queueing, output accumulation, and output
//! sanitization.
//!
//! Intentional Rust difference: upstream's `ToolContext` carries `signal`
//! (AbortSignal) and `onUpdate` callback. In the Rust port these are supplied
//! by `pir-agent`'s `AgentTool::execute` parameters and are **not** part of
//! `ToolContext`. This keeps the context cheaply cloneable and free of
//! lifetime-bound callback handles.

pub mod bash;
pub mod bash_executor;
pub mod edit;
pub mod edit_diff;
pub mod file_mutation_queue;
pub mod find;
pub mod grep;
pub mod image_process;
pub mod ls;
pub mod mime;
pub mod output_accumulator;
pub mod path_utils;
pub mod read;
pub mod sanitize;
pub mod truncate;
pub mod write;

use std::path::PathBuf;
use std::sync::Arc;

use pir_agent::types::AgentTool;

/// Context passed to every built-in tool execution.
///
/// Port of the `ToolContext` shape used throughout `packages/coding-agent/src/core/tools/`.
/// The `cwd` is the working directory for path resolution; `session_env` carries
/// optional session/model metadata injected into `PIR_*` environment variables.
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// Working directory for resolving relative paths.
    pub cwd: PathBuf,
    /// Optional session environment (model, session id, etc.).
    ///
    /// Shared cell (T10): the owning `AgentSession` updates the contents on
    /// model/thinking/session-file changes so the bash tool resolves `PIR_*`
    /// per command spawn (requirements §3.3: 模型切换即时生效).
    pub session_env: Option<std::sync::Arc<std::sync::RwLock<SessionEnv>>>,
}

/// Session-level environment metadata, surfaced as `PIR_*` env vars in bash.
///
/// Port of the session/model fields read in `bash.ts` `resolveSpawnContext`.
/// Upstream names are `PI_*`; Pir intentionally renames the prefix to `PIR_*`
/// (ADR-0001, requirements §1.4).
#[derive(Debug, Clone)]
pub struct SessionEnv {
    pub session_id: String,
    pub session_file: Option<PathBuf>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub reasoning_level: Option<String>,
}

// ---------------------------------------------------------------------------
// Built-in tool set wiring
// ---------------------------------------------------------------------------

/// Default active built-in tool names.
///
/// Port of `defaultActiveToolNames` in `packages/coding-agent/src/core/sdk.ts:246`
/// @ pi 0.82.1 (2efa728).
pub const DEFAULT_ACTIVE_TOOL_NAMES: [&str; 4] = ["read", "bash", "edit", "write"];

/// Resolve the initial active tool name list from CLI-style switches.
///
/// Port of the name-resolution logic in `sdk.ts:246-252`: an explicit `--tools`
/// allowlist wins; otherwise `--no-tools` yields an empty list; otherwise the
/// default set. The `--exclude-tools` denylist is applied last (deny after
/// allow). Names not matching a built-in tool are kept as-is — they are
/// resolved against extension/custom tools by the caller (T10/T15 wiring).
pub fn resolve_active_tool_names(
    tools: Option<&[String]>,
    no_tools: bool,
    exclude_tools: Option<&[String]>,
) -> Vec<String> {
    let base: Vec<String> = if let Some(allow) = tools {
        allow.to_vec()
    } else if no_tools {
        Vec::new()
    } else {
        DEFAULT_ACTIVE_TOOL_NAMES
            .iter()
            .map(|s| (*s).to_owned())
            .collect()
    };
    match exclude_tools {
        Some(deny) => base.into_iter().filter(|n| !deny.contains(n)).collect(),
        None => base,
    }
}

/// Settings-derived options for the built-in tools (`_buildRuntime`,
/// agent-session.ts:2552-2564). `None` keeps each tool's own default.
#[derive(Default)]
pub struct BuiltinToolOptions {
    /// `settingsManager.getImageAutoResize()` → read tool.
    pub auto_resize_images: Option<bool>,
    /// `settingsManager.getShellCommandPrefix()` → bash tool.
    pub shell_command_prefix: Option<String>,
    /// `settingsManager.getShellPath()` → bash tool.
    pub shell_path: Option<String>,
}

/// Construct the built-in tools selected by `active_names`, in list order.
///
/// Unknown names are skipped here (extension/custom tools are attached by the
/// session assembly layer in T10/T15).
pub fn create_builtin_tools(
    ctx: &ToolContext,
    active_names: &[String],
    options: &BuiltinToolOptions,
) -> Vec<Arc<dyn AgentTool>> {
    active_names
        .iter()
        .filter_map(|name| match name.as_str() {
            "read" => {
                let mut read_options = read::ReadToolOptions::default();
                if let Some(auto_resize_images) = options.auto_resize_images {
                    read_options.auto_resize_images = auto_resize_images;
                }
                Some(read::create_read_tool(ctx, read_options))
            }
            "bash" => Some(bash::create_bash_tool(
                ctx,
                bash::BashToolOptions {
                    command_prefix: options.shell_command_prefix.clone(),
                    shell_path: options.shell_path.clone(),
                    ..Default::default()
                },
            )),
            "edit" => Some(edit::create_edit_tool(
                ctx,
                edit::EditToolOptions::default(),
            )),
            "write" => Some(write::create_write_tool(
                ctx,
                write::WriteToolOptions::default(),
            )),
            "grep" => Some(grep::create_grep_tool(
                ctx,
                grep::GrepToolOptions::default(),
            )),
            "find" => Some(find::create_find_tool(
                ctx,
                find::FindToolOptions::default(),
            )),
            "ls" => Some(ls::create_ls_tool(ctx, ls::LsToolOptions::default())),
            _ => None,
        })
        .collect()
}

/// Map a `std::io::Error` to the upstream Node `error.code` message format.
///
/// Upstream uses `error.code ? "Error code: ${error.code}" : String(error)` in
/// edit/write tools (`edit.ts:328-329`). This function replicates the mapping
/// from Rust's `io::Error` to the textual error codes callers expect.
pub(crate) fn io_error_message(e: &std::io::Error) -> String {
    // On Unix, prefer raw_os_error for exact errno → error-code mapping.
    // This correctly distinguishes EPERM (1) from EACCES (13), which both map
    // to `ErrorKind::PermissionDenied` in Rust.
    #[cfg(unix)]
    {
        if let Some(code) = e.raw_os_error() {
            match code {
                1 => return "Error code: EPERM".to_string(),
                2 => return "Error code: ENOENT".to_string(),
                13 => return "Error code: EACCES".to_string(),
                17 => return "Error code: EEXIST".to_string(),
                20 => return "Error code: ENOTDIR".to_string(),
                21 => return "Error code: EISDIR".to_string(),
                _ => {}
            }
        }
    }

    // Fallback for non-Unix or errors without a raw OS error code.
    match e.kind() {
        std::io::ErrorKind::NotFound => "Error code: ENOENT".to_string(),
        std::io::ErrorKind::PermissionDenied => "Error code: EACCES".to_string(),
        _ => format!("Error: {e}"),
    }
}

/// Generate a 16-char lowercase hex string (8 bytes of entropy).
///
/// Equivalent to upstream `randomBytes(8).toString("hex")` used for temp-file
/// naming. This is **not** cryptographically secure — it mixes system time
/// (nanos), process id, and a global atomic counter to produce a unique-enough
/// identifier for temp-file names within a single process lifetime.
pub(crate) fn random_hex_16() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);

    // Non-cryptographic mix — only used for temp-file name uniqueness.
    let mixed = nanos
        .wrapping_add(pid.rotate_left(17))
        .wrapping_add(count.rotate_left(31))
        .wrapping_mul(0x517cc1b727220a95);

    format!("{mixed:016x}")
}

// ---------------------------------------------------------------------------
// test helpers shared across tool submodules
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod test_helpers {
    use std::path::{Path, PathBuf};

    /// RAII temp directory with a unique name derived from `random_hex_16`.
    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("pir-test-{}", super::random_hex_16()));
            std::fs::create_dir_all(&dir).expect("failed to create temp dir for test");
            TempDir(dir)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod wiring_tests {
    //! Tests for `resolve_active_tool_names` / `create_builtin_tools`
    //! (sdk.ts:246-252 name-resolution semantics).
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn test_default_active_set_is_read_bash_edit_write() {
        let active = resolve_active_tool_names(None, false, None);
        assert_eq!(active, names(&["read", "bash", "edit", "write"]));
    }

    #[test]
    fn test_allowlist_wins_over_default() {
        let allow = names(&["read", "grep"]);
        let active = resolve_active_tool_names(Some(&allow), false, None);
        assert_eq!(active, names(&["read", "grep"]));
    }

    #[test]
    fn test_no_tools_yields_empty_set() {
        let active = resolve_active_tool_names(None, true, None);
        assert!(active.is_empty());
    }

    #[test]
    fn test_denylist_applies_after_allowlist() {
        let allow = names(&["read", "bash"]);
        let deny = names(&["bash"]);
        let active = resolve_active_tool_names(Some(&allow), false, Some(&deny));
        assert_eq!(active, names(&["read"]));
        // Deny also filters the default set.
        let active = resolve_active_tool_names(None, false, Some(&deny));
        assert_eq!(active, names(&["read", "edit", "write"]));
    }

    #[test]
    fn test_create_builtin_tools_filters_and_orders() {
        let ctx = ToolContext {
            cwd: PathBuf::from("."),
            session_env: None,
        };
        let tools = create_builtin_tools(
            &ctx,
            &names(&["write", "custom-x", "read"]),
            &BuiltinToolOptions::default(),
        );
        let tool_names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        // List order preserved; unknown names skipped (extension tools are
        // attached by the session assembly layer, T10/T15).
        assert_eq!(tool_names, vec!["write", "read"]);
    }

    #[test]
    fn test_create_builtin_tools_includes_optional_tools() {
        // Optional tools (T14 W1) are constructed by the factory; the session
        // assembly decides whether they become active.
        let ctx = ToolContext {
            cwd: PathBuf::from("."),
            session_env: None,
        };
        let tools = create_builtin_tools(
            &ctx,
            &names(&["read", "grep", "find", "ls"]),
            &BuiltinToolOptions::default(),
        );
        let tool_names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(tool_names, vec!["read", "grep", "find", "ls"]);
    }
}
