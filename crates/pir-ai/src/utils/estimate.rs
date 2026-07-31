//! Port of `packages/ai/src/utils/estimate.ts` @ pi 0.82.1 (2efa728).
//!
//! chars/4 heuristic token estimation with usage-anchor trailing estimation.
//! Do not "improve" — the constants are pinned behavior (ADR-0002 §4).
//!
//! Intentional differences: JS `String.length` counts UTF-16 code units;
//! `chars` here counts `char`s. For BMP text they are identical; for astral
//! characters the JS estimate is up to 2x higher per character. This matches
//! the D-003 faux-provider decision (chars/4, BMP-equivalent).

use crate::types::{
    AssistantContent, Context, ImageContent, Message, TextContent, Tool, ToolResultContent, Usage,
    UserContent, UserContentBlock,
};

const CHARS_PER_TOKEN: usize = 4;
const ESTIMATED_IMAGE_CHARS: usize = 4800;

/// `calculateContextTokens`: `totalTokens` when set, else the component sum.
pub fn calculate_context_tokens(usage: &Usage) -> u64 {
    if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        usage.input + usage.output + usage.cache_read + usage.cache_write
    }
}

fn char_len(text: &str) -> usize {
    text.chars().count()
}

fn safe_json_stringify<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "[unserializable]".to_owned())
}

/// Estimated total context tokens plus the usage-anchor breakdown.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextUsageEstimate {
    /// Estimated total context tokens.
    pub tokens: u64,
    /// Tokens reported by the most recent applicable assistant usage block.
    pub usage_tokens: u64,
    /// Estimated tokens after the most recent applicable assistant usage block.
    pub trailing_tokens: u64,
    /// Index of the applicable message that provided usage, or `None`.
    pub last_usage_index: Option<usize>,
}

fn estimate_text_and_image_chars_text(text: &str) -> usize {
    char_len(text)
}

fn estimate_block_chars(text: &TextContent) -> usize {
    char_len(&text.text)
}

fn image_chars(_image: &ImageContent) -> usize {
    ESTIMATED_IMAGE_CHARS
}

pub fn estimate_text_tokens(text: &str) -> u64 {
    (char_len(text) as u64).div_ceil(CHARS_PER_TOKEN as u64)
}

pub fn estimate_message_tokens(message: &Message) -> u64 {
    let chars: usize = match message {
        Message::User(user) => match &user.content {
            UserContent::Text(text) => estimate_text_and_image_chars_text(text),
            UserContent::Blocks(blocks) => blocks
                .iter()
                .map(|block| match block {
                    UserContentBlock::Text(text) => estimate_block_chars(text),
                    UserContentBlock::Image(image) => image_chars(image),
                })
                .sum(),
        },
        Message::ToolResult(result) => result
            .content
            .iter()
            .map(|block| match block {
                ToolResultContent::Text(text) => estimate_block_chars(text),
                ToolResultContent::Image(image) => image_chars(image),
            })
            .sum(),
        Message::Assistant(assistant) => assistant
            .content
            .iter()
            .map(|block| match block {
                AssistantContent::Text(text) => char_len(&text.text),
                AssistantContent::Thinking(thinking) => char_len(&thinking.thinking),
                AssistantContent::ToolCall(call) => {
                    char_len(&call.name) + char_len(&safe_json_stringify(&call.arguments))
                }
            })
            .sum(),
    };
    (chars as u64).div_ceil(CHARS_PER_TOKEN as u64)
}

fn last_assistant_usage_info(messages: &[Message]) -> Option<(&Usage, usize)> {
    let mut latest_prefix_timestamp = i64::MIN;
    let mut usage_info: Option<(&Usage, usize)> = None;

    for (i, message) in messages.iter().enumerate() {
        if let Message::Assistant(assistant) = message {
            // A newer prefix message was inserted after this response (for
            // example, a compaction summary), so its usage cannot describe
            // the current prefix.
            let usage_applies_to_prefix = assistant.timestamp >= latest_prefix_timestamp;
            if usage_applies_to_prefix
                && assistant.stop_reason != crate::types::StopReason::Aborted
                && assistant.stop_reason != crate::types::StopReason::Error
                && calculate_context_tokens(&assistant.usage) > 0
            {
                usage_info = Some((&assistant.usage, i));
            }
        }
        latest_prefix_timestamp = latest_prefix_timestamp.max(message_timestamp(message));
    }

    usage_info
}

fn message_timestamp(message: &Message) -> i64 {
    match message {
        Message::User(m) => m.timestamp,
        Message::Assistant(m) => m.timestamp,
        Message::ToolResult(m) => m.timestamp,
    }
}

fn estimate_messages(messages: &[Message]) -> ContextUsageEstimate {
    if let Some((usage, index)) = last_assistant_usage_info(messages) {
        let usage_tokens = calculate_context_tokens(usage);
        let trailing_tokens: u64 = messages[index + 1..]
            .iter()
            .map(estimate_message_tokens)
            .sum();
        return ContextUsageEstimate {
            tokens: usage_tokens + trailing_tokens,
            usage_tokens,
            trailing_tokens,
            last_usage_index: Some(index),
        };
    }

    let tokens: u64 = messages.iter().map(estimate_message_tokens).sum();
    ContextUsageEstimate {
        tokens,
        usage_tokens: 0,
        trailing_tokens: tokens,
        last_usage_index: None,
    }
}

fn estimate_tools_tokens(tools: Option<&[Tool]>) -> u64 {
    match tools {
        Some(tools) if !tools.is_empty() => estimate_text_tokens(&safe_json_stringify(&tools)),
        _ => 0,
    }
}

/// `estimateContextTokens` for a bare message slice (upstream array overload).
pub fn estimate_messages_tokens(messages: &[Message]) -> ContextUsageEstimate {
    estimate_messages(messages)
}

/// `estimateContextTokens` for a full [`Context`].
pub fn estimate_context_tokens(context: &Context) -> ContextUsageEstimate {
    let estimate = estimate_messages(&context.messages);

    if let Some(last_usage_index) = estimate.last_usage_index {
        let added_names: std::collections::HashSet<&str> = context.messages[last_usage_index + 1..]
            .iter()
            .filter_map(|message| match message {
                Message::ToolResult(result) => result.added_tool_names.as_deref(),
                _ => None,
            })
            .flatten()
            .map(String::as_str)
            .collect();
        let added_tools: Vec<Tool> = context
            .tools
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .filter(|tool| added_names.contains(tool.name.as_str()))
            .cloned()
            .collect();
        let added_tool_tokens = estimate_tools_tokens(Some(&added_tools));
        return ContextUsageEstimate {
            tokens: estimate.tokens + added_tool_tokens,
            usage_tokens: estimate.usage_tokens,
            trailing_tokens: estimate.trailing_tokens + added_tool_tokens,
            last_usage_index: Some(last_usage_index),
        };
    }

    let prefix_tokens = context
        .system_prompt
        .as_deref()
        .map(estimate_text_tokens)
        .unwrap_or(0)
        + estimate_tools_tokens(context.tools.as_deref());

    ContextUsageEstimate {
        tokens: estimate.tokens + prefix_tokens,
        usage_tokens: estimate.usage_tokens,
        trailing_tokens: estimate.trailing_tokens + prefix_tokens,
        last_usage_index: None,
    }
}
