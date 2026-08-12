//! Port of `packages/coding-agent/src/core/tools/find.ts` @ pi 0.82.1 (2efa728).
//!
//! Native Rust implementation of the fd-backed find tool (ADR-0003 §2):
//! upstream shells out to `fd --glob --color=never --hidden [--no-require-git]
//! --max-results N [--full-path]`; this port reproduces the same observable
//! behavior with the `ignore` + `globset` crates and downloads no external
//! binary (gate G4).
//!
//! fd semantics replicated (verified against fd 10.4):
//! - `--hidden`: hidden files are searched.
//! - gitignore rules: inside a git repository fd applies its default
//!   git-aware behavior (parent `.gitignore` rules stop at nested repo
//!   boundaries); outside a repository upstream adds `--no-require-git` so
//!   gitignore rules still apply. Repository detection walks up from the
//!   search path looking for a `.git` entry (find.ts:230-239).
//! - `** /node_modules/**` and `**/.git/**` are always excluded
//!   (requirements §4.5: always ignore node_modules/.git, matching
//!   upstream's
//!   custom-operations ignore list at find.ts:165).
//! - A pattern containing `/` switches to full-path matching and gets a
//!   `**/` prefix unless it starts with `/` or `**/` or equals `**`
//!   (find.ts:246-252). Otherwise the pattern matches the basename.
//! - Directories are reported with a trailing `/`; results are relative to
//!   the search directory.
//! - Walk errors fail the search only when nothing was found (fd exits
//!   non-zero; find.ts:290-296).
//!
//! TUI rendering methods (`renderCall`, `renderResult`) are intentionally
//! omitted — rendering lives in the TUI layer.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use globset::GlobBuilder;
use ignore::WalkBuilder;
use rpi_agent::{AgentError, AgentTool, AgentToolResult, AgentToolUpdateCallback};
use rpi_ai::types::{TextContent, ToolResultContent};
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;

use crate::tools::path_utils::resolve_to_cwd;
use crate::tools::truncate::{format_size, truncate_head, TruncateOptions, DEFAULT_MAX_BYTES};
use crate::tools::ToolContext;

/// Default maximum number of results (find.ts:30).
const DEFAULT_LIMIT: f64 = 1000.0;

/// Always-pruned directory names (requirements §4.5; upstream custom-ops
/// ignore list `["**/node_modules/**", "**/.git/**"]`, find.ts:165).
const PRUNED_DIR_NAMES: [&str; 2] = ["node_modules", ".git"];

// ---------------------------------------------------------------------------
// FindOperations (find.ts:41-52)
// ---------------------------------------------------------------------------

/// Pluggable operations for the find tool.
///
/// Override these to delegate file search to remote systems (for example
/// SSH). When `glob` is overridden it replaces the built-in walker entirely
/// (find.ts:154-211).
#[async_trait]
pub trait FindOperations: Send + Sync {
    /// Check if path exists.
    async fn exists(&self, absolute_path: &Path) -> bool;

    /// Find files matching a glob pattern under `cwd`. Returns relative or
    /// absolute paths.
    async fn glob(&self, pattern: &str, cwd: &Path, ignore: &[String], limit: f64) -> Vec<String>;
}

// ---------------------------------------------------------------------------
// FindToolOptions (find.ts:54-57)
// ---------------------------------------------------------------------------

/// Options for creating a find tool instance.
#[derive(Default)]
pub struct FindToolOptions {
    /// Custom operations for find. Default: native walker (upstream: fd).
    pub operations: Option<Arc<dyn FindOperations>>,
}

// ---------------------------------------------------------------------------
// createFindTool (find.ts:109-119, 372-374)
// ---------------------------------------------------------------------------

/// Create a find tool bound to the given context.
pub fn create_find_tool(ctx: &ToolContext, options: FindToolOptions) -> Arc<dyn AgentTool> {
    Arc::new(FindTool {
        cwd: ctx.cwd.clone(),
        operations: options.operations,
    })
}

// ---------------------------------------------------------------------------
// FindTool
// ---------------------------------------------------------------------------

struct FindTool {
    cwd: PathBuf,
    operations: Option<Arc<dyn FindOperations>>,
}

/// Tool description with constants expanded (find.ts:117).
const DESCRIPTION: &str = "Search for files by glob pattern. Returns matching file paths \
relative to the search directory. Respects .gitignore. Output is truncated to 1000 results \
or 50KB (whichever is hit first).";

/// Format a number for display in notices (JS `${limit}` semantics).
fn format_number(v: f64) -> String {
    if v.fract() == 0.0 && v.is_finite() {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// JS-number JSON value (no trailing ".0" for integers).
fn number_value(v: f64) -> Value {
    if v.fract() == 0.0 && v.is_finite() {
        json!(v as i64)
    } else {
        json!(v)
    }
}

/// `/`-separated path string (toPosixPath component; replaces native `\`
/// separators from Windows-style results with `/`).
fn to_posix_path(value: &str) -> String {
    value.replace('\\', "/")
}

/// `relativizeFindResultPath` (find.ts:17-27 @ 523b5a491): strip the search
/// root from absolute result paths (preserving the trailing separator of
/// directories), normalize to posix separators, and pass relative results
/// through unchanged.
///
/// On Unix `path.sep` is `/`, so `hadTrailingSeparator` checks for `/` only;
/// the Windows `[/\\]` fallback is gated behind `cfg!(windows)`.
fn relativize_find_result_path(result_path: &str, search_path: &str) -> String {
    let had_trailing_sep =
        result_path.ends_with('/') || (cfg!(windows) && result_path.ends_with('\\'));
    let relative_path = if Path::new(result_path).is_absolute() {
        // path.relative(searchPath, resultPath): strip the search root only
        // when the result is actually a descendant (i.e. the prefix is
        // followed by a path separator or equals the full result). This
        // prevents matching siblings with a shared string prefix
        // (523b5a491 / #6104: `/foo/bar-baz` is NOT a child of `/foo/bar`).
        let search = search_path.trim_end_matches('/');
        if result_path == search {
            String::new()
        } else if let Some(rest) = result_path.strip_prefix(search) {
            if rest.starts_with('/') {
                rest.trim_start_matches('/').to_owned()
            } else {
                // Sibling with shared prefix — not a descendant, keep as-is.
                result_path.to_owned()
            }
        } else {
            result_path.to_owned()
        }
    } else {
        result_path.to_owned()
    };
    let posix_path = to_posix_path(&relative_path);
    if had_trailing_sep && !posix_path.ends_with('/') {
        format!("{posix_path}/")
    } else {
        posix_path
    }
}

/// `pathExists` walk-up git repository detection (find.ts:230-239).
fn is_inside_git_repo(search_path: &Path) -> bool {
    let mut current = Some(search_path);
    while let Some(dir) = current {
        if dir.join(".git").try_exists().unwrap_or(false) {
            return true;
        }
        current = dir.parent();
    }
    false
}

/// Outcome of the blocking walk phase.
struct WalkOutcome {
    /// Relativised, posix-separated result paths (directories keep their
    /// trailing `/`).
    results: Vec<String>,
    /// Walk error messages (fd's stderr lines).
    errors: Vec<String>,
    /// Cancelled via the abort signal mid-walk.
    aborted: bool,
}

/// Run the fd-equivalent walk (blocking; call inside `spawn_blocking`).
fn run_walk(
    pattern: &str,
    search_path: &Path,
    limit: f64,
    signal: &CancellationToken,
) -> Result<WalkOutcome, AgentError> {
    let mut outcome = WalkOutcome {
        results: Vec::new(),
        errors: Vec::new(),
        aborted: false,
    };

    // fd rejects a non-directory search path (find.ts:290-296 surfaces fd's
    // stderr; the first fd line is this one).
    if !search_path.is_dir() {
        return Err(AgentError::Message(format!(
            "Search path '{}' is not a directory.",
            search_path.display()
        )));
    }

    // --full-path + `**/` prefixing for path-containing patterns
    // (find.ts:243-252).
    let full_path = pattern.contains('/');
    let effective_pattern =
        if full_path && !pattern.starts_with('/') && !pattern.starts_with("**/") && pattern != "**"
        {
            format!("**/{pattern}")
        } else {
            pattern.to_string()
        };
    // fd glob semantics: `*` does not cross `/`, `**` does.
    let matcher = GlobBuilder::new(&effective_pattern)
        .literal_separator(true)
        .build()
        .map_err(|e| AgentError::Message(e.to_string()))?
        .compile_matcher();

    let mut builder = WalkBuilder::new(search_path);
    builder
        // fd --hidden: search hidden files and directories.
        .hidden(false)
        // fd default: no symlink following.
        .follow_links(false)
        // fd respects `.ignore` and `.fdignore` files in addition to the
        // gitignore chain.
        .add_custom_ignore_filename(".fdignore")
        // --no-require-git outside repositories (find.ts:226-240).
        .require_git(is_inside_git_repo(search_path))
        // Fixed prune of node_modules/.git (requirements §4.5).
        .filter_entry(|entry| {
            entry.depth() == 0
                || !(entry.file_type().is_some_and(|ft| ft.is_dir())
                    && entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| PRUNED_DIR_NAMES.contains(&name)))
        });

    for result in builder.build() {
        if signal.is_cancelled() {
            outcome.aborted = true;
            return Ok(outcome);
        }
        let entry = match result {
            Ok(entry) => entry,
            Err(e) => {
                outcome.errors.push(e.to_string());
                continue;
            }
        };
        if entry.depth() == 0 {
            continue;
        }
        let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
        // fd --glob matches the basename by default, the full (absolute, as
        // displayed) path with --full-path; directories match without their
        // trailing separator.
        let matched = if full_path {
            matcher.is_match(entry.path())
        } else {
            matcher.is_match(entry.file_name())
        };
        if !matched {
            continue;
        }
        // Relativise against the search root (find.ts:307-320); directories
        // keep fd's trailing slash.
        let relative = match entry.path().strip_prefix(search_path) {
            Ok(relative) => relative.to_string_lossy().into_owned(),
            Err(_) => entry.path().to_string_lossy().into_owned(),
        };
        let mut relative = to_posix_path(&relative);
        if is_dir && !relative.ends_with('/') {
            relative.push('/');
        }
        outcome.results.push(relative);
        if outcome.results.len() as f64 >= limit {
            return Ok(outcome);
        }
    }
    Ok(outcome)
}

/// Shared result assembly (find.ts:182-209, 322-346).
///
/// `refine_hint` selects the limit-notice wording: the fd/native branch
/// appends "Use limit=N*2 for more, or refine pattern" (find.ts:330), the
/// custom-operations branch does not (find.ts:194).
fn assemble_result(
    relativized: Vec<String>,
    effective_limit: f64,
    refine_hint: bool,
) -> AgentToolResult {
    let result_limit_reached = relativized.len() as f64 >= effective_limit;
    let raw_output = relativized.join("\n");
    let truncation = truncate_head(
        &raw_output,
        Some(TruncateOptions {
            max_lines: usize::MAX,
            max_bytes: DEFAULT_MAX_BYTES,
        }),
    );
    let mut output = truncation.content.clone();
    let mut details = Map::new();
    let mut notices: Vec<String> = Vec::new();
    if result_limit_reached {
        if refine_hint {
            notices.push(format!(
                "{} results limit reached. Use limit={} for more, or refine pattern",
                format_number(effective_limit),
                format_number(effective_limit * 2.0)
            ));
        } else {
            notices.push(format!(
                "{} results limit reached",
                format_number(effective_limit)
            ));
        }
        details.insert(
            "resultLimitReached".to_string(),
            number_value(effective_limit),
        );
    }
    if truncation.truncated {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        details.insert(
            "truncation".to_string(),
            serde_json::to_value(&truncation).unwrap_or(Value::Null),
        );
    }
    if !notices.is_empty() {
        output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    AgentToolResult {
        content: vec![ToolResultContent::Text(TextContent {
            text: output,
            text_signature: None,
        })],
        details: if details.is_empty() {
            Value::Null
        } else {
            Value::Object(details)
        },
        usage: None,
        added_tool_names: None,
        terminate: None,
    }
}

fn no_files_found() -> AgentToolResult {
    AgentToolResult {
        content: vec![ToolResultContent::Text(TextContent {
            text: "No files found matching pattern".to_string(),
            text_signature: None,
        })],
        details: Value::Null,
        usage: None,
        added_tool_names: None,
        terminate: None,
    }
}

#[async_trait]
impl AgentTool for FindTool {
    fn name(&self) -> &str {
        "find"
    }

    fn label(&self) -> &str {
        "find"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters(&self) -> &Value {
        // TypeBox Type.Object with additionalProperties: false (find.ts:20-26).
        static PARAMETERS: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        PARAMETERS.get_or_init(|| {
            json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'"
                    },
                    "path": {
                        "type": "string",
                        "description": "Directory to search in (default: current directory)"
                    },
                    "limit": {
                        "type": "number",
                        "description": "Maximum number of results (default: 1000)"
                    }
                },
                "required": ["pattern"],
                "additionalProperties": false
            })
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        signal: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, AgentError> {
        // --- Extract parameters (find.ts:120-125) ---
        let pattern = params["pattern"]
            .as_str()
            .ok_or_else(|| AgentError::Message("Missing required parameter: pattern".to_string()))?
            .to_string();
        let search_dir = params["path"].as_str();
        let limit = params["limit"].as_f64();

        // --- Abort check at entry (find.ts:128-131) ---
        if signal.is_cancelled() {
            return Err(AgentError::Message("Operation aborted".to_string()));
        }

        let search_path = resolve_to_cwd(search_dir.unwrap_or("."), &self.cwd);
        let effective_limit = limit.unwrap_or(DEFAULT_LIMIT);

        // --- Custom operations branch (find.ts:154-211) ---
        if let Some(ops) = &self.operations {
            if !ops.exists(&search_path).await {
                return Err(AgentError::Message(format!(
                    "Path not found: {}",
                    search_path.display()
                )));
            }
            if signal.is_cancelled() {
                return Err(AgentError::Message("Operation aborted".to_string()));
            }
            let ignore = vec!["**/node_modules/**".to_string(), "**/.git/**".to_string()];
            let results = ops
                .glob(&pattern, &search_path, &ignore, effective_limit)
                .await;
            if signal.is_cancelled() {
                return Err(AgentError::Message("Operation aborted".to_string()));
            }
            if results.is_empty() {
                return Ok(no_files_found());
            }
            // Relativise against the search root for stable output
            // (relativizeFindResultPath, find.ts:17-27 @ 523b5a491).
            let search_root = search_path.display().to_string();
            let relativized: Vec<String> = results
                .iter()
                .map(|p| relativize_find_result_path(p, &search_root))
                .collect();
            return Ok(assemble_result(relativized, effective_limit, false));
        }

        // --- Native walker branch (upstream: fd subprocess) ---
        let search_path_block = search_path.clone();
        let signal_clone = signal.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            run_walk(&pattern, &search_path_block, effective_limit, &signal_clone)
        })
        .await
        .map_err(|e| AgentError::Message(format!("find walk task failed: {e}")))??;

        if outcome.aborted || signal.is_cancelled() {
            return Err(AgentError::Message("Operation aborted".to_string()));
        }
        // fd exits non-zero on errors; upstream rejects only when nothing was
        // printed (find.ts:290-296).
        if outcome.results.is_empty() && !outcome.errors.is_empty() {
            return Err(AgentError::Message(outcome.errors.join("\n")));
        }
        if outcome.results.is_empty() {
            return Ok(no_files_found());
        }
        Ok(assemble_result(outcome.results, effective_limit, true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Port of `relativizeFindResultPath` behavior (find.ts:17-27 @ 523b5a491):
    // absolute paths are stripped of the search root; relative paths pass
    // through; trailing separators are preserved; backslashes become `/`.

    #[test]
    fn relativize_strips_search_root_from_absolute_path() {
        let result = relativize_find_result_path("/foo/bar/sub/file.txt", "/foo/bar");
        assert_eq!(result, "sub/file.txt");
    }

    #[test]
    fn relativize_preserves_directory_trailing_slash() {
        let result = relativize_find_result_path("/foo/bar/sub/", "/foo/bar");
        assert_eq!(result, "sub/");
    }

    #[test]
    fn relativize_passes_relative_path_unchanged() {
        // Relative results from custom glob operations pass through unchanged
        // instead of being resolved against process.cwd() (523b5a491).
        let result = relativize_find_result_path("sub/file.txt", "/foo/bar");
        assert_eq!(result, "sub/file.txt");
    }

    #[test]
    fn relativize_keeps_trailing_slash_on_relative_dir() {
        let result = relativize_find_result_path("sub/", "/foo/bar");
        assert_eq!(result, "sub/");
    }

    #[test]
    fn relativize_does_not_match_sibling_with_shared_prefix() {
        // `/foo/bar-baz` shares a string prefix with `/foo/bar` but is NOT a
        // child — this was the bug fixed in 523b5a491 (#6104). Our prefix
        // check strips the search root followed by `/`, so the sibling is
        // left intact.
        let result = relativize_find_result_path("/foo/bar-baz/file.txt", "/foo/bar");
        assert_eq!(result, "/foo/bar-baz/file.txt");
    }

    #[test]
    fn relativize_normalizes_backslashes_to_posix() {
        let result = relativize_find_result_path("sub\\file.txt", "/foo/bar");
        assert_eq!(result, "sub/file.txt");
    }

    #[test]
    fn relativize_strips_trailing_slash_from_search_root() {
        // The search root may or may not end with `/`; either way the result
        // must be the same.
        let with_slash = relativize_find_result_path("/foo/bar/sub", "/foo/bar/");
        let without_slash = relativize_find_result_path("/foo/bar/sub", "/foo/bar");
        assert_eq!(with_slash, "sub");
        assert_eq!(with_slash, without_slash);
    }
}
