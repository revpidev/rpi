//! Built-in tool renderers (T17) — port of the `renderCall`/`renderResult`
//! hooks carried by the upstream tool definitions in
//! `packages/coding-agent/src/core/tools/*.ts` @ pi 0.82.1 (2efa728).
//!
//! [`builtin_tool_definition`] is the render-only port of
//! `createAllToolDefinitions(cwd)[toolName]` (tool-execution.ts:57): one
//! [`ToolDefinition`] per built-in tool, looked up by tool name. Renderer
//! state lives in the component's `RendererStateSlot` (typed per tool), so
//! the registered definitions are shared, stateless singletons.

pub mod bash;
pub mod edit;
pub mod find;
pub mod grep;
pub mod ls;
pub mod read;
pub mod render_utils;
pub mod write;

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use super::components::tool_execution::ToolDefinition;

static REGISTRY: OnceLock<HashMap<&'static str, Arc<dyn ToolDefinition>>> = OnceLock::new();

/// `createAllToolDefinitions(cwd)[toolName]` (tool-execution.ts:57): the
/// built-in renderer for `tool_name`, if the tool has one.
pub fn builtin_tool_definition(tool_name: &str) -> Option<Arc<dyn ToolDefinition>> {
    let registry = REGISTRY.get_or_init(|| {
        let mut map: HashMap<&'static str, Arc<dyn ToolDefinition>> = HashMap::new();
        map.insert("bash", Arc::new(bash::BashToolRenderer));
        map.insert("edit", Arc::new(edit::EditToolRenderer));
        map.insert("find", Arc::new(find::FindToolRenderer));
        map.insert("grep", Arc::new(grep::GrepToolRenderer));
        map.insert("ls", Arc::new(ls::LsToolRenderer));
        map.insert("read", Arc::new(read::ReadToolRenderer));
        map.insert("write", Arc::new(write::WriteToolRenderer));
        map
    });
    registry.get(tool_name).cloned()
}
