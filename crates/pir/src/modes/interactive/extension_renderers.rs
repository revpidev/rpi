//! Extension renderer plumbing for the TUI (T15 W4): bridges the host's
//! JSON-descriptor renderers (message/entry/tool) into the TUI component
//! hooks (`CustomMessageComponent` / `CustomEntryComponent` /
//! `ToolExecutionComponent`).
//!
//! Renderer failures degrade to the default rendering (`None`), matching
//! upstream's per-renderer try/catch fallback (custom-message.ts:69-85,
//! tool-execution.ts:14-16).

use std::sync::Arc;

use pir_ext_host::host::NativeExtensionHost;
use pir_ext_host::types as ext;
use pir_tui::tui::Component;

use super::component_tree::component_from_tree;
use super::components::custom_entry::EntryRenderer;
use super::components::custom_message::MessageRenderer;
use super::components::tool_execution::{
    RenderShell, ResultRenderOptions, ToolDefinition, ToolResultState,
};
use crate::core::agent_session::AgentSession;
use crate::core::extension_host_adapter::host_of_runner;
use crate::core::themes::Theme;
use pir_agent::session::CustomEntry;

fn host_of(session: &AgentSession) -> Option<Arc<NativeExtensionHost>> {
    host_of_runner(&session.extension_runner())
}

/// `getMessageRenderer(customType)` → TUI `MessageRenderer`
/// (interactive-mode.ts message render path).
pub fn host_message_renderer(session: &AgentSession, custom_type: &str) -> Option<MessageRenderer> {
    let render = host_of(session)?.get_message_renderer(custom_type)?;
    Some(Box::new(move |message, options, theme| {
        let value = serde_json::to_value(message).ok()?;
        let tree = render(
            value,
            ext::MessageRenderOptions {
                expanded: options.expanded,
                output_pad: options.output_pad as u32,
            },
        )
        .ok()??;
        Some(component_from_tree(&tree, &Arc::new(theme.clone())))
    }))
}

/// `getEntryRenderer(customType)` → TUI `EntryRenderer`.
pub fn host_entry_renderer(session: &AgentSession, custom_type: &str) -> Option<EntryRenderer> {
    let render = host_of(session)?.get_entry_renderer(custom_type)?;
    Some(Box::new(move |entry: &CustomEntry, options, theme| {
        let value = serde_json::to_value(entry).ok()?;
        let tree = render(
            value,
            ext::EntryRenderOptions {
                expanded: options.expanded,
            },
        )
        .ok()??;
        Some(component_from_tree(&tree, &Arc::new(theme.clone())))
    }))
}

/// `getRegisteredToolDefinition` (interactive-mode.ts:2944): the render
/// hooks of the (possibly extension-overridden) tool definition. Overrides
/// without render hooks return `None`, so the built-in rendering by tool
/// name applies unchanged — the render-slot inheritance.
pub fn host_tool_definition(
    session: &AgentSession,
    tool_name: &str,
) -> Option<Arc<dyn ToolDefinition>> {
    let definition = host_of(session)?.get_tool_definition(tool_name)?;
    if definition.render_call.is_none() && definition.render_result.is_none() {
        return None;
    }
    Some(Arc::new(HostToolRenderDefinition {
        render_call: definition.render_call,
        render_result: definition.render_result,
        render_shell: definition.render_shell,
    }))
}

struct HostToolRenderDefinition {
    render_call: Option<ext::RenderCallFn>,
    render_result: Option<ext::RenderResultFn>,
    render_shell: Option<String>,
}

fn to_render_context(
    context: &super::components::tool_execution::ToolRenderContext,
) -> ext::ToolRenderContext {
    ext::ToolRenderContext {
        args: context.args.clone(),
        tool_call_id: context.tool_call_id.clone(),
        cwd: context.cwd.clone(),
        execution_started: context.execution_started,
        args_complete: context.args_complete,
        is_partial: context.is_partial,
        expanded: context.expanded,
        show_images: context.show_images,
        is_error: context.is_error,
    }
}

impl ToolDefinition for HostToolRenderDefinition {
    fn render_call(
        &self,
        args: &serde_json::Value,
        theme: &Theme,
        context: &super::components::tool_execution::ToolRenderContext,
    ) -> Option<Box<dyn Component>> {
        let render = self.render_call.as_ref()?;
        let mut ext_context = to_render_context(context);
        ext_context.args = args.clone();
        let tree = render(ext_context).ok()?;
        Some(component_from_tree(&tree, &Arc::new(theme.clone())))
    }

    fn render_result(
        &self,
        result: &ToolResultState,
        options: ResultRenderOptions,
        theme: &Theme,
        context: &super::components::tool_execution::ToolRenderContext,
    ) -> Option<Box<dyn Component>> {
        let render = self.render_result.as_ref()?;
        let content = result
            .content
            .iter()
            .map(|block| {
                serde_json::json!({
                    "type": block.kind,
                    "text": block.text,
                    "data": block.data,
                    "mimeType": block.mime_type,
                })
            })
            .collect::<Vec<_>>();
        let agent_result = pir_agent::types::AgentToolResult {
            content: serde_json::from_value(serde_json::Value::Array(content)).unwrap_or_default(),
            details: result.details.clone().unwrap_or(serde_json::Value::Null),
            usage: None,
            added_tool_names: None,
            terminate: None,
        };
        let tree = render(
            agent_result,
            ext::ToolRenderResultOptions {
                expanded: options.expanded,
                is_partial: options.is_partial,
            },
            to_render_context(context),
        )
        .ok()?;
        Some(component_from_tree(&tree, &Arc::new(theme.clone())))
    }

    fn render_shell(&self) -> Option<RenderShell> {
        // Upstream `renderShell?: "default" | "self"` — `undefined` (and any
        // unrecognized value) means "not provided", an explicit `"default"`
        // is a real value that wins over the built-in shell.
        match self.render_shell.as_deref() {
            Some("self") => Some(RenderShell::Self_),
            Some("default") => Some(RenderShell::Default),
            _ => None,
        }
    }
}
