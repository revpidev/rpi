//! Port of `packages/ai/src/utils/deferred-tools.ts` @ pi 0.82.1 (2efa728).

use std::collections::HashSet;

use crate::types::{AssistantContent, Context, Message, Tool};

/// Result of [`split_deferred_tools`].
#[derive(Debug, Clone, Default)]
pub struct SplitTools {
    pub immediate: Vec<Tool>,
    /// Deferred tools keyed by normalized name. Insertion order is preserved
    /// (upstream `Map<string, Tool>` iterates in insertion order and adapters
    /// serialize `deferred.values()` into the request payload).
    pub deferred: Vec<(String, Tool)>,
}

/// `splitDeferredTools`: splits current tools into prefix and
/// transcript-loaded definitions. `normalize_name` defaults to identity.
pub fn split_deferred_tools(
    context: &Context,
    enabled: bool,
    normalize_name: impl Fn(&str) -> String,
) -> SplitTools {
    let mut unique_tools: Vec<(String, Tool)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for tool in context.tools.as_deref().unwrap_or(&[]) {
        let name = normalize_name(&tool.name);
        if seen.insert(name.clone()) {
            unique_tools.push((name, tool.clone()));
        }
    }
    if !enabled {
        return SplitTools {
            immediate: unique_tools.into_iter().map(|(_, tool)| tool).collect(),
            deferred: Vec::new(),
        };
    }

    let mut deferred_names: HashSet<String> = HashSet::new();
    let mut used_names: HashSet<String> = HashSet::new();
    for message in &context.messages {
        match message {
            Message::Assistant(assistant) => {
                for block in &assistant.content {
                    if let AssistantContent::ToolCall(call) = block {
                        used_names.insert(normalize_name(&call.name));
                    }
                }
            }
            Message::ToolResult(result) => {
                for name in result.added_tool_names.as_deref().unwrap_or(&[]) {
                    let normalized = normalize_name(name);
                    if !used_names.contains(&normalized) {
                        deferred_names.insert(normalized);
                    }
                }
            }
            Message::User(_) => {}
        }
    }

    let mut immediate = Vec::new();
    let mut deferred = Vec::new();
    for (name, tool) in unique_tools {
        if deferred_names.contains(&name) {
            deferred.push((name, tool));
        } else {
            immediate.push(tool);
        }
    }
    SplitTools {
        immediate,
        deferred,
    }
}

/// `splitDeferredTools` with the identity normalizer.
pub fn split_deferred_tools_identity(context: &Context, enabled: bool) -> SplitTools {
    split_deferred_tools(context, enabled, |name| name.to_owned())
}
