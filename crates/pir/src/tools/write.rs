//! Port of `packages/coding-agent/src/core/tools/write.ts` @ pi 0.82.1 (2efa728).
//!
//! The write tool creates or overwrites a file, automatically creating parent
//! directories. All file operations are wrapped in `with_file_mutation_queue`
//! to serialize concurrent operations on the same file.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use pir_agent::types::{AgentTool, AgentToolResult};
use pir_ai::types::{TextContent, ToolResultContent};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::file_mutation_queue::with_file_mutation_queue;
use super::path_utils::resolve_to_cwd;
use crate::tools::ToolContext;

// ---------------------------------------------------------------------------
// Operations trait (write.ts:25-35)
// ---------------------------------------------------------------------------

/// Pluggable operations for the write tool.
///
/// Override these to delegate file writing to remote systems (for example SSH).
/// Port of `WriteOperations` (write.ts:25-30).
#[async_trait]
pub trait WriteOperations: Send + Sync {
    /// Write content to a file as UTF-8.
    async fn write_file(&self, path: &Path, content: &str) -> io::Result<()>;
    /// Create directory recursively.
    async fn mkdir(&self, dir: &Path) -> io::Result<()>;
}

/// Default local filesystem implementation (write.ts:32-35).
struct DefaultWriteOperations;

#[async_trait]
impl WriteOperations for DefaultWriteOperations {
    async fn write_file(&self, path: &Path, content: &str) -> io::Result<()> {
        tokio::fs::write(path, content).await
    }

    async fn mkdir(&self, dir: &Path) -> io::Result<()> {
        tokio::fs::create_dir_all(dir).await
    }
}

// ---------------------------------------------------------------------------
// Options + factory (write.ts:37-40, 181-183)
// ---------------------------------------------------------------------------

/// Options for `create_write_tool`.
///
/// Port of `WriteToolOptions` (write.ts:37-40).
#[derive(Default)]
pub struct WriteToolOptions {
    /// Custom operations for file writing. Default: local filesystem.
    pub operations: Option<Arc<dyn WriteOperations>>,
}

/// Create a write tool bound to the given context.
///
/// Port of `createWriteToolDefinition` + `createWriteTool` (write.ts:181-267).
pub fn create_write_tool(ctx: &ToolContext, options: WriteToolOptions) -> Arc<dyn AgentTool> {
    let ops = options
        .operations
        .unwrap_or_else(|| Arc::new(DefaultWriteOperations));
    let cwd = ctx.cwd.clone();
    Arc::new(WriteTool { cwd, ops })
}

// ---------------------------------------------------------------------------
// Tool implementation
// ---------------------------------------------------------------------------

struct WriteTool {
    cwd: PathBuf,
    ops: Arc<dyn WriteOperations>,
}

/// Static JSON Schema for the write tool parameters (write.ts:14-17).
fn write_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Path to the file to write (relative or absolute)"
            },
            "content": {
                "type": "string",
                "description": "Content to write to the file"
            }
        },
        "required": ["path", "content"]
    })
}

#[async_trait]
impl AgentTool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn label(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories."
    }

    fn parameters(&self) -> &Value {
        WRITE_PARAMS.get_or_init(write_parameters)
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        signal: CancellationToken,
        _on_update: Option<pir_agent::types::AgentToolUpdateCallback>,
    ) -> Result<AgentToolResult, pir_agent::error::AgentError> {
        let path = params
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let content = params
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        let absolute_path = resolve_to_cwd(path, &self.cwd);
        let dir = absolute_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let ops = self.ops.clone();
        let path_owned = path.to_string();

        // The entire file operation runs inside the mutation queue.
        // We do NOT cancel via a task abort listener — that would release the
        // queue while an in-flight filesystem operation may still finish.
        // Instead we check `signal.is_cancelled()` after each await point
        // (write.ts:204-207).
        with_file_mutation_queue(&absolute_path, || async {
            if signal.is_cancelled() {
                return Err(pir_agent::error::AgentError::Message(
                    "Operation aborted".to_string(),
                ));
            }

            // Create parent directories.
            ops.mkdir(&dir).await?;

            if signal.is_cancelled() {
                return Err(pir_agent::error::AgentError::Message(
                    "Operation aborted".to_string(),
                ));
            }

            // Write file contents.
            ops.write_file(&absolute_path, content).await?;

            if signal.is_cancelled() {
                return Err(pir_agent::error::AgentError::Message(
                    "Operation aborted".to_string(),
                ));
            }

            // Success message: `content.length` in JS is the UTF-16 code unit
            // count (string length), NOT the byte count. We replicate this for
            // parity with the upstream text output.
            let byte_count = content.encode_utf16().count();

            Ok(AgentToolResult {
                content: vec![ToolResultContent::Text(TextContent {
                    text: format!("Successfully wrote {byte_count} bytes to {path_owned}"),
                    ..Default::default()
                })],
                details: Value::Null,
                ..Default::default()
            })
        })
        .await
    }
}

/// Lazily initialised static schema value.
static WRITE_PARAMS: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
