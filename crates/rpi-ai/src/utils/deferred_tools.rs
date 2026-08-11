//! Port of `packages/ai/src/utils/deferred-tools.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences: none.

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
///
/// Duplicate normalized names resolve like upstream `Map.set`: the **last**
/// tool wins while the first occurrence keeps its insertion position.
pub fn split_deferred_tools(
    context: &Context,
    enabled: bool,
    normalize_name: impl Fn(&str) -> String,
) -> SplitTools {
    let mut unique_tools: Vec<(String, Tool)> = Vec::new();
    for tool in context.tools.as_deref().unwrap_or(&[]) {
        let name = normalize_name(&tool.name);
        match unique_tools
            .iter_mut()
            .find(|(existing, _)| *existing == name)
        {
            // Upstream `uniqueTools.set(name, tool)`: a later duplicate
            // overwrites the value but keeps the first insertion order.
            Some((_, existing_tool)) => *existing_tool = tool.clone(),
            None => unique_tools.push((name, tool.clone())),
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

#[cfg(test)]
mod tests {
    use serde_json::{json, Map};

    use super::*;
    use crate::types::{
        AssistantContent, AssistantMessage, AssistantRole, StopReason, TextContent, ToolCall,
        ToolResultContent, ToolResultMessage, ToolResultRole, Usage, UserContent, UserMessage,
        UserRole,
    };

    fn tool(name: &str) -> Tool {
        serde_json::from_value(json!({
            "name": name, "description": "d",
            "parameters": {"type": "object", "properties": {"x": {"type": "string"}}, "required": ["x"]}
        }))
        .expect("tool")
    }

    fn context(messages: Vec<Message>, tools: Vec<Tool>) -> Context {
        Context {
            system_prompt: None,
            messages,
            tools: Some(tools),
        }
    }

    fn user() -> Message {
        Message::User(UserMessage {
            role: UserRole::User,
            content: UserContent::Text("hi".to_owned()),
            timestamp: 0,
        })
    }

    fn assistant_call(name: &str) -> Message {
        Message::Assistant(AssistantMessage {
            role: AssistantRole::Assistant,
            content: vec![AssistantContent::ToolCall(ToolCall {
                id: "call_1".to_owned(),
                name: name.to_owned(),
                arguments: Map::new(),
                thought_signature: None,
                namespace: None,
            })],
            api: crate::types::ApiKind::from(crate::types::ApiKind::ANTHROPIC_MESSAGES),
            provider: "anthropic".to_owned(),
            model: "claude-sonnet-4-5".to_owned(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            timestamp: 1,
            deferred: None,
            end_turn: None,
            raw_stop_reason: None,
        })
    }

    fn tool_result_marker(added: &[&str]) -> Message {
        Message::ToolResult(ToolResultMessage {
            role: ToolResultRole::ToolResult,
            tool_call_id: "call_1".to_owned(),
            tool_name: "base".to_owned(),
            content: vec![ToolResultContent::Text(TextContent {
                text: "ok".to_owned(),
                text_signature: None,
            })],
            details: None,
            usage: None,
            added_tool_names: Some(added.iter().map(|s| s.to_string()).collect()),
            is_error: false,
            timestamp: 2,
        })
    }

    /// OAuth-style canonical casing: first letter uppercased, mimicking
    /// `toClaudeCodeName` (read → Read, Read → Read).
    fn capitalize(name: &str) -> String {
        let mut chars = name.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            None => String::new(),
        }
    }

    #[test]
    fn test_disabled_returns_all_immediate() {
        let ctx = context(
            vec![
                user(),
                assistant_call("base"),
                tool_result_marker(&["late"]),
            ],
            vec![tool("base"), tool("late")],
        );
        let split = split_deferred_tools_identity(&ctx, false);
        assert_eq!(split.immediate.len(), 2);
        assert!(split.deferred.is_empty());
    }

    #[test]
    fn test_marked_tool_moves_to_deferred_keeping_order() {
        let ctx = context(
            vec![
                user(),
                assistant_call("base"),
                tool_result_marker(&["late"]),
            ],
            vec![tool("base"), tool("late")],
        );
        let split = split_deferred_tools_identity(&ctx, true);
        assert_eq!(split.immediate.len(), 1);
        assert_eq!(split.immediate[0].name, "base");
        assert_eq!(split.deferred.len(), 1);
        assert_eq!(split.deferred[0].0, "late");
        assert_eq!(split.deferred[0].1.name, "late");
    }

    #[test]
    fn test_used_before_marker_stays_immediate() {
        // Upstream "keeps a tool immediate when it was used before its marker".
        let ctx = context(
            vec![
                user(),
                assistant_call("late"),
                tool_result_marker(&["late"]),
            ],
            vec![tool("base"), tool("late")],
        );
        let split = split_deferred_tools_identity(&ctx, true);
        assert_eq!(split.immediate.len(), 2);
        assert!(split.deferred.is_empty());
    }

    #[test]
    fn test_oauth_canonicalized_marker_matches_tool() {
        // Upstream "matches OAuth-canonicalized markers to active tools":
        // the "Read" marker normalizes to the same key as the "read" tool.
        let ctx = context(
            vec![
                user(),
                assistant_call("base"),
                tool_result_marker(&["Read"]),
            ],
            vec![tool("base"), tool("read")],
        );
        let split = split_deferred_tools(&ctx, true, capitalize);
        assert_eq!(split.immediate.len(), 1);
        assert_eq!(split.immediate[0].name, "base");
        assert_eq!(split.deferred.len(), 1);
        assert_eq!(split.deferred[0].0, "Read");
        assert_eq!(split.deferred[0].1.name, "read");
    }

    #[test]
    fn test_oauth_used_name_keeps_tool_immediate() {
        // Upstream "normalizes OAuth names before checking prior tool usage":
        // a call to "Read" satisfies the "read" marker, so nothing defers.
        let ctx = context(
            vec![
                user(),
                assistant_call("Read"),
                tool_result_marker(&["read"]),
            ],
            vec![tool("base"), tool("read")],
        );
        let split = split_deferred_tools(&ctx, true, capitalize);
        assert_eq!(split.immediate.len(), 2);
        assert!(split.deferred.is_empty());
    }

    #[test]
    fn test_dedupes_after_canonicalization_last_wins() {
        // Upstream "deduplicates active tools after OAuth canonicalization":
        // JS `Map.set` keeps the LAST definition for a normalized key.
        let mut canonical = tool("Read");
        canonical.description = "Canonical definition".to_owned();
        let ctx = context(vec![user()], vec![tool("read"), canonical]);
        let split = split_deferred_tools(&ctx, true, capitalize);
        assert_eq!(split.immediate.len(), 1);
        assert_eq!(split.immediate[0].name, "Read");
        assert_eq!(split.immediate[0].description, "Canonical definition");
        assert!(split.deferred.is_empty());
    }

    #[test]
    fn test_marker_for_missing_tool_ignored() {
        // Upstream "does not resurrect a marked tool missing from
        // Context.tools": markers only defer tools that are actually present.
        let ctx = context(
            vec![
                user(),
                assistant_call("base"),
                tool_result_marker(&["ghost"]),
            ],
            vec![tool("base")],
        );
        let split = split_deferred_tools_identity(&ctx, true);
        assert_eq!(split.immediate.len(), 1);
        assert_eq!(split.immediate[0].name, "base");
        assert!(split.deferred.is_empty());
    }

    #[test]
    fn test_no_tools() {
        let ctx = context(vec![user()], vec![]);
        let split = split_deferred_tools_identity(&ctx, true);
        assert!(split.immediate.is_empty());
        assert!(split.deferred.is_empty());
    }
}
