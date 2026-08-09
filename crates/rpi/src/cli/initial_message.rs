//! Combine stdin content, `@file` text, and the first CLI message into the
//! initial prompt for non-interactive mode.
//!
//! Port of `packages/coding-agent/src/cli/initial-message.ts` @ pi 0.82.1
//! (2efa728).

use rpi_ai::types::ImageContent;

/// `InitialMessageResult` (initial-message.ts:11-14).
#[derive(Debug, Default, PartialEq)]
pub struct InitialMessage {
    pub initial_message: Option<String>,
    pub initial_images: Option<Vec<ImageContent>>,
}

/// `buildInitialMessage` (initial-message.ts:21-43).
///
/// `messages` is the parsed args message list; the first message (if any) is
/// shifted out and merged after stdin content and `@file` text.
pub fn build_initial_message(
    messages: &mut Vec<String>,
    file_text: Option<&str>,
    file_images: Option<Vec<ImageContent>>,
    stdin_content: Option<&str>,
) -> InitialMessage {
    let mut parts: Vec<String> = Vec::new();
    if let Some(stdin) = stdin_content {
        parts.push(stdin.to_owned());
    }
    if let Some(file_text) = file_text {
        if !file_text.is_empty() {
            parts.push(file_text.to_owned());
        }
    }
    if !messages.is_empty() {
        parts.push(messages.remove(0));
    }

    InitialMessage {
        initial_message: if parts.is_empty() {
            None
        } else {
            Some(parts.join(""))
        },
        initial_images: file_images.filter(|images| !images.is_empty()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img() -> ImageContent {
        ImageContent {
            data: "AAAA".to_owned(),
            mime_type: "image/png".to_owned(),
        }
    }

    #[test]
    fn test_combines_stdin_file_and_first_message_in_order() {
        let mut messages = vec!["first".to_owned(), "second".to_owned()];
        let result = build_initial_message(&mut messages, Some("<file/>"), None, Some("stdin"));
        assert_eq!(result.initial_message.as_deref(), Some("stdin<file/>first"));
        assert_eq!(messages, vec!["second".to_owned()]);
    }

    #[test]
    fn test_empty_parts_yield_none() {
        let mut messages = Vec::new();
        let result = build_initial_message(&mut messages, None, None, None);
        assert_eq!(result.initial_message, None);
        assert_eq!(result.initial_images, None);
    }

    #[test]
    fn test_empty_file_text_is_falsy_upstream() {
        // `if (fileText)` — empty string is falsy and skipped.
        let mut messages = vec!["msg".to_owned()];
        let result = build_initial_message(&mut messages, Some(""), None, None);
        assert_eq!(result.initial_message.as_deref(), Some("msg"));
    }

    #[test]
    fn test_empty_image_list_becomes_none() {
        let mut messages = Vec::new();
        let result = build_initial_message(&mut messages, Some("x"), Some(Vec::new()), None);
        assert_eq!(result.initial_images, None);
    }

    #[test]
    fn test_images_pass_through() {
        let mut messages = Vec::new();
        let result = build_initial_message(&mut messages, Some("x"), Some(vec![img()]), None);
        assert_eq!(result.initial_images.as_ref().map(Vec::len), Some(1));
    }
}
