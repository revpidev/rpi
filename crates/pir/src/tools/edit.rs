//! Port of `packages/coding-agent/src/core/tools/edit.ts` @ pi 0.82.1 (2efa728).
//!
//! The edit tool performs exact-text replacements in a file. Every
//! `edits[].oldText` must match a unique, non-overlapping region of the original
//! file. BOM and line-ending style are preserved across edits.

use std::ffi::CString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use pir_agent::error::AgentError;
use pir_agent::types::{AgentTool, AgentToolResult};
use pir_ai::types::{TextContent, ToolResultContent};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::edit_diff::{
    apply_edits_to_normalized_content, detect_line_ending, generate_diff_string,
    generate_unified_patch, normalize_to_lf, restore_line_endings, strip_bom, EditReplacement,
};
use super::file_mutation_queue::with_file_mutation_queue;
use super::path_utils::resolve_to_cwd;
use crate::tools::{io_error_message, ToolContext};

// ---------------------------------------------------------------------------
// Operations trait (edit.ts:74-87)
// ---------------------------------------------------------------------------

/// Pluggable operations for the edit tool.
///
/// Override these to delegate file editing to remote systems (for example SSH).
/// Port of `EditOperations` (edit.ts:74-81).
#[async_trait]
pub trait EditOperations: Send + Sync {
    /// Read file contents as raw bytes.
    async fn read_file(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// Write content to a file as UTF-8.
    async fn write_file(&self, path: &Path, content: &str) -> io::Result<()>;
    /// Check if file is readable and writable (throw if not).
    ///
    /// Upstream uses `fs.access(path, R_OK | W_OK)`. We use `libc::access`
    /// for exact parity on Unix.
    async fn access(&self, path: &Path) -> io::Result<()>;
}

/// Default local filesystem implementation (edit.ts:83-87).
struct DefaultEditOperations;

#[async_trait]
impl EditOperations for DefaultEditOperations {
    async fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        tokio::fs::read(path).await
    }

    async fn write_file(&self, path: &Path, content: &str) -> io::Result<()> {
        tokio::fs::write(path, content).await
    }

    async fn access(&self, path: &Path) -> io::Result<()> {
        // Upstream: fs.access(path, constants.R_OK | W_OK).
        // On Unix we use libc::access for exact R_OK|W_OK parity.
        #[cfg(unix)]
        {
            access_unix(path)
        }
        #[cfg(not(unix))]
        {
            // Non-Unix fallback: check that the file exists and is not read-only.
            let metadata = tokio::fs::metadata(path).await?;
            if metadata.permissions().readonly() {
                return Err(io::Error::from_raw_os_error(13)); // EACCES
            }
            Ok(())
        }
    }
}

/// Unix `access(2)` with `R_OK | W_OK`.
#[cfg(unix)]
fn access_unix(path: &Path) -> io::Result<()> {
    let c_path = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL byte"))?;
    // Safety: c_path is a valid NUL-terminated C string, and libc::access
    // does not retain the pointer beyond the call.
    let ret = unsafe { libc::access(c_path.as_ptr(), libc::R_OK | libc::W_OK) };
    if ret == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

// ---------------------------------------------------------------------------
// Options + factory (edit.ts:89-92, 287-291)
// ---------------------------------------------------------------------------

/// Options for `create_edit_tool`.
///
/// Port of `EditToolOptions` (edit.ts:89-92).
#[derive(Default)]
pub struct EditToolOptions {
    /// Custom operations for file editing. Default: local filesystem.
    pub operations: Option<Arc<dyn EditOperations>>,
}

/// Create an edit tool bound to the given context.
///
/// Port of `createEditToolDefinition` + `createEditTool` (edit.ts:287-437).
pub fn create_edit_tool(ctx: &ToolContext, options: EditToolOptions) -> Arc<dyn AgentTool> {
    let ops = options
        .operations
        .unwrap_or_else(|| Arc::new(DefaultEditOperations));
    let cwd = ctx.cwd.clone();
    Arc::new(EditTool { cwd, ops })
}

// ---------------------------------------------------------------------------
// Tool implementation
// ---------------------------------------------------------------------------

struct EditTool {
    cwd: PathBuf,
    ops: Arc<dyn EditOperations>,
}

/// Static JSON Schema for the edit tool parameters (edit.ts:33-53).
///
/// Intentionally does NOT include `oldText`/`newText` at the top level —
/// those are legacy fields folded into `edits[]` by `prepare_arguments`.
/// Upstream test: "keeps legacy fields out of the public schema".
fn edit_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Path to the file to edit (relative or absolute)"
            },
            "edits": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "oldText": {
                            "type": "string",
                            "description": "Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call."
                        },
                        "newText": {
                            "type": "string",
                            "description": "Replacement text for this targeted edit."
                        }
                    },
                    "required": ["oldText", "newText"]
                },
                "description": "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead."
            }
        },
        "required": ["path", "edits"]
    })
}

static EDIT_PARAMS: std::sync::OnceLock<Value> = std::sync::OnceLock::new();

#[async_trait]
impl AgentTool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn label(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes."
    }

    fn parameters(&self) -> &Value {
        EDIT_PARAMS.get_or_init(edit_parameters)
    }

    /// Legacy shim + JSON string parse (edit.ts:94-118).
    ///
    /// 1. If `edits` is a JSON string, try to parse it as an array.
    ///    (Some models — Opus 4.6, GLM-5.1 — send edits as a JSON string.)
    /// 2. If both top-level `oldText` and `newText` are strings, fold them
    ///    into `edits[]` and remove the top-level fields.
    fn prepare_arguments(&self, args: Value) -> Value {
        let Some(obj) = args.as_object() else {
            // Non-object input passes through unchanged.
            return args;
        };
        let mut map = obj.clone();

        // 1. JSON string edits → array (edit.ts:102-107).
        if let Some(edits_val) = map.get("edits") {
            if let Some(edits_str) = edits_val.as_str() {
                if let Ok(parsed) = serde_json::from_str::<Value>(edits_str) {
                    if parsed.is_array() {
                        map.insert("edits".to_string(), parsed);
                    }
                }
                // On parse failure, leave `edits` as the original string (silent).
            }
        }

        // 2. Legacy oldText/newText → fold into edits (edit.ts:109-117).
        let legacy_old = map
            .get("oldText")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let legacy_new = map
            .get("newText")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        match (legacy_old, legacy_new) {
            (Some(old_text), Some(new_text)) => {
                // Remove top-level legacy fields.
                map.remove("oldText");
                map.remove("newText");

                // Append to existing edits or create a new array.
                let mut edits = map
                    .get("edits")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                edits.push(json!({ "oldText": old_text, "newText": new_text }));
                map.insert("edits".to_string(), Value::Array(edits));

                Value::Object(map)
            }
            _ => {
                // oldText or newText not both strings → return as-is.
                Value::Object(map)
            }
        }
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        signal: CancellationToken,
        _on_update: Option<pir_agent::types::AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, AgentError> {
        // --- Validate input (edit.ts:120-125) ---
        let edits_raw = params.get("edits");
        let path = params
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        let edits_raw_arr = match edits_raw {
            Some(Value::Array(arr)) if !arr.is_empty() => arr,
            _ => {
                return Err(AgentError::Message(
                    "Edit tool input is invalid. edits must contain at least one replacement."
                        .to_string(),
                ));
            }
        };

        // Build EditReplacement list.
        let edits: Vec<EditReplacement> = edits_raw_arr
            .iter()
            .enumerate()
            .map(|(i, e)| EditReplacement {
                old_text: e
                    .get("oldText")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                new_text: e
                    .get("newText")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                edit_index: i,
            })
            .collect();

        let absolute_path = resolve_to_cwd(path, &self.cwd);
        let ops = self.ops.clone();
        let path_owned = path.to_string();
        let edits_count = edits.len();

        with_file_mutation_queue(&absolute_path, || async {
            // Check abort at entry (edit.ts:321).
            if signal.is_cancelled() {
                return Err(AgentError::Message("Operation aborted".to_string()));
            }

            // --- Access check (edit.ts:324-331) ---
            if let Err(e) = ops.access(&absolute_path).await {
                // Check abort after the access attempt, before returning error.
                if signal.is_cancelled() {
                    return Err(AgentError::Message("Operation aborted".to_string()));
                }
                let error_message = io_error_message(&e);
                return Err(AgentError::Message(format!(
                    "Could not edit file: {path_owned}. {error_message}."
                )));
            }

            if signal.is_cancelled() {
                return Err(AgentError::Message("Operation aborted".to_string()));
            }

            // --- Read file (edit.ts:335-336) ---
            let buffer = ops.read_file(&absolute_path).await?;
            if signal.is_cancelled() {
                return Err(AgentError::Message("Operation aborted".to_string()));
            }

            let raw_content = String::from_utf8_lossy(&buffer).into_owned();

            // --- Process content (edit.ts:340-343) ---
            // Strip BOM before matching. The model will not include an invisible BOM in oldText.
            let (bom, content) = strip_bom(&raw_content);
            let original_ending = detect_line_ending(&content);
            let normalized_content = normalize_to_lf(&content);

            let applied =
                match apply_edits_to_normalized_content(&normalized_content, &edits, &path_owned) {
                    Ok(result) => result,
                    Err(err) => return Err(AgentError::Message(err)),
                };

            if signal.is_cancelled() {
                return Err(AgentError::Message("Operation aborted".to_string()));
            }

            // --- Write back (edit.ts:346-347) ---
            let final_content = format!(
                "{bom}{}",
                restore_line_endings(&applied.new_content, original_ending)
            );
            ops.write_file(&absolute_path, &final_content).await?;

            if signal.is_cancelled() {
                return Err(AgentError::Message("Operation aborted".to_string()));
            }

            // --- Generate diff (edit.ts:350-351) ---
            let diff_result = generate_diff_string(&applied.base_content, &applied.new_content, 4);
            let patch =
                generate_unified_patch(&path_owned, &applied.base_content, &applied.new_content, 4);

            // --- Build details JSON (edit.ts:352-360) ---
            // Upstream: { diff, patch, firstChangedLine }.
            // `firstChangedLine` is `undefined` (omitted by JSON.stringify) when
            // there are no changes — but since we already checked base != new,
            // it will always be Some here. We use skip_serializing_if for
            // parity with JS undefined omission.
            let details = match diff_result.first_changed_line {
                Some(line) => json!({
                    "diff": diff_result.diff,
                    "patch": patch,
                    "firstChangedLine": line,
                }),
                None => json!({
                    "diff": diff_result.diff,
                    "patch": patch,
                }),
            };

            Ok(AgentToolResult {
                content: vec![ToolResultContent::Text(TextContent {
                    text: format!("Successfully replaced {edits_count} block(s) in {path_owned}."),
                    ..Default::default()
                })],
                details,
                ..Default::default()
            })
        })
        .await
    }
}
