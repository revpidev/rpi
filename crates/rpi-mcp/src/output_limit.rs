use std::sync::Arc;

use crate::content::LlmContent;

pub const MCP_OUTPUT_MAX_BYTES: usize = 20 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpOutputData {
    Text(String),
    Binary(Vec<u8>),
}

pub type McpOutputSaver = Arc<dyn Fn(McpOutputData, &str) -> Result<String, String> + Send + Sync>;

#[derive(Debug, Clone, PartialEq)]
pub struct LimitedMcpContent {
    pub content: Vec<LlmContent>,
    pub full_output_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MiddleTruncation {
    content: String,
    total_bytes: usize,
    total_lines: usize,
    removed_chars: usize,
}

fn truncate_middle(text: &str, max_bytes: usize) -> Option<MiddleTruncation> {
    let bytes = text.as_bytes();
    if bytes.len() <= max_bytes {
        return None;
    }
    let is_boundary = |index: usize| index >= bytes.len() || (bytes[index] & 0xc0) != 0x80;
    let mut head_end = max_bytes / 2;
    while head_end > 0 && !is_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = bytes.len() - (max_bytes - max_bytes / 2);
    while tail_start < bytes.len() && !is_boundary(tail_start) {
        tail_start += 1;
    }
    let head = String::from_utf8_lossy(&bytes[..head_end]).into_owned();
    let tail = String::from_utf8_lossy(&bytes[tail_start..]).into_owned();
    let removed = String::from_utf8_lossy(&bytes[head_end..tail_start]).into_owned();
    let removed_chars = removed.chars().count();
    let total_lines = if text.is_empty() {
        0
    } else {
        text.split('\n').count() - usize::from(text.ends_with('\n'))
    };
    Some(MiddleTruncation {
        content: format!("{head}…{removed_chars} chars truncated…{tail}"),
        total_bytes: bytes.len(),
        total_lines,
        removed_chars,
    })
}

pub fn limit_mcp_content(
    content: Vec<LlmContent>,
    save_output: Option<&McpOutputSaver>,
) -> LimitedMcpContent {
    let combined = content
        .iter()
        .filter_map(LlmContent::as_text)
        .collect::<Vec<_>>()
        .join("\n");
    let Some(truncation) = truncate_middle(&combined, MCP_OUTPUT_MAX_BYTES) else {
        return LimitedMcpContent {
            content,
            full_output_path: None,
        };
    };
    let mut full_output_path = None;
    let mut where_line = String::new();
    if let Some(save_output) = save_output {
        match save_output(McpOutputData::Text(combined), ".txt") {
            Ok(path) => {
                where_line = format!("\n\n[Full output: {path} (read it with offset/limit)]");
                full_output_path = Some(path);
            }
            Err(error) => {
                where_line = format!("\n\n[Could not save the full output: {error}]");
            }
        }
    }
    let tokens = truncation.total_bytes.div_ceil(4);
    let text = format!(
        "Warning: truncated output (original token count: {tokens})\nTotal output lines: {}\n\n{}{}",
        truncation.total_lines, truncation.content, where_line
    );
    let mut limited = vec![LlmContent::text(text)];
    limited.extend(
        content
            .into_iter()
            .filter(|block| matches!(block, LlmContent::Image { .. })),
    );
    LimitedMcpContent {
        content: limited,
        full_output_path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn cuts_multibyte_text_in_the_middle_and_saves_full_output() {
        let text = format!("{}end", "é".repeat(20_000));
        let saved = Arc::new(Mutex::new(Vec::new()));
        let saved_callback = saved.clone();
        let saver: McpOutputSaver = Arc::new(move |data, extension| {
            let McpOutputData::Text(text) = data else {
                return Err("expected text".to_owned());
            };
            saved_callback
                .lock()
                .unwrap()
                .push((text, extension.to_owned()));
            Ok("/tmp/full.txt".to_owned())
        });
        let limited = limit_mcp_content(vec![LlmContent::text(text)], Some(&saver));
        let LlmContent::Text { text } = &limited.content[0] else {
            panic!("text")
        };
        assert!(text.contains("chars truncated"));
        assert!(text.ends_with("/tmp/full.txt (read it with offset/limit)]"));
        assert!(!text.contains('\u{fffd}'));
        assert_eq!(limited.full_output_path.as_deref(), Some("/tmp/full.txt"));
        assert_eq!(saved.lock().unwrap().len(), 1);
    }

    #[test]
    fn preserves_content_within_the_limit() {
        let content = vec![
            LlmContent::text("small"),
            LlmContent::image("aa", "image/png"),
        ];
        let limited = limit_mcp_content(content.clone(), None);
        assert_eq!(limited.content, content);
        assert_eq!(limited.full_output_path, None);
    }

    #[test]
    fn truncation_counts_removed_unicode_scalars() {
        let truncation =
            truncate_middle(&format!("{}end", "é".repeat(2_000)), 1001).expect("truncated");
        assert!(truncation.content.ends_with("end"));
        assert!(!truncation.content.contains('\u{fffd}'));
        assert!(truncation.removed_chars > 0);
    }
}
