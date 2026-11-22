//! Assistant message rendering — port of
//! `packages/coding-agent/src/modes/interactive/components/assistant-message.ts`
//! @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - The theme is passed explicitly (`Arc<Theme>`) instead of read from the
//!   global `theme` getter (theme.ts:799-816); `markdownTheme` defaults to
//!   the shared interactive theme built by
//!   [`crate::modes::interactive::theme::markdown_theme`].
//! - `updateContent` takes the message by value (upstream reference); the
//!   component stores its own clone.

use std::boxed::Box as StdBox;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use rpi_ai::types::{AssistantContent, AssistantMessage, SharedAssistantMessage, StopReason};
use rpi_ext_host::types::MarkdownTransformerFn;
use rpi_tui::components::markdown::{
    DefaultTextStyle, Markdown, MarkdownOptions, MarkdownTheme, MarkdownTransformRevisionFn,
};
use rpi_tui::components::mouse_region::MouseRegion;
use rpi_tui::components::spacer::Spacer;
use rpi_tui::components::text::Text;
use rpi_tui::tui::{
    Component, Container, TuiMouseButton, TuiMouseEvent, TuiMouseEventResult, TuiMouseEventType,
    TuiMouseHandlerResult, shared_component_from_boxed,
};

use crate::core::themes::Theme;

use super::markdown_transform::create_markdown_transform_with_metadata;
use super::util::{OSC133_ZONE_END, OSC133_ZONE_FINAL, OSC133_ZONE_START};

/// Component that renders a complete assistant message
/// (assistant-message.ts:12-180).
pub struct AssistantMessageComponent {
    content_container: Container,
    hide_thinking_block: bool,
    /// `thinkingVisibilityOverrides` (assistant-message.ts:24 @ 9841914,
    /// `71026970a`): per-thinking-run visibility overrides keyed by run
    /// index, toggled by clicking the block (FR-A R9). Shared interior
    /// mutability so the `MouseRegion` callback — which runs while this
    /// component is locked for `handle_mouse` — can record toggles without
    /// re-entering the component (the toggle is drained and applied right
    /// after the child dispatch returns, before the next render).
    thinking_visibility_overrides: Arc<StdMutex<BTreeMap<usize, bool>>>,
    /// Run indices whose blocks were clicked since the last drain.
    pending_visibility_toggles: Arc<StdMutex<Vec<usize>>>,
    markdown_theme: Arc<MarkdownTheme>,
    hidden_thinking_label: String,
    output_pad: usize,
    /// V15-13 L3: the stored message is shared by `Arc` — the per-delta
    /// skip path stores a refcount, not a deep copy of the accumulated
    /// partial.
    last_message: Option<SharedAssistantMessage>,
    /// Fingerprint of everything the `updateContent` rebuild depends on;
    /// `None` until the first rebuild. Equal fingerprints skip the rebuild.
    last_signature: Option<VisibleSignature>,
    /// Construction-time inputs shared by every Markdown child: `output_pad`
    /// (padding is set at construction) and the message metadata captured by
    /// the transform. `is_streaming` is read lazily and is deliberately not
    /// part of this key (V19-04 FR-B).
    last_global_key: Option<(usize, Option<String>, Option<String>)>,
    /// Markdown instances of the last rebuild, in content order (V19-04
    /// FR-B / upstream `markdownSlots`). The next rebuild reuses the instance
    /// at the same position and kind and calls `set_text()`.
    markdown_slots: Vec<MarkdownSlot>,
    /// Lazy `isStreaming` source read by the transform closure, so one
    /// Markdown instance survives `true -> false`.
    streaming_flag: Arc<AtomicBool>,
    /// Monotonic streaming-flag generation (part of the Markdown revision).
    streaming_generation: Arc<AtomicU64>,
    /// Whether the built-in transform's revision source is trusted. Set by
    /// the interactive mode (the Mermaid transformer is the only built-in
    /// one); opaque extension transformers keep `cache_revision()` at None.
    trust_transform_revision: bool,
    /// Monotonic rebuild revision for [`Component::cache_revision`].
    revision: u64,
    has_tool_calls: bool,
    theme: Arc<Theme>,
    /// Extension-registered Markdown transformers (assistant-message.ts:20).
    markdown_transformers: Vec<MarkdownTransformerFn>,
    /// `isStreaming` (assistant-message.ts:23): set by `updateContent`'s
    /// argument and captured into the transform context.
    is_streaming: bool,
    message_id: Option<String>,
    timestamp: Option<String>,
}

/// Cheap fingerprint of the rebuild inputs: streaming flag, thinking-block
/// settings, pad, stop reason / error text, tool-call presence, and a
/// (length, hash) pair per visible text/thinking block. Everything the
/// rebuilt children and the render-time OSC133 markers depend on.
type VisibleSignature = (
    bool,
    bool,
    String,
    usize,
    StopReason,
    Option<String>,
    bool,
    Vec<(u64, u64)>,
    u64,
);

/// (len, hash) fingerprint of a visible block's content.
fn block_fingerprint(text: &str) -> (u64, u64) {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    (text.len() as u64, hasher.finish())
}

/// Kind of a reusable Markdown slot (upstream `MarkdownSlot["kind"]`,
/// assistant-message.ts @ 0f21a3319).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SlotKind {
    Text,
    Thinking,
}

/// A Markdown instance reused across updates at the same position and kind
/// (upstream `markdownSlots`). Content changes call `set_text()` instead of
/// replacing the instance, so its parsed/rendered block caches survive.
struct MarkdownSlot {
    kind: SlotKind,
    markdown: Arc<StdMutex<Markdown>>,
}

/// Container child that forwards to a slot's shared Markdown instance, so the
/// component can keep mutating it (`set_text`) after mounting.
struct SlotMarkdownChild(Arc<StdMutex<Markdown>>);

impl Component for SlotMarkdownChild {
    fn render(&self, width: usize) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .render(width)
    }

    fn invalidate(&mut self) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .invalidate();
    }

    fn cache_revision(&self) -> Option<u64> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .cache_revision()
    }
}

/// Narrow per-update snapshot (V19-04 FR-B R3): only the visible strings,
/// stop/error and tool presence leave the live-message lock. Tool-call
/// arguments are never cloned (only a marker is kept).
enum VisibleBlock {
    Text(String),
    Thinking(String),
    ToolCall,
}

struct VisibleSnapshot {
    content: Vec<VisibleBlock>,
    stop_reason: StopReason,
    error_message: Option<String>,
    has_tool_calls: bool,
}

impl VisibleSnapshot {
    fn from_message(message: &AssistantMessage) -> Self {
        let mut content = Vec::with_capacity(message.content.len());
        let mut has_tool_calls = false;
        for item in &message.content {
            match item {
                AssistantContent::Text(text) => content.push(VisibleBlock::Text(text.text.clone())),
                AssistantContent::Thinking(thinking) => {
                    content.push(VisibleBlock::Thinking(thinking.thinking.clone()));
                }
                AssistantContent::ToolCall(_) => {
                    has_tool_calls = true;
                    content.push(VisibleBlock::ToolCall);
                }
            }
        }
        Self {
            content,
            stop_reason: message.stop_reason,
            error_message: message.error_message.clone(),
            has_tool_calls,
        }
    }
}

/// Stable fingerprint of the thinking-visibility overrides (part of the
/// rebuild signature since V14-14: a click toggle must invalidate the
/// cached rebuild even when the message itself is unchanged).
fn overrides_fingerprint(overrides: &BTreeMap<usize, bool>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (run, hidden) in overrides {
        run.hash(&mut hasher);
        hidden.hash(&mut hasher);
    }
    hasher.finish()
}

#[cfg(test)]
thread_local! {
    /// Counts `update_content` calls that constructed at least one child
    /// (perf tests). Since V15-13 FR-L2 a call only constructs the slots
    /// whose descriptors changed — the count is the observable for "this
    /// update changed something visible".
    static REBUILD_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Counts `Markdown` component constructions inside `update_content`
    /// (V15-13 FR-L2 assertion: per folded delta the count is bounded by the
    /// number of changed blocks, not the message's block count).
    static MARKDOWN_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Test-only introspection for the V13-06 §3.4 integration anchor
/// (M6 follow-up): the synthetic 500-delta folding test in
/// `interactive_mode.rs` drives the REAL queue/drain pipeline and asserts
/// rebuild counts — the counter must be reachable from that module.
#[cfg(test)]
pub(crate) mod rebuild_counter {
    /// Reset the current thread's rebuild counter.
    pub(crate) fn reset() {
        super::REBUILD_COUNT.with(|count| count.set(0));
    }
    /// Current thread's rebuild count since the last [`reset`].
    pub(crate) fn get() -> usize {
        super::REBUILD_COUNT.with(|count| count.get())
    }
}

impl AssistantMessageComponent {
    pub fn new(
        message: Option<AssistantMessage>,
        hide_thinking_block: bool,
        theme: Arc<Theme>,
        markdown_theme: Arc<MarkdownTheme>,
        hidden_thinking_label: impl Into<String>,
        output_pad: usize,
        markdown_transformers: Vec<MarkdownTransformerFn>,
    ) -> Self {
        let mut component = Self {
            content_container: Container::new(),
            hide_thinking_block,
            thinking_visibility_overrides: Arc::new(StdMutex::new(BTreeMap::new())),
            pending_visibility_toggles: Arc::new(StdMutex::new(Vec::new())),
            markdown_theme,
            hidden_thinking_label: hidden_thinking_label.into(),
            output_pad,
            last_message: None,
            last_signature: None,
            last_global_key: None,
            markdown_slots: Vec::new(),
            streaming_flag: Arc::new(AtomicBool::new(false)),
            streaming_generation: Arc::new(AtomicU64::new(0)),
            trust_transform_revision: false,
            revision: 0,
            has_tool_calls: false,
            theme,
            markdown_transformers,
            is_streaming: false,
            message_id: None,
            timestamp: None,
        };
        if let Some(message) = message {
            component.update_content(&SharedAssistantMessage::new(message), false);
        }
        component
    }

    /// Enable the built-in transform revision source (V19-04 FR-E). The
    /// interactive mode sets this for live assistant messages; tests and
    /// opaque transformer chains leave it off.
    pub fn set_transform_revision_trusted(&mut self, trusted: bool) {
        if self.trust_transform_revision == trusted {
            return;
        }
        self.trust_transform_revision = trusted;
        // Existing Markdown instances captured the old transform options;
        // rebuild the slots once so they carry the revision source. The
        // content itself is unchanged.
        if let Some(message) = self.last_message.clone() {
            self.markdown_slots.clear();
            self.last_signature = None;
            self.update_content(&message, self.is_streaming);
        }
    }

    pub fn set_message_metadata(&mut self, message_id: Option<String>, timestamp: Option<String>) {
        self.message_id = message_id;
        self.timestamp = timestamp;
        if let Some(message) = self.last_message.clone() {
            self.update_content(&message, self.is_streaming);
        }
    }

    /// `setHideThinkingBlock` (assistant-message.ts:51-56): also clears the
    /// per-run visibility overrides (:60) — the global setting replaces any
    /// click-toggled state.
    pub fn set_hide_thinking_block(&mut self, hide: bool) {
        self.hide_thinking_block = hide;
        self.thinking_visibility_overrides
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        if let Some(message) = self.last_message.clone() {
            self.update_content(&message, self.is_streaming);
        }
    }

    /// `setHiddenThinkingLabel` (assistant-message.ts:58-63).
    pub fn set_hidden_thinking_label(&mut self, label: impl Into<String>) {
        self.hidden_thinking_label = label.into();
        if let Some(message) = self.last_message.clone() {
            self.update_content(&message, self.is_streaming);
        }
    }

    /// `setOutputPad` (assistant-message.ts:65-70).
    pub fn set_output_pad(&mut self, padding: usize) {
        self.output_pad = padding;
        if let Some(message) = self.last_message.clone() {
            self.update_content(&message, self.is_streaming);
        }
    }

    /// `updateContent` (assistant-message.ts:89-179). `is_streaming` is the
    /// upstream `isStreaming` argument (`updateContent(message, isStreaming =
    /// this.isStreaming)` — the setters and `invalidate` keep the stored
    /// value).
    ///
    /// Perf deviation: upstream rebuilds the content container on every
    /// call. During tool-call streaming this runs once per delta; when the
    /// visible fingerprint is unchanged (a write/edit streaming its args —
    /// no text/thinking/stop changes), the rebuild is skipped and only the
    /// stored message is refreshed.
    /// `constructed` below feeds only the test-only rebuild counter
    /// (`REBUILD_COUNT`); non-test builds never read it.
    #[allow(unused_assignments, unused_variables)]
    pub fn update_content(&mut self, shared_message: &SharedAssistantMessage, is_streaming: bool) {
        // The transform closure reads the flag lazily (V19-04 FR-B), so a
        // stream end does not require rebuilding the Markdown instances; the
        // generation is part of their `cache_revision()`.
        if self.is_streaming != is_streaming {
            self.streaming_generation.fetch_add(1, Ordering::Relaxed);
        }
        self.is_streaming = is_streaming;
        self.streaming_flag.store(is_streaming, Ordering::Relaxed);

        // Compute the visible fingerprint under the lock without copying the
        // message: during tool-call streaming this runs once per delta, and a
        // full snapshot here would move the O(message) copy that A2 removed
        // from the producer onto the driver thread.
        let signature = shared_message.with(|message| {
            (
                is_streaming,
                self.hide_thinking_block,
                self.hidden_thinking_label.clone(),
                self.output_pad,
                message.stop_reason,
                message.error_message.clone(),
                message
                    .content
                    .iter()
                    .any(|c| matches!(c, AssistantContent::ToolCall(_))),
                message
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        AssistantContent::Text(text) => Some(block_fingerprint(&text.text)),
                        AssistantContent::Thinking(thinking) => {
                            Some(block_fingerprint(&thinking.thinking))
                        }
                        AssistantContent::ToolCall(_) => None,
                    })
                    .collect::<Vec<_>>(),
                overrides_fingerprint(
                    &self
                        .thinking_visibility_overrides
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()),
                ),
            )
        });
        // Retain the live handle for setters and invalidation.
        if self.last_signature.as_ref() == Some(&signature) {
            self.last_message = Some(shared_message.clone());
            return;
        }
        self.last_signature = Some(signature);
        self.last_message = Some(shared_message.clone());

        // Narrow snapshot (V19-04 FR-B R3): copy only visible strings, stop /
        // error and tool presence; never the tool-call arguments.
        let snapshot = shared_message.with(VisibleSnapshot::from_message);
        self.has_tool_calls = snapshot.has_tool_calls;

        let global_key = (
            self.output_pad,
            self.message_id.clone(),
            self.timestamp.clone(),
        );
        let global_changed = self.last_global_key.as_ref() != Some(&global_key);
        self.last_global_key = Some(global_key);

        // Reuse the Markdown instance at the same position and kind unless a
        // construction-time input (outputPad, metadata) changed.
        let previous_slots = if global_changed {
            self.markdown_slots.clear();
            Vec::new()
        } else {
            std::mem::take(&mut self.markdown_slots)
        };
        let mut next_slots: Vec<MarkdownSlot> = Vec::new();
        let mut new_children: Vec<StdBox<dyn Component>> = Vec::new();

        if self.has_visible_content_from(&snapshot.content, 0) {
            new_children.push(StdBox::new(Spacer::new(1)));
        }

        // Render content in order (assistant-message.ts:98-146).
        let mut i = 0;
        let mut thinking_run_index = 0usize;
        while i < snapshot.content.len() {
            match &snapshot.content[i] {
                // Assistant text messages with no background - trim the text.
                // Set paddingY=0 to avoid extra spacing before tool executions
                // (assistant-message.ts:100-103).
                VisibleBlock::Text(text) if !text.trim().is_empty() => {
                    let trimmed = text.trim();
                    let slot_index = next_slots.len();
                    let markdown = self.take_or_create_markdown(
                        &previous_slots,
                        slot_index,
                        SlotKind::Text,
                        trimmed,
                        "assistant",
                    );
                    next_slots.push(MarkdownSlot {
                        kind: SlotKind::Text,
                        markdown: Arc::clone(&markdown),
                    });
                    new_children.push(StdBox::new(SlotMarkdownChild(markdown)));
                    i += 1;
                }
                VisibleBlock::Thinking(_) => {
                    // Merge consecutive thinking blocks into one run
                    // (assistant-message.ts:104-116).
                    let mut thinking_blocks: Vec<String> = Vec::new();
                    while i < snapshot.content.len() {
                        match &snapshot.content[i] {
                            VisibleBlock::Thinking(thinking) => {
                                let trimmed = thinking.trim();
                                if !trimmed.is_empty() {
                                    thinking_blocks.push(trimmed.to_string());
                                }
                                i += 1;
                            }
                            _ => break,
                        }
                    }

                    if thinking_blocks.is_empty() {
                        continue;
                    }

                    // Add spacing only when another visible assistant content
                    // block follows (assistant-message.ts:122-126).
                    let has_visible_content_after =
                        self.has_visible_content_from(&snapshot.content[i..], thinking_run_index);

                    // Per-run visibility override + clickable toggle
                    // (assistant-message.ts:141-171 @ 9841914, 71026970a):
                    // each run is wrapped in a MouseRegion; a left click flips
                    // the run's override and rebuilds the content.
                    let run_index = thinking_run_index;
                    thinking_run_index += 1;
                    let hidden = self
                        .thinking_visibility_overrides
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .get(&run_index)
                        .copied()
                        .unwrap_or(self.hide_thinking_block);
                    if hidden && self.hidden_thinking_label.trim().is_empty() {
                        continue;
                    }
                    let joined = thinking_blocks.join("\n\n");
                    let thinking_component: StdBox<dyn Component> = if hidden {
                        // Show one static label for the run when hidden
                        // (assistant-message.ts:128-132). The label is cheap
                        // and is not a Markdown slot upstream either.
                        let label = Theme::italic(
                            &self.theme.fg("thinkingText", &self.hidden_thinking_label),
                        );
                        StdBox::new(Text::new(label, self.output_pad, 0, None))
                    } else {
                        let slot_index = next_slots.len();
                        let markdown = self.take_or_create_markdown(
                            &previous_slots,
                            slot_index,
                            SlotKind::Thinking,
                            &joined,
                            "assistant-thinking",
                        );
                        next_slots.push(MarkdownSlot {
                            kind: SlotKind::Thinking,
                            markdown: Arc::clone(&markdown),
                        });
                        StdBox::new(SlotMarkdownChild(markdown))
                    };
                    let overrides = Arc::clone(&self.thinking_visibility_overrides);
                    let pending = Arc::clone(&self.pending_visibility_toggles);
                    new_children.push(StdBox::new(MouseRegion::new(
                        shared_component_from_boxed(thinking_component),
                        Box::new(move |event| {
                            if event.event_type != TuiMouseEventType::Click
                                || event.button != TuiMouseButton::Left
                            {
                                return None;
                            }
                            // Upstream flips the override synchronously and
                            // rebuilds; the port records the toggle (the
                            // component is locked for `handle_mouse`) and the
                            // drain below applies it before the next render.
                            let mut overrides = overrides
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            overrides.insert(run_index, !hidden);
                            drop(overrides);
                            pending
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .push(run_index);
                            Some(TuiMouseEventResult {
                                handled: true,
                                ..Default::default()
                            })
                        }),
                    )));
                    if has_visible_content_after {
                        new_children.push(StdBox::new(Spacer::new(1)));
                    }
                }
                _ => {
                    i += 1;
                }
            }
        }

        // Incomplete/failed handling (assistant-message.ts:148-178).
        // Length stops: neutral truncation wording (assistant-message.ts:180
        // @ 32850ef7c).
        let trailer: Option<String> = if snapshot.stop_reason == StopReason::Length {
            Some(
                self.theme
                    .fg("error", "Response was truncated before completion."),
            )
        } else if !snapshot.has_tool_calls {
            match snapshot.stop_reason {
                StopReason::Aborted => {
                    let abort_message = match &snapshot.error_message {
                        Some(msg) if msg != "Request was aborted" => msg.clone(),
                        _ => "Operation aborted".to_string(),
                    };
                    Some(self.theme.fg("error", &abort_message))
                }
                StopReason::Error => {
                    let error_msg = snapshot
                        .error_message
                        .clone()
                        .unwrap_or_else(|| "Unknown error".to_string());
                    Some(self.theme.fg("error", &format!("Error: {error_msg}")))
                }
                _ => None,
            }
        } else {
            None
        };
        if let Some(text) = &trailer {
            new_children.push(StdBox::new(Spacer::new(1)));
            new_children.push(StdBox::new(Text::new(
                text.clone(),
                self.output_pad,
                0,
                None,
            )));
        }

        self.content_container.children = new_children;
        self.markdown_slots = next_slots;
        self.revision = self.revision.wrapping_add(1);
        #[cfg(test)]
        REBUILD_COUNT.with(|count| count.set(count.get() + 1));
    }

    /// Reuse the Markdown instance at `slot_index` when its kind matches and
    /// call `set_text()`; otherwise construct one. V19-04 FR-B R1.
    fn take_or_create_markdown(
        &self,
        previous_slots: &[MarkdownSlot],
        slot_index: usize,
        kind: SlotKind,
        text: &str,
        message_type: &str,
    ) -> Arc<StdMutex<Markdown>> {
        if let Some(slot) = previous_slots.get(slot_index)
            && slot.kind == kind
        {
            let markdown = Arc::clone(&slot.markdown);
            markdown
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .set_text(text);
            return markdown;
        }
        let default_text_style = match kind {
            SlotKind::Text => None,
            SlotKind::Thinking => Some(DefaultTextStyle {
                color: Some(Box::new({
                    let theme = Arc::clone(&self.theme);
                    move |text: &str| theme.fg("thinkingText", text)
                })),
                italic: true,
                ..Default::default()
            }),
        };
        #[cfg(test)]
        MARKDOWN_COUNT.with(|count| count.set(count.get() + 1));
        Arc::new(StdMutex::new(Markdown::new(
            text,
            self.output_pad,
            0,
            Arc::clone(&self.markdown_theme),
            default_text_style,
            self.markdown_options(message_type),
        )))
    }

    fn has_visible_content_from(&self, content: &[VisibleBlock], start_run: usize) -> bool {
        let mut index = 0;
        let mut run_index = start_run;
        while index < content.len() {
            match &content[index] {
                VisibleBlock::Text(text) if !text.trim().is_empty() => return true,
                VisibleBlock::Thinking(_) => {
                    let mut nonempty = false;
                    while let Some(VisibleBlock::Thinking(thinking)) = content.get(index) {
                        nonempty |= !thinking.trim().is_empty();
                        index += 1;
                    }
                    let hidden = self
                        .thinking_visibility_overrides
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .get(&run_index)
                        .copied()
                        .unwrap_or(self.hide_thinking_block);
                    if nonempty && (!hidden || !self.hidden_thinking_label.trim().is_empty()) {
                        return true;
                    }
                    run_index += 1;
                }
                _ => index += 1,
            }
        }
        false
    }

    /// Markdown options carrying the width-aware transform chain for the
    /// block's message type (assistant-message.ts:110-113, 156-162); `None`
    /// when no transformer is registered (the Markdown renders unchanged).
    ///
    /// V19-04 FR-B: `isStreaming` is a lazy closure, so one Markdown instance
    /// survives the stream end. FR-E: when only the built-in Mermaid
    /// transformer is registered and the interactive mode trusts its revision,
    /// the Markdown exposes a monotonic revision source.
    fn markdown_options(&self, message_type: &str) -> Option<MarkdownOptions> {
        let streaming = Arc::clone(&self.streaming_flag);
        let transform = create_markdown_transform_with_metadata(
            message_type,
            move || streaming.load(Ordering::Relaxed),
            self.markdown_transformers.clone(),
            self.message_id.clone(),
            self.timestamp.clone(),
        )?;
        let transform_revision =
            if self.trust_transform_revision && self.markdown_transformers.len() == 1 {
                let streaming = Arc::clone(&self.streaming_flag);
                Some(Arc::new(move || {
                    let mode = crate::core::settings_manager::mermaid_mode_generation();
                    let capabilities = rpi_tui::terminal_image::capabilities_generation();
                    u64::from(streaming.load(Ordering::Relaxed))
                        ^ mode.rotate_left(21)
                        ^ capabilities.rotate_left(42)
                }) as MarkdownTransformRevisionFn)
            } else {
                None
            };
        Some(MarkdownOptions {
            transform: Some(transform),
            transform_revision,
            ..Default::default()
        })
    }

    /// Apply thinking-visibility toggles recorded by `MouseRegion` callbacks
    /// during a dispatch and rebuild the content
    /// (assistant-message.ts:165-167 @ 9841914 — upstream rebuilds
    /// synchronously inside the callback; the port defers to here because
    /// the component is locked for `handle_mouse`).
    fn drain_pending_visibility_toggles(&mut self) {
        let toggles = std::mem::take(
            &mut *self
                .pending_visibility_toggles
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        if toggles.is_empty() {
            return;
        }
        if let Some(message) = self.last_message.clone() {
            self.update_content(&message, self.is_streaming);
        }
    }
}

impl Component for AssistantMessageComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = self.content_container.render(width);
        // OSC 133 zone markers on the first/last line unless tool calls
        // render separately (assistant-message.ts:72-81).
        if self.has_tool_calls || lines.is_empty() {
            return lines;
        }
        lines[0] = format!("{OSC133_ZONE_START}{}", lines[0]);
        let last = lines.len() - 1;
        lines[last] = format!("{OSC133_ZONE_END}{OSC133_ZONE_FINAL}{}", lines[last]);
        lines
    }

    /// Mouse dispatch through the content container
    /// (`AssistantMessageComponent extends Container`, assistant-message.ts:14:
    /// `Container.prototype.handleMouse` hit-tests the children — including
    /// the `MouseRegion`-wrapped thinking blocks). Click-recorded visibility
    /// toggles are applied right after the dispatch returns.
    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<TuiMouseHandlerResult> {
        let result = self.content_container.handle_mouse(event);
        self.drain_pending_visibility_toggles();
        result
    }

    /// `updateThinkingBlockVisibility` walk target
    /// (interactive-mode.ts:4198-4205 @ 9841914, b07e17faa): in-place toggle
    /// so live tool components keep their state.
    fn set_hide_thinking_block(&mut self, hide: bool) {
        AssistantMessageComponent::set_hide_thinking_block(self, hide);
    }

    fn set_output_pad(&mut self, output_pad: usize) {
        // `"setOutputPad" in child` walk equivalent (#10557); no recursion.
        AssistantMessageComponent::set_output_pad(self, output_pad);
    }

    fn invalidate(&mut self) {
        self.content_container.invalidate();
        self.revision = self.revision.wrapping_add(1);
        if let Some(message) = self.last_message.clone() {
            self.update_content(&message, self.is_streaming);
        }
    }

    fn cache_revision(&self) -> Option<u64> {
        // V19-04 FR-E: aggregate the trusted children. `content_container`
        // itself is a `Container` with publicly mutable children, so it
        // deliberately returns `None`; this component owns the child list and
        // aggregates it here.
        let mut revision = self.revision;
        for child in &self.content_container.children {
            revision = revision.wrapping_mul(0x0000_0100_0000_01b3) ^ child.cache_revision()?;
        }
        Some(revision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::themes::load_theme;
    use crate::modes::interactive::theme::markdown_theme;
    use rpi_ai::types::{AssistantRole, TextContent, ThinkingContent, Usage};

    fn theme() -> Arc<Theme> {
        Arc::new(load_theme("dark", None).expect("builtin dark theme"))
    }

    /// V15-13 L3: `update_content` takes the shared streaming partial.
    fn shared_message(content: Vec<AssistantContent>) -> SharedAssistantMessage {
        SharedAssistantMessage::new(message(content))
    }

    fn shared_message_from(message: AssistantMessage) -> SharedAssistantMessage {
        SharedAssistantMessage::new(message)
    }

    fn message(content: Vec<AssistantContent>) -> AssistantMessage {
        AssistantMessage {
            role: AssistantRole::Assistant,
            content,
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            model: "m".into(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            thinking_level: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
            deferred: None,
            end_turn: None,
            raw_stop_reason: None,
            duration_ms: None,
        }
    }

    fn text(s: &str) -> AssistantContent {
        AssistantContent::Text(TextContent {
            text: s.to_string(),
            text_signature: None,
        })
    }

    fn thinking(s: &str) -> AssistantContent {
        AssistantContent::Thinking(ThinkingContent {
            thinking: s.to_string(),
            thinking_signature: None,
            redacted: None,
        })
    }

    fn strip_ansi(input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let mut chars = input.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' && chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    /// Perf regression (300+ line streaming writes): `MessageUpdate` fires
    /// once per tool-args delta; updates that only change tool-call blocks
    /// (no text/thinking/stop/error changes) must skip the container
    /// rebuild entirely.
    #[test]
    fn tool_call_only_updates_skip_the_rebuild() {
        let mut component = AssistantMessageComponent::new(
            None,
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
            Vec::new(),
        );
        let tool_call = |args_len: usize| {
            AssistantContent::ToolCall(rpi_ai::types::ToolCall {
                id: "t1".into(),
                name: "write".into(),
                arguments: serde_json::Map::from_iter([(
                    "content".to_owned(),
                    serde_json::Value::String("x".repeat(args_len)),
                )]),
                thought_signature: None,
                namespace: None,
                arguments_error: None,
            })
        };

        REBUILD_COUNT.with(|count| count.set(0));
        // Visible text first — rebuilds.
        component.update_content(
            &shared_message(vec![text("Writing the file:"), tool_call(10)]),
            true,
        );
        assert_eq!(REBUILD_COUNT.with(|c| c.get()), 1);

        // 300 tool-args-only deltas: no further rebuilds.
        for len in 11..=310 {
            component.update_content(
                &shared_message(vec![text("Writing the file:"), tool_call(len)]),
                true,
            );
        }
        assert_eq!(
            REBUILD_COUNT.with(|c| c.get()),
            1,
            "tool-call-only deltas must not rebuild the container"
        );

        // The rendered text survives the skipped rebuilds.
        let lines = component.render(60);
        assert!(lines.iter().any(|l| l.contains("Writing the file:")));

        // isStreaming transition (message_end) rebuilds exactly once more.
        component.update_content(
            &shared_message(vec![text("Writing the file:"), tool_call(310)]),
            false,
        );
        assert_eq!(REBUILD_COUNT.with(|c| c.get()), 2);

        // A text delta rebuilds.
        component.update_content(
            &shared_message(vec![text("Writing the file: done"), tool_call(310)]),
            false,
        );
        assert_eq!(REBUILD_COUNT.with(|c| c.get()), 3);
    }

    /// `set_output_pad` and the thinking setters must not be swallowed by
    /// the signature skip: they change the rebuild inputs and must rebuild.
    /// V15-13 FR-L2 (G2): the fixture carries a thinking run so the hide
    /// and label setters have an affected child to reconstruct — with the
    /// slot-level diff, a setter that changes no child (e.g. a label change
    /// with no hidden run) correctly constructs nothing while the output
    /// stays identical. The counts keep their pre-L2 meaning: every setter
    /// here invalidates at least one slot.
    #[test]
    fn setters_bypass_the_signature_skip() {
        let mut component = AssistantMessageComponent::new(
            Some(message(vec![thinking("secret"), text("hello")])),
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
            Vec::new(),
        );
        REBUILD_COUNT.with(|count| count.set(0));
        component.set_output_pad(2);
        assert_eq!(REBUILD_COUNT.with(|c| c.get()), 1, "pad change rebuilds");
        component.set_hide_thinking_block(true);
        assert_eq!(REBUILD_COUNT.with(|c| c.get()), 2, "hide toggle rebuilds");
        component.set_hidden_thinking_label("Thoughts");
        assert_eq!(REBUILD_COUNT.with(|c| c.get()), 3, "label change rebuilds");
        // A same-value setter is a no-op (the fingerprint is unchanged and
        // the rendering would be identical anyway).
        component.set_output_pad(2);
        assert_eq!(REBUILD_COUNT.with(|c| c.get()), 3);
    }

    // ------------------------------------------------------------------
    // V15-13 FR-L2 (rpi#53 root cause 2): block-level component reuse.
    // ------------------------------------------------------------------

    /// Folded-delta replay (issue #53 shape: one stable thinking run + one
    /// growing text block): per update, `Markdown` constructions must be
    /// bounded by the number of CHANGED blocks — the stable thinking run
    /// keeps its component (and its render cache) across every delta. The
    /// final frame must be byte-identical to a freshly built component
    /// rendering the same final message.
    #[test]
    fn block_diff_reuses_unchanged_markdown_components() {
        let thinking_text = "稳定的思考内容，跨 delta 不变。\n\nsecond paragraph";
        let shared_mk = |grown: usize| {
            SharedAssistantMessage::new(message(vec![
                thinking(thinking_text),
                text(&format!(
                    "输出前缀，第 {grown} 轮：{}",
                    "数据".repeat(grown)
                )),
            ]))
        };
        let mk = |grown: usize| {
            message(vec![
                thinking(thinking_text),
                text(&format!(
                    "输出前缀，第 {grown} 轮：{}",
                    "数据".repeat(grown)
                )),
            ])
        };

        let mut component = AssistantMessageComponent::new(
            None,
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
            Vec::new(),
        );
        // Initial build: one Markdown for the thinking run + one for text.
        MARKDOWN_COUNT.with(|count| count.set(0));
        component.update_content(&shared_mk(1), true);
        assert_eq!(MARKDOWN_COUNT.with(|c| c.get()), 2);

        // 50 folded deltas growing only the text block: the Markdown
        // instances are reused at their positions and updated with
        // `set_text`, so no delta constructs a new component (V19-04 FR-B).
        for grown in 2..=51 {
            component.update_content(&shared_mk(grown), true);
            assert_eq!(
                MARKDOWN_COUNT.with(|c| c.get()),
                2,
                "delta {grown}: reused Markdown instances must not be reconstructed"
            );
            if grown == 26 {
                // Byte parity DURING the reuse path: compare against a fresh
                // STREAMING component while the reused slots are live.
                let before = MARKDOWN_COUNT.with(|c| c.get());
                let mut fresh_streaming = AssistantMessageComponent::new(
                    None,
                    false,
                    theme(),
                    markdown_theme(&load_theme("dark", None).unwrap()),
                    "Thinking...",
                    1,
                    Vec::new(),
                );
                fresh_streaming.update_content(&shared_mk(26), true);
                assert_eq!(MARKDOWN_COUNT.with(|c| c.get()), before + 2);
                assert_eq!(
                    component.render(100),
                    fresh_streaming.render(100),
                    "reused slots must render byte-identically mid-stream"
                );
                MARKDOWN_COUNT.with(|c| c.set(before));
            }
        }

        // Frame parity: streamed component vs a fresh one with the final
        // message (byte-identical lines). The fresh constructor itself
        // increments the thread-local counter, so capture the baseline after
        // it.
        let fresh = AssistantMessageComponent::new(
            Some(mk(51)),
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
            Vec::new(),
        );
        let before_fresh = MARKDOWN_COUNT.with(|c| c.get());
        // The fresh component renders the same content non-streaming; flip
        // the streamed one to the same state first. The lazy streaming flag
        // changes the transform context and the frame, but must not rebuild
        // any Markdown instance (V19-04 FR-B R2).
        component.update_content(&shared_mk(51), false);
        assert_eq!(
            MARKDOWN_COUNT.with(|c| c.get()),
            before_fresh,
            "the streaming end must not reconstruct Markdown instances"
        );
        assert_eq!(
            component.render(100),
            fresh.render(100),
            "slot reuse must not change a single output byte"
        );
    }

    /// V15-13 FR-Bench (rpi#53 headline table): per-folded-delta
    /// `update_content` + `render` at 100-col width while a CJK thinking
    /// block accumulates to 10/50/100/200 KB. Issue #53 measured
    /// ~3.8/18.5/62/227 ms per delta (quadratic); the target is linear —
    /// 200 KB ≤ ~30 ms. `#[ignore]`d benchmark; run with
    /// `--release --ignored --nocapture`.
    #[test]
    #[ignore = "V15-13 FR-Bench: run explicitly with --release --ignored --nocapture"]
    fn folded_delta_update_render_bench() {
        let mut component = AssistantMessageComponent::new(
            None,
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
            Vec::new(),
        );
        // ~600 B CJK per delta (24 lines x 8 chars + newlines).
        let delta_line = "思考推理内容测试行";

        let mut accumulated = String::new();
        let mut next_checkpoint = 0usize;
        let checkpoints_kb: [usize; 4] = [10, 50, 100, 200];
        let mut checkpoint_idx = 0usize;
        // Regression guard (V15-13): per-delta cost must stay LINEAR in
        // content size. The quadratic pre-fix measured ~3.8ms @10KB vs
        // ~227ms @200KB (~60x); the linear target is ~0.45ms vs ~8ms (~18x).
        // 25x is a generous ceiling that still fails loudly on a
        // quadratic regression. Manual-run only (this bench is
        // `#[ignore]`d; run with --release --ignored --nocapture).
        let mut first_checkpoint_elapsed: Option<std::time::Duration> = None;
        let mut last_checkpoint_elapsed: Option<std::time::Duration> = None;
        loop {
            for _ in 0..24 {
                accumulated.push_str(delta_line);
                accumulated.push('\n');
            }
            let message = SharedAssistantMessage::new(message(vec![thinking(&accumulated)]));
            let start = std::time::Instant::now();
            component.update_content(&message, true);
            let lines = component.render(100);
            let elapsed = start.elapsed();
            next_checkpoint += 1;
            if checkpoint_idx < checkpoints_kb.len()
                && accumulated.len() >= checkpoints_kb[checkpoint_idx] * 1024
            {
                println!(
                    "folded delta @ ~{} KB ({} source lines, {} frame lines): {:?}",
                    accumulated.len() / 1024,
                    accumulated.lines().count(),
                    lines.len(),
                    elapsed
                );
                if checkpoint_idx == 0 {
                    first_checkpoint_elapsed = Some(elapsed);
                }
                if checkpoint_idx == checkpoints_kb.len() - 1 {
                    last_checkpoint_elapsed = Some(elapsed);
                }
                checkpoint_idx += 1;
            }
            if checkpoint_idx >= checkpoints_kb.len() {
                break;
            }
            let _ = next_checkpoint;
        }
        if let (Some(first), Some(last)) = (first_checkpoint_elapsed, last_checkpoint_elapsed) {
            assert!(
                last <= first * 25,
                "per-delta cost regressed towards quadratic: 10KB {first:?} vs 200KB {last:?} \
                 (linear target is ~18x; the pre-fix quadratic measured ~60x)"
            );
        } else {
            panic!("bench did not reach all checkpoints");
        }
    }

    /// Multi-block interleavings: appending a new text block in the middle
    /// (before the trailer) reconstructs only the new block; toggling a
    /// thinking run's visibility reconstructs only that run.
    #[test]
    fn block_diff_scopes_rebuilds_to_changed_slots() {
        let mut component = AssistantMessageComponent::new(
            Some(message(vec![
                thinking("第一段思考"),
                text("first answer"),
                thinking("第二段思考"),
                text("second answer"),
            ])),
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
            Vec::new(),
        );
        // Initial: 4 Markdown children (2 thinking runs + 2 text blocks).
        MARKDOWN_COUNT.with(|count| count.set(0));
        component.update_content(
            &shared_message(vec![
                thinking("第一段思考"),
                text("first answer"),
                thinking("第二段思考"),
                text("second answer"),
            ]),
            true,
        );
        assert_eq!(MARKDOWN_COUNT.with(|c| c.get()), 0, "slots reused");

        // Same content, is_streaming flip: the lazy flag changes the frame
        // but not the instances.
        component.update_content(
            &shared_message(vec![
                thinking("第一段思考"),
                text("first answer"),
                thinking("第二段思考"),
                text("second answer"),
            ]),
            false,
        );
        assert_eq!(MARKDOWN_COUNT.with(|c| c.get()), 0);

        // A stop-reason change only touches the trailer slots: zero
        // Markdown reconstructions.
        let mut aborted = message(vec![
            thinking("第一段思考"),
            text("first answer"),
            thinking("第二段思考"),
            text("second answer"),
        ]);
        aborted.stop_reason = StopReason::Aborted;
        aborted.error_message = Some("Request was aborted".to_string());
        component.update_content(&shared_message_from(aborted), false);
        assert_eq!(
            MARKDOWN_COUNT.with(|c| c.get()),
            0,
            "trailer-only change must not touch any Markdown child"
        );
        let rendered = strip_ansi(&component.render(80).join("\n"));
        assert!(rendered.contains("Operation aborted"));

        // Toggling run 0 hidden removes its Markdown slot; the following
        // slots shift by one position, so position+kind reuse reconstructs
        // them (upstream `markdownSlots` semantics).
        component.handle_mouse(&click(1));
        assert_eq!(
            MARKDOWN_COUNT.with(|c| c.get()),
            3,
            "hidden label is a Text; shifted positions rebuild"
        );
        // Toggling back shifts the positions again.
        component.handle_mouse(&click(1));
        assert_eq!(MARKDOWN_COUNT.with(|c| c.get()), 7);
        let rendered = strip_ansi(&component.render(80).join("\n"));
        assert!(rendered.contains("第一段思考"));
    }

    #[test]
    fn renders_text_with_osc133_markers() {
        let mut component = AssistantMessageComponent::new(
            Some(message(vec![text("hello")])),
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
            Vec::new(),
        );
        let lines = component.render(40);
        assert!(lines[0].starts_with("\u{1b}]133;A\u{7}"));
        assert!(lines[lines.len() - 1].starts_with("\u{1b}]133;B\u{7}\u{1b}]133;C\u{7}"));
        // Text renders with outputPad 1: " hello " (first content line).
        let hello_line = lines
            .iter()
            .find(|l| strip_ansi(l).contains("hello"))
            .expect("hello line");
        assert!(strip_ansi(hello_line).contains(" hello "));

        // Tool calls disable the OSC markers (rendered separately).
        component.update_content(
            &shared_message(vec![
                text("hi"),
                AssistantContent::ToolCall(Default::default()),
            ]),
            false,
        );
        let lines = component.render(40);
        assert!(!lines[0].starts_with("\u{1b}]133;A\u{7}"));
    }

    #[test]
    fn hide_thinking_block_shows_italic_label() {
        let component = AssistantMessageComponent::new(
            Some(message(vec![thinking("deep thoughts")])),
            true,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
            Vec::new(),
        );
        let lines = component.render(40);
        let stripped = strip_ansi(&lines.join("\n"));
        assert!(stripped.contains("Thinking..."));
        // The label is italic-wrapped.
        assert!(lines.iter().any(|l| l.contains("\u{1b}[3m")));
        assert!(!stripped.contains("deep thoughts"));
    }

    #[test]
    fn thinking_blocks_merge_and_render_markdown() {
        let component = AssistantMessageComponent::new(
            Some(message(vec![
                thinking("first"),
                thinking("second"),
                text("after"),
            ])),
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
            Vec::new(),
        );
        let lines = component.render(40);
        let stripped = strip_ansi(&lines.join("\n"));
        // Both thinking blocks rendered, blank-line separated.
        assert!(stripped.contains("first"));
        assert!(stripped.contains("second"));
        assert!(stripped.contains("after"));
        // Italic thinking styling is applied.
        assert!(lines.iter().any(|l| l.contains("\u{1b}[3m")));
    }

    #[test]
    fn stop_reason_length_appends_error_line() {
        let mut m = message(vec![text("partial")]);
        m.stop_reason = StopReason::Length;
        let component = AssistantMessageComponent::new(
            Some(m),
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
            Vec::new(),
        );
        let stripped = strip_ansi(&component.render(80).join("\n"));
        assert!(stripped.contains("Response was truncated before completion."));
    }

    #[test]
    fn stop_reason_aborted_appends_message() {
        let mut m = message(vec![text("partial")]);
        m.stop_reason = StopReason::Aborted;
        m.error_message = Some("Request was aborted".to_string());
        let component = AssistantMessageComponent::new(
            Some(m),
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
            Vec::new(),
        );
        let stripped = strip_ansi(&component.render(40).join("\n"));
        assert!(stripped.contains("Operation aborted"));

        let mut m2 = message(vec![]);
        m2.stop_reason = StopReason::Aborted;
        m2.error_message = Some("User pressed escape".to_string());
        let component2 = AssistantMessageComponent::new(
            Some(m2),
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
            Vec::new(),
        );
        let stripped = strip_ansi(&component2.render(40).join("\n"));
        assert!(stripped.contains("User pressed escape"));
    }

    #[test]
    fn stop_reason_error_appends_error_prefix() {
        let mut m = message(vec![text("partial")]);
        m.stop_reason = StopReason::Error;
        m.error_message = Some("boom".to_string());
        let component = AssistantMessageComponent::new(
            Some(m),
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
            Vec::new(),
        );
        let stripped = strip_ansi(&component.render(40).join("\n"));
        assert!(stripped.contains("Error: boom"));
    }

    #[test]
    fn setters_rebuild_content() {
        let mut component = AssistantMessageComponent::new(
            Some(message(vec![thinking("secret")])),
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
            Vec::new(),
        );
        assert!(strip_ansi(&component.render(40).join("\n")).contains("secret"));
        component.set_hide_thinking_block(true);
        assert!(!strip_ansi(&component.render(40).join("\n")).contains("secret"));
        component.set_hidden_thinking_label("Deliberating");
        assert!(strip_ansi(&component.render(40).join("\n")).contains("Deliberating"));
    }

    #[test]
    fn applies_markdown_transform_chain_with_assistant_context() {
        // T29: the extension transformer chain applies in order; the context
        // carries messageType "assistant" and the (non-streaming) flag of
        // the constructor path (assistant-message.ts:110-113).
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_a = Arc::clone(&seen);
        let seen_b = Arc::clone(&seen);
        let component = AssistantMessageComponent::new(
            Some(message(vec![text("hello")])),
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
            vec![
                Arc::new(move |md, ctx| {
                    seen_a
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(("a".to_owned(), ctx.clone()));
                    format!("[a]{md}")
                }),
                Arc::new(move |md, ctx| {
                    seen_b
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(("b".to_owned(), ctx.clone()));
                    format!("{md}[b]")
                }),
            ],
        );
        let stripped = strip_ansi(&component.render(40).join("\n"));
        assert!(stripped.contains("[a]hello[b]"), "stripped: {stripped}");

        let calls = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "a");
        assert_eq!(calls[1].0, "b");
        for (_, context) in &calls {
            assert_eq!(context.message_type, "assistant");
            assert!(!context.is_streaming);
            // width - padding_x * 2 = 40 - 2.
            assert_eq!(context.available_width, 38);
        }
    }

    #[test]
    fn thinking_transform_context_uses_streaming_flag() {
        // assistant-message.ts:156-162: thinking blocks transform with
        // messageType "assistant-thinking"; a streaming update marks
        // isStreaming true.
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_inner = Arc::clone(&seen);
        let mut component = AssistantMessageComponent::new(
            None,
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
            vec![Arc::new(move |md, ctx| {
                seen_inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(ctx.clone());
                md
            })],
        );
        component.update_content(&shared_message(vec![thinking("deep")]), true);
        let _ = component.render(50);
        let calls = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].message_type, "assistant-thinking");
        assert!(calls[0].is_streaming);
        // width - padding_x * 2 = 50 - 2.
        assert_eq!(calls[0].available_width, 48);
    }

    // ------------------------------------------------------------------
    // V14-14: thinking-block click toggle (assistant-message.ts:141-171 @
    // 9841914, 71026970a — FR-A R9)
    // ------------------------------------------------------------------

    use rpi_tui::tui::{TuiMouseButton, TuiMouseEvent, TuiMouseEventType, TuiMouseHandlerResult};

    fn click(y: isize) -> TuiMouseEvent {
        TuiMouseEvent {
            event_type: TuiMouseEventType::Click,
            button: TuiMouseButton::Left,
            x: 2,
            y,
            screen_x: 2,
            screen_y: y,
            width: 40,
            height: 20,
            shift: false,
            alt: false,
            ctrl: false,
            wheel_delta: None,
            click_count: Some(1),
        }
    }

    #[test]
    fn clicking_a_thinking_block_toggles_its_visibility() {
        let mut component = AssistantMessageComponent::new(
            Some(message(vec![thinking("secret thoughts"), text("answer")])),
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
            Vec::new(),
        );
        // Initially visible (hideThinkingBlock=false).
        assert!(strip_ansi(&component.render(60).join("\n")).contains("secret thoughts"));

        // Click row 1 (the spacer is row 0, the thinking block follows).
        match component.handle_mouse(&click(1)) {
            Some(TuiMouseHandlerResult::Event(event)) => assert!(event.handled),
            _ => panic!("click on the thinking block is handled"),
        }
        // Collapsed to the label after the click.
        assert!(!strip_ansi(&component.render(60).join("\n")).contains("secret thoughts"));
        assert!(strip_ansi(&component.render(60).join("\n")).contains("Thinking..."));

        // Clicking the label expands it again.
        component.handle_mouse(&click(1));
        assert!(strip_ansi(&component.render(60).join("\n")).contains("secret thoughts"));

        // Right-button and non-click events do nothing.
        let mut right = click(1);
        right.button = TuiMouseButton::Right;
        assert!(component.handle_mouse(&right).is_none());
        let mut press = click(1);
        press.event_type = TuiMouseEventType::Press;
        assert!(component.handle_mouse(&press).is_none());
        assert!(strip_ansi(&component.render(60).join("\n")).contains("secret thoughts"));
    }

    #[test]
    fn thinking_overrides_survive_content_updates_and_clear_on_global_toggle() {
        let mut component = AssistantMessageComponent::new(
            Some(message(vec![
                thinking("first secret"),
                text("middle"),
                thinking("second secret"),
                text("answer"),
            ])),
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
            Vec::new(),
        );
        // Two runs separated by text. Collapse the first (spacer row 0,
        // run 0 starts at row 1).
        component.handle_mouse(&click(1));
        let rendered = strip_ansi(&component.render(60).join("\n"));
        assert!(!rendered.contains("first secret"), "{rendered}");
        assert!(rendered.contains("second secret"), "{rendered}");

        // A streaming update with identical content keeps the override.
        component.update_content(
            &shared_message(vec![
                thinking("first secret"),
                text("middle"),
                thinking("second secret"),
                text("answer"),
            ]),
            true,
        );
        let rendered = strip_ansi(&component.render(60).join("\n"));
        assert!(
            !rendered.contains("first secret"),
            "override survives updates: {rendered}"
        );

        // Toggling the global setting clears the overrides
        // (assistant-message.ts:60).
        component.set_hide_thinking_block(false);
        let rendered = strip_ansi(&component.render(60).join("\n"));
        assert!(rendered.contains("first secret"));
    }

    #[test]
    fn hidden_thinking_with_an_empty_label_has_zero_rows_or_spacers() {
        let mut component = AssistantMessageComponent::new(
            Some(message(vec![thinking("secret"), text("answer")])),
            true,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            0,
            Vec::new(),
        );
        component.set_hidden_thinking_label("");
        let rendered = component
            .content_container
            .render(60)
            .iter()
            .map(|line| strip_ansi(line).trim_end().to_owned())
            .collect::<Vec<_>>();
        assert!(!rendered.join("\n").contains("secret"));
        assert_eq!(
            rendered,
            ["", "answer"],
            "only the normal leading spacer remains"
        );

        component.update_content(
            &shared_message(vec![
                thinking("secret"),
                AssistantContent::ToolCall(Default::default()),
            ]),
            false,
        );
        assert!(
            component
                .render(60)
                .iter()
                .all(|line| line.trim().is_empty())
        );
    }

    // ------------------------------------------------------------------
    // V19-04 FR-E: component revision contract.
    // ------------------------------------------------------------------

    #[test]
    fn cache_revision_is_monotonic_and_tracks_lazy_streaming() {
        let transformer: rpi_ext_host::types::MarkdownTransformerFn =
            Arc::new(|markdown, _context| markdown);
        let mut component = AssistantMessageComponent::new(
            None,
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
            vec![transformer],
        );
        component.set_transform_revision_trusted(true);

        let with_tool = |text_value: &str, args: &str| {
            let mut tool = AssistantContent::ToolCall(Default::default());
            if let AssistantContent::ToolCall(call) = &mut tool {
                call.arguments.insert(
                    "command".to_string(),
                    serde_json::Value::String(args.to_string()),
                );
            }
            shared_message(vec![text(text_value), tool])
        };

        component.update_content(&with_tool("hello", "one"), true);
        let first = component
            .cache_revision()
            .expect("trusted built-in transform revision");
        assert_eq!(component.cache_revision(), Some(first));

        // A tool-argument-only delta is not a visible change: no rebuild, no
        // revision bump.
        component.update_content(&with_tool("hello", "two"), true);
        assert_eq!(component.cache_revision(), Some(first));

        // The lazy streaming flag changes the transform context and the child
        // Markdown revision without rebuilding the instance.
        component.update_content(&with_tool("hello", "two"), false);
        let second = component.cache_revision().expect("revision");
        assert!(second > first, "streaming flip must bump the revision");

        // A visible text change rebuilds and bumps again.
        component.update_content(&with_tool("hello world", "two"), false);
        let third = component.cache_revision().expect("revision");
        assert!(third > second, "text change must bump the revision");
    }

    #[test]
    fn cache_revision_is_none_for_untrusted_transform_chains() {
        let transformer: rpi_ext_host::types::MarkdownTransformerFn =
            Arc::new(|markdown, _context| markdown);
        let mut component = AssistantMessageComponent::new(
            None,
            false,
            theme(),
            markdown_theme(&load_theme("dark", None).unwrap()),
            "Thinking...",
            1,
            vec![transformer],
        );
        component.update_content(&shared_message(vec![text("hello")]), true);
        assert_eq!(component.cache_revision(), None);
    }
}
