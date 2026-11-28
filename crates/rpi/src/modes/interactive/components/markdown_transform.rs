//! Port of
//! `packages/coding-agent/src/modes/interactive/components/markdown-transform.ts`
//! @ pi 4181f66 (714978bf5).
//!
//! Adapts the ext-host `MarkdownTransformerFn` chain (per transformer:
//! `(markdown, MarkdownTransformContext) -> String`) onto the width-aware
//! rpi-tui `MarkdownTransformFn` shape (`(markdown, availableWidth) ->
//! String`). `messageType` / `isStreaming` are captured when the transform is
//! created; `availableWidth` is filled in per render call.
//!
//! Intentional differences:
//! - Upstream wraps each transformer in try/catch (markdown-transform.ts:20-26);
//!   the Rust chain cannot catch panics, so the isolation contract lives at the
//!   WASM boundary: a failing guest transformer returns the input unchanged
//!   (host_call.rs `registerMarkdownTransformer`), and non-string returns are
//!   already normalized there too.

use std::sync::Arc;

use rpi_ext_host::types::{MarkdownTransformContext, MarkdownTransformerFn};
use rpi_tui::components::markdown::MarkdownTransformFn;

/// `createMarkdownTransform` (markdown-transform.ts:3-9): build the
/// width-aware transform applied by `MarkdownOptions.transform`, or `None`
/// when no transformer is registered (the Markdown then renders unchanged —
/// upstream always returns a function, but the extension chain is empty).
pub fn create_markdown_transform(
    message_type: &str,
    is_streaming: impl Fn() -> bool + Send + Sync + 'static,
    transformers: Vec<MarkdownTransformerFn>,
) -> Option<MarkdownTransformFn> {
    create_markdown_transform_with_metadata(message_type, is_streaming, transformers, None, None)
}

/// `isStreaming` is read per transform call (upstream 0f21a3319), so a
/// reused Markdown component sees a stream end without being rebuilt.
pub fn create_markdown_transform_with_metadata(
    message_type: &str,
    is_streaming: impl Fn() -> bool + Send + Sync + 'static,
    transformers: Vec<MarkdownTransformerFn>,
    message_id: Option<String>,
    timestamp: Option<String>,
) -> Option<MarkdownTransformFn> {
    if transformers.is_empty() {
        return None;
    }
    let message_type = message_type.to_owned();
    let is_streaming = Arc::new(is_streaming);
    Some(Arc::new(move |markdown: &str, available_width: usize| {
        let context = MarkdownTransformContext {
            message_type: message_type.clone(),
            is_streaming: is_streaming(),
            available_width,
            message_id: message_id.clone(),
            timestamp: timestamp.clone(),
        };
        apply_markdown_transformers(markdown, &context, &transformers)
    }))
}

pub fn create_header_prefixed_markdown_transform(
    header: String,
    message_type: &str,
    transformers: Vec<MarkdownTransformerFn>,
    message_id: Option<String>,
    timestamp: Option<String>,
) -> MarkdownTransformFn {
    let transform = create_markdown_transform_with_metadata(
        message_type,
        || false,
        transformers,
        message_id,
        timestamp,
    );
    Arc::new(move |markdown, width| {
        let transformed = transform
            .as_ref()
            .map(|transform| transform(markdown, width))
            .unwrap_or_else(|| markdown.to_owned());
        format!("{header}{transformed}")
    })
}

/// `applyMarkdownTransformers` (markdown-transform.ts:11-29): apply each
/// transformer in order; each transformer observes the previous transformer's
/// output. Panics propagate (no try/catch in Rust); the WASM boundary already
/// converts guest errors into unchanged input.
fn apply_markdown_transformers(
    markdown: &str,
    context: &MarkdownTransformContext,
    transformers: &[MarkdownTransformerFn],
) -> String {
    let mut transformed = markdown.to_owned();
    for transformer in transformers {
        transformed = transformer(transformed, context.clone());
    }
    transformed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    type SeenCalls = Arc<std::sync::Mutex<Vec<(String, MarkdownTransformContext)>>>;

    fn transformer(
        label: &'static str,
        seen: Option<SeenCalls>,
        prefix: &'static str,
    ) -> MarkdownTransformerFn {
        Arc::new(move |md, ctx| {
            if let Some(seen) = &seen {
                seen.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((label.to_owned(), ctx.clone()));
            }
            format!("{prefix}({md})")
        })
    }

    #[test]
    fn empty_transformer_list_returns_none() {
        // markdown-transform.ts + upstream constructor defaults: no
        // transformers means the Markdown renders without a transform.
        assert!(create_markdown_transform("assistant", || false, Vec::new()).is_none());
    }

    #[test]
    fn chains_transformers_in_order_with_context() {
        // markdown-transform.ts:12-28: each transformer observes the
        // previous output; all receive the same context.
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let transform = create_markdown_transform(
            "assistant",
            || true,
            vec![
                transformer("a", Some(Arc::clone(&seen)), "A"),
                transformer("b", Some(Arc::clone(&seen)), "B"),
            ],
        )
        .expect("transform for non-empty list");
        let out = transform("hello", 42);
        assert_eq!(out, "B(A(hello))");

        let calls = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "a");
        assert_eq!(calls[1].0, "b");
        for (_, context) in &calls {
            assert_eq!(context.message_type, "assistant");
            assert!(context.is_streaming);
            assert_eq!(context.available_width, 42);
        }
    }

    #[test]
    fn captures_message_type_and_is_streaming_across_calls() {
        // The context's messageType/isStreaming come from creation time;
        // availableWidth from the render call (markdown-transform.ts:7-9).
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_inner = Arc::clone(&counter);
        let transform = create_markdown_transform(
            "assistant-thinking",
            || false,
            vec![Arc::new(move |md, ctx| {
                counter_inner.fetch_add(1, Ordering::Relaxed);
                assert_eq!(ctx.message_type, "assistant-thinking");
                assert!(!ctx.is_streaming);
                md
            })],
        )
        .expect("transform");
        assert_eq!(transform("x", 10), "x");
        assert_eq!(transform("x", 20), "x");
        assert_eq!(counter.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn streaming_flag_is_read_per_transform_call() {
        // Upstream 0f21a3319: a reused Markdown component must see a stream
        // end without being rebuilt.
        let streaming = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let streaming_inner = Arc::clone(&streaming);
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_inner = Arc::clone(&seen);
        let transform = create_markdown_transform(
            "assistant",
            move || streaming_inner.load(Ordering::Relaxed),
            vec![Arc::new(move |md, ctx| {
                seen_inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(ctx.is_streaming);
                md
            })],
        )
        .expect("transform");
        transform("x", 10);
        streaming.store(false, Ordering::Relaxed);
        transform("x", 10);
        assert_eq!(
            *seen.lock().unwrap_or_else(|e| e.into_inner()),
            vec![true, false]
        );
    }

    #[test]
    fn persisted_metadata_is_forwarded_and_host_header_is_not_transformed() {
        let transform = create_header_prefixed_markdown_transform(
            "**Summary**\n\n".to_owned(),
            "summary",
            vec![Arc::new(|markdown, context| {
                assert_eq!(markdown, "body");
                assert_eq!(context.message_id.as_deref(), Some("entry-7"));
                assert_eq!(context.timestamp.as_deref(), Some("2026-11-18T00:00:00Z"));
                assert_eq!(context.message_type, "summary");
                format!("transformed({markdown})")
            })],
            Some("entry-7".to_owned()),
            Some("2026-11-18T00:00:00Z".to_owned()),
        );
        assert_eq!(transform("body", 80), "**Summary**\n\ntransformed(body)");
    }
}
