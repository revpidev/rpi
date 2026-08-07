//! Port of `packages/coding-agent/src/core/tools/ls.ts` @ pi 0.82.1 (2efa728).
//!
//! Pure filesystem directory listing — no external process involved upstream,
//! so this port is a direct translation.
//!
//! Intentional difference (deviation D-039): upstream sorts with
//! `toLowerCase().localeCompare(...)` (ICU collation); this port compares the
//! lowercased names by Unicode code point. Identical for plain ASCII
//! alphanumeric names; entries mixing punctuation/underscores with letters
//! may order differently.
//!
//! TUI rendering methods (`renderCall`, `renderResult`) are intentionally
//! omitted — rendering lives in the TUI layer.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use pir_agent::{AgentError, AgentTool, AgentToolResult, AgentToolUpdateCallback};
use pir_ai::types::{TextContent, ToolResultContent};
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;

use crate::tools::path_utils::resolve_to_cwd;
use crate::tools::truncate::{format_size, truncate_head, TruncateOptions, DEFAULT_MAX_BYTES};
use crate::tools::ToolContext;

/// Default maximum number of entries (ls.ts:21).
const DEFAULT_LIMIT: f64 = 500.0;

// ---------------------------------------------------------------------------
// LsOperations (ls.ts:32-45)
// ---------------------------------------------------------------------------

/// Pluggable operations for the ls tool.
///
/// Override these to delegate directory listing to remote systems (for
/// example SSH).
#[async_trait]
pub trait LsOperations: Send + Sync {
    /// Check if path exists.
    async fn exists(&self, absolute_path: &Path) -> bool;

    /// Get file or directory stats. Errors if not found. Symlinks are
    /// followed (upstream `fsStat`).
    async fn is_directory(&self, absolute_path: &Path) -> std::io::Result<bool>;

    /// Read directory entry names.
    async fn read_dir(&self, absolute_path: &Path) -> std::io::Result<Vec<String>>;
}

/// Default local-filesystem implementation of [`LsOperations`] (ls.ts:41-45).
pub struct LocalLsOperations;

#[async_trait]
impl LsOperations for LocalLsOperations {
    async fn exists(&self, absolute_path: &Path) -> bool {
        tokio::fs::try_exists(absolute_path).await.unwrap_or(false)
    }

    async fn is_directory(&self, absolute_path: &Path) -> std::io::Result<bool> {
        // tokio::fs::metadata follows symlinks, like upstream fsStat.
        Ok(tokio::fs::metadata(absolute_path).await?.is_dir())
    }

    async fn read_dir(&self, absolute_path: &Path) -> std::io::Result<Vec<String>> {
        let mut entries = Vec::new();
        let mut read_dir = tokio::fs::read_dir(absolute_path).await?;
        while let Some(entry) = read_dir.next_entry().await? {
            entries.push(entry.file_name().to_string_lossy().into_owned());
        }
        Ok(entries)
    }
}

// ---------------------------------------------------------------------------
// LsToolOptions (ls.ts:47-50)
// ---------------------------------------------------------------------------

/// Options for creating an ls tool instance.
#[derive(Default)]
pub struct LsToolOptions {
    /// Custom operations for directory listing. Default: local filesystem.
    pub operations: Option<Arc<dyn LsOperations>>,
}

// ---------------------------------------------------------------------------
// createLsTool (ls.ts:95-105, 223-225)
// ---------------------------------------------------------------------------

/// Create an ls tool bound to the given context.
pub fn create_ls_tool(ctx: &ToolContext, options: LsToolOptions) -> Arc<dyn AgentTool> {
    let operations = options
        .operations
        .unwrap_or_else(|| Arc::new(LocalLsOperations));
    Arc::new(LsTool {
        cwd: ctx.cwd.clone(),
        operations,
    })
}

// ---------------------------------------------------------------------------
// LsTool
// ---------------------------------------------------------------------------

struct LsTool {
    cwd: PathBuf,
    operations: Arc<dyn LsOperations>,
}

/// Tool description with constants expanded (ls.ts:103).
const DESCRIPTION: &str = "List directory contents. Returns entries sorted alphabetically, \
with '/' suffix for directories. Includes dotfiles. Output is truncated to 500 entries or \
50KB (whichever is hit first).";

/// Format a number for display in notices (JS `${limit}` semantics).
fn format_number(v: f64) -> String {
    if v.fract() == 0.0 && v.is_finite() {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

#[async_trait]
impl AgentTool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }

    fn label(&self) -> &str {
        "ls"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters(&self) -> &Value {
        // TypeBox Type.Object with additionalProperties: false (ls.ts:14-17).
        static PARAMETERS: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        PARAMETERS.get_or_init(|| {
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory to list (default: current directory)"
                    },
                    "limit": {
                        "type": "number",
                        "description": "Maximum number of entries to return (default: 500)"
                    }
                },
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
        // --- Extract parameters (ls.ts:106-112) ---
        let path = params["path"].as_str();
        let limit = params["limit"].as_f64();

        // --- Abort check at entry (ls.ts:114-117) ---
        if signal.is_cancelled() {
            return Err(AgentError::Message("Operation aborted".to_string()));
        }

        let dir_path = resolve_to_cwd(path.unwrap_or("."), &self.cwd);
        let effective_limit = limit.unwrap_or(DEFAULT_LIMIT);

        // Check if path exists (ls.ts:128-131).
        if !self.operations.exists(&dir_path).await {
            return Err(AgentError::Message(format!(
                "Path not found: {}",
                dir_path.display()
            )));
        }

        // Check if path is a directory (ls.ts:133-138). A stat failure here
        // propagates the raw error, as upstream's rejection does.
        let is_directory = self
            .operations
            .is_directory(&dir_path)
            .await
            .map_err(|e| AgentError::Message(e.to_string()))?;
        if !is_directory {
            return Err(AgentError::Message(format!(
                "Not a directory: {}",
                dir_path.display()
            )));
        }

        // Read directory entries (ls.ts:140-147).
        let mut entries = self
            .operations
            .read_dir(&dir_path)
            .await
            .map_err(|e| AgentError::Message(format!("Cannot read directory: {e}")))?;

        // Sort alphabetically, case-insensitive (ls.ts:149-150). See the
        // module header for the localeCompare → code-point deviation (D-039).
        entries.sort_by_key(|a| a.to_lowercase());

        // Format entries with directory indicators (ls.ts:152-171).
        let mut results: Vec<String> = Vec::new();
        let mut entry_limit_reached = false;
        for entry in &entries {
            // Per-entry cancellation (T14 review M4): a long directory's
            // read_dir + sort + stat loop must not run to completion past
            // an abort (upstream rejects immediately via its abort
            // listener).
            if signal.is_cancelled() {
                return Err(AgentError::Message("Operation aborted".to_string()));
            }
            if results.len() as f64 >= effective_limit {
                entry_limit_reached = true;
                break;
            }
            let full_path = dir_path.join(entry);
            let suffix = match self.operations.is_directory(&full_path).await {
                Ok(true) => "/",
                Ok(false) => "",
                // Skip entries we cannot stat (ls.ts:165-168).
                Err(_) => continue,
            };
            results.push(format!("{entry}{suffix}"));
        }

        if signal.is_cancelled() {
            return Err(AgentError::Message("Operation aborted".to_string()));
        }

        if results.is_empty() {
            return Ok(AgentToolResult {
                content: vec![ToolResultContent::Text(TextContent {
                    text: "(empty directory)".to_string(),
                    text_signature: None,
                })],
                details: Value::Null,
                usage: None,
                added_tool_names: None,
                terminate: None,
            });
        }

        // Byte truncation + notices (ls.ts:180-202).
        let raw_output = results.join("\n");
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
        if entry_limit_reached {
            notices.push(format!(
                "{} entries limit reached. Use limit={} for more",
                format_number(effective_limit),
                format_number(effective_limit * 2.0)
            ));
            let limit_value = if effective_limit.fract() == 0.0 {
                json!(effective_limit as i64)
            } else {
                json!(effective_limit)
            };
            details.insert("entryLimitReached".to_string(), limit_value);
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

        Ok(AgentToolResult {
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
        })
    }
}
