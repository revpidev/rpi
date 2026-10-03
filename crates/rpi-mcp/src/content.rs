//! Content blocks and the LLM-facing conversion (port of
//! `packages/mcp/src/protocol/content.ts` @ a13d35a74).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::CallToolResult;

/// `LlmContent` (content.ts:80): the text/image shapes the model APIs accept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum LlmContent {
    Text {
        text: String,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

impl LlmContent {
    pub fn text(text: impl Into<String>) -> Self {
        LlmContent::Text { text: text.into() }
    }

    pub fn image(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        LlmContent::Image {
            data: data.into(),
            mime_type: mime_type.into(),
        }
    }

    /// Plain text of this block (`None` for images), for joins and checks.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            LlmContent::Text { text } => Some(text),
            LlmContent::Image { .. } => None,
        }
    }
}

/// `blockToLlmContent` (content.ts:84): text and images pass through,
/// embedded text resources become text, embedded image resources become
/// images, and other blocks become a short text placeholder.
pub fn block_to_llm_content(block: &Value) -> LlmContent {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => LlmContent::text(
            block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        ),
        Some("image") => LlmContent::image(
            block
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            block
                .get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        ),
        Some("audio") => LlmContent::text(format!(
            "[audio {} omitted]",
            block
                .get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or_default()
        )),
        Some("resource_link") => LlmContent::text(format!(
            "{}: {}",
            block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            block.get("uri").and_then(Value::as_str).unwrap_or_default()
        )),
        Some("resource") => {
            let resource = block.get("resource").cloned().unwrap_or(Value::Null);
            if let Some(text) = resource.get("text").and_then(Value::as_str) {
                return LlmContent::text(text);
            }
            let mime_type = resource.get("mimeType").and_then(Value::as_str);
            let blob = resource
                .get("blob")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if mime_type.is_some_and(|value| value.starts_with("image/")) {
                return LlmContent::image(blob, mime_type.unwrap_or_default());
            }
            LlmContent::text(format!(
                "[binary resource {} ({} type) omitted]",
                resource
                    .get("uri")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                mime_type.unwrap_or("unknown")
            ))
        }
        Some(other) => LlmContent::text(format!("[unsupported MCP content {other}]")),
        None => LlmContent::text("[unsupported MCP content ]"),
    }
}

/// `toLlmContent` (content.ts:120): a result without content blocks but with
/// `structuredContent` becomes its JSON (pretty-printed, like upstream).
pub fn to_llm_content(result: &CallToolResult) -> Vec<LlmContent> {
    let mut content: Vec<LlmContent> = result.content.iter().map(block_to_llm_content).collect();
    if content.is_empty()
        && let Some(structured) = &result.structured_content
    {
        content.push(LlmContent::text(
            serde_json::to_string_pretty(structured).unwrap_or_default(),
        ));
    }
    content
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn text_and_image_pass_through() {
        assert_eq!(
            block_to_llm_content(&json!({"type": "text", "text": "hi"})),
            LlmContent::Text {
                text: "hi".to_owned()
            }
        );
        assert_eq!(
            block_to_llm_content(&json!({"type": "image", "data": "aa", "mimeType": "image/png"})),
            LlmContent::Image {
                data: "aa".to_owned(),
                mime_type: "image/png".to_owned()
            }
        );
    }

    #[test]
    fn placeholders_match_upstream_wording() {
        assert_eq!(
            block_to_llm_content(&json!({"type": "audio", "data": "x", "mimeType": "audio/mp3"})),
            LlmContent::Text {
                text: "[audio audio/mp3 omitted]".to_owned()
            }
        );
        assert_eq!(
            block_to_llm_content(&json!({"type": "resource_link", "uri": "u://x", "name": "n"})),
            LlmContent::Text {
                text: "n: u://x".to_owned()
            }
        );
        assert_eq!(
            block_to_llm_content(
                &json!({"type": "resource", "resource": {"uri": "u://x", "blob": "aa", "mimeType": "application/pdf"}})
            ),
            LlmContent::Text {
                text: "[binary resource u://x (application/pdf type) omitted]".to_owned()
            }
        );
        assert_eq!(
            block_to_llm_content(
                &json!({"type": "resource", "resource": {"uri": "u://x", "blob": "aa", "mimeType": "image/png"}})
            ),
            LlmContent::Image {
                data: "aa".to_owned(),
                mime_type: "image/png".to_owned()
            }
        );
    }

    #[test]
    fn structured_content_fallback_is_pretty_json() {
        let result = CallToolResult {
            content: vec![],
            structured_content: Some(json!({"a": 1})),
            is_error: None,
            meta: None,
        };
        assert_eq!(
            to_llm_content(&result),
            vec![LlmContent::Text {
                text: "{\n  \"a\": 1\n}".to_owned()
            }]
        );
        let empty = CallToolResult {
            content: vec![],
            structured_content: None,
            is_error: None,
            meta: None,
        };
        assert!(to_llm_content(&empty).is_empty());
    }
}
