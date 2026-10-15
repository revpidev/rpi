//! Tool execution rendering — port of
//! `packages/coding-agent/src/modes/interactive/components/tool-execution.ts`
//! @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - The theme is passed explicitly (`Arc<Theme>`) instead of read from the
//!   global `theme` getter (theme.ts:799-816).
//! - Built-in tool definitions (`createAllToolDefinitions(cwd)[toolName]`,
//!   tool-execution.ts:57) live in
//!   [`crate::modes::interactive::tool_renderers`] (T17): one
//!   [`ToolDefinition`] per built-in tool, looked up by tool name in the
//!   constructor. Renderer selection merges the extension definition and the
//!   built-in definition per hook — extension hook wins, a missing/failed
//!   hook falls through to the built-in one (tool-execution.ts:81-99); the
//!   generic fallback (`contentText` + `formatToolExecution`,
//!   tool-execution.ts:315-319, 365-376) remains for tools with neither.
//! - Upstream catches renderer exceptions and falls back
//!   (tool-execution.ts:274-283, 295-311); Rust has no safe cross-component
//!   catch, so [`ToolDefinition`] methods signal failure via
//!   `Option<Component>` return instead — `None` falls through to the
//!   built-in definition, then to the generic fallback (documented T15
//!   contract).
//! - Kitty image conversion happens inside the `rpi-tui` `Image` component
//!   through the registered PNG transcoder (`utils/image_convert.rs`,
//!   upstream `imageSources` reuse + `ensurePngTranscoder`,
//!   tool-execution.ts:340-385 + utils/image-convert.ts:78): the component keeps
//!   converted PNG data across updates and reuses `Image` instances whose
//!   source bytes did not change. Upstream's async conversion has a
//!   synchronous equivalent because the `image` crate is compiled in.
//! - `invalidate()` in the render context is replaced by a [`RenderHandle`]:
//!   upstream `invalidate: () => { this.invalidate(); this.ui.requestRender(); }`
//!   (tool-execution.ts:118-122) needs `&mut self` access; built-in renderers
//!   instead recompute their dynamic parts (bash Elapsed line) at `render()`
//!   time from shared state and only need `request_render` (T17).
//! - The renderer state (`rendererState: any`, tool-execution.ts:19) is a
//!   typed, lazily-initialized [`RendererStateSlot`] (T17): the renderer
//!   downcasts to its own state type instead of upstream's shared `any`
//!   object. `lastComponent` (tool-execution.ts:117, 124) has no equivalent —
//!   components are rebuilt from state on every update; renderers keep
//!   cross-render state in the slot.

use std::any::Any;
use std::boxed::Box as StdBox;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rpi_tui::components::r#box::Box as TuiBox;
use rpi_tui::components::image::{Image, ImageOptions, ImageTheme};
use rpi_tui::components::mouse_region::MouseRegion;
use rpi_tui::components::spacer::Spacer;
use rpi_tui::components::text::Text;
use rpi_tui::terminal_image::{get_capabilities, get_image_dimensions, image_fallback};
use rpi_tui::tui::{
    Component, Container, RenderHandle, TuiMouseButton, TuiMouseEvent, TuiMouseEventResult,
    TuiMouseEventType, TuiMouseHandlerResult, shared_component_from_boxed,
};
use serde_json::Value;

use crate::core::themes::Theme;
use crate::tools::sanitize::{sanitize_binary_output, strip_ansi};

use super::keybinding_hints::key_hint;
use super::visual_truncate::{VisualKeep, VisualLinePreview};

/// `FALLBACK_PREVIEW_LINES` (tool-execution.ts:40, e14afc648 / V14-11 FR-G):
/// collapsed generic-fallback preview height.
const FALLBACK_PREVIEW_LINES: usize = 10;

/// `ToolExecutionOptions` (tool-execution.ts:8-11).
#[derive(Debug, Clone, Copy)]
pub struct ToolExecutionOptions {
    pub show_images: bool,
    pub image_width_cells: usize,
    /// `outputPad` (tool-execution.ts:28, #10557): horizontal padding for
    /// the transcript block; defaults to 1 like upstream.
    pub output_pad: usize,
}

impl Default for ToolExecutionOptions {
    fn default() -> Self {
        Self {
            show_images: true,
            image_width_cells: 60,
            output_pad: 1,
        }
    }
}

/// Lock a mutex, recovering from poisoning (same pattern as
/// `interactive_mode.rs`'s `lock`).
pub(crate) fn lock_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Typed per-component renderer state (`rendererState: any`,
/// tool-execution.ts:19) — T17. The slot is empty until the tool's renderer
/// initializes it with its own state type via [`RendererStateSlot::get_or_init`];
/// shared by every render context handed to the renderer, so `renderCall` and
/// `renderResult` see the same state across updates.
#[derive(Debug, Clone, Default)]
pub struct RendererStateSlot {
    inner: Arc<Mutex<Option<Arc<dyn Any + Send + Sync>>>>,
}

impl RendererStateSlot {
    /// Get the renderer's typed state, lazily initializing it on first use.
    ///
    /// One component has at most one tool definition, so at most one state
    /// type ever lands in a slot; the downcast cannot fail.
    pub fn get_or_init<T: Default + Send + Sync + 'static>(&self) -> Arc<T> {
        let mut guard = lock_recover(&self.inner);
        let slot = guard.get_or_insert_with(|| Arc::new(T::default()));
        Arc::clone(slot)
            .downcast::<T>()
            .expect("renderer state type is stable per component")
    }
}

/// Loose tool-result content item (tool-execution.ts:35-39): partial results
/// during streaming only carry the fields set so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResultContentLoose {
    pub kind: String,
    pub text: Option<String>,
    pub data: Option<String>,
    pub mime_type: Option<String>,
}

impl ToolResultContentLoose {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            kind: "text".to_string(),
            text: Some(text.into()),
            data: None,
            mime_type: None,
        }
    }

    pub fn image(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self {
            kind: "image".to_string(),
            text: None,
            data: Some(data.into()),
            mime_type: Some(mime_type.into()),
        }
    }
}

/// The `result` field of the component (tool-execution.ts:35-39).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResultState {
    pub content: Vec<ToolResultContentLoose>,
    pub is_error: bool,
    pub details: Option<Value>,
    /// `durationMs` (agent-loop.ts:919-928 / types.ts `ToolResultMessage`):
    /// recorded execution time, absent for legacy/deferred results. The TUI
    /// consumes it for the bash `Took` line (V17-05 FR-C R3); the
    /// `tool_execution_end` event payload field is V17-06.
    pub duration_ms: Option<u64>,
}

/// `RenderShell` (extensions/types.ts `renderShell`): whether the tool
/// definition renders its own framing ("self") or uses the component's box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderShell {
    Default,
    Self_,
}

/// Render context passed to tool renderers (`ToolRenderContext`,
/// extensions/types.ts) — see the module doc for the `invalidate` note.
///
/// `lastComponent` (tool-execution.ts:117, 124) has no equivalent: renderers
/// keep cross-render state in `state` and are rebuilt from it on every
/// update (T17).
#[derive(Debug, Clone)]
pub struct ToolRenderContext {
    pub args: Value,
    pub tool_call_id: String,
    pub render_handle: RenderHandle,
    pub state: RendererStateSlot,
    pub cwd: String,
    pub execution_started: bool,
    pub args_complete: bool,
    pub is_partial: bool,
    pub expanded: bool,
    pub show_images: bool,
    pub is_error: bool,
    /// `durationMs` (tool-execution.ts:137): recorded execution time of the
    /// finished result; `None` while partial or for legacy results. Internal
    /// field first — the extension ABI field opens in V17-06.
    pub duration_ms: Option<u64>,
    /// `outputPad` (extensions/types.ts:489-493, #10557): horizontal padding
    /// configured by the setting; renderers with `renderShell: "self"`
    /// apply it themselves. Internal field first — the extension ABI field
    /// opens in V17-06.
    pub output_pad: usize,
    /// Viewport width the component last rendered at (0 before the first
    /// render). Forwarded to extension renderers so progress bars size to
    /// the actual terminal instead of a fixed width (rpi extension over
    /// upstream, which has no width on the context).
    pub terminal_width: usize,
}

/// Options passed to `renderResult` (tool-execution.ts:296-298).
#[derive(Debug, Clone, Copy)]
pub struct ResultRenderOptions {
    pub expanded: bool,
    pub is_partial: bool,
}

/// T15 hook: the extension/tool `renderCall`/`renderResult`/`renderShell`
/// contract (extensions/types.ts). While no definitions exist, the component
/// falls back to the generic text rendering.
pub trait ToolDefinition: Send + Sync {
    /// `renderCall` (tool-execution.ts:275): the component rendered for the
    /// tool call; `None` falls back to [`create_call_fallback`].
    fn render_call(
        &self,
        args: &Value,
        theme: &Theme,
        context: &ToolRenderContext,
    ) -> Option<StdBox<dyn Component>>;

    /// `renderResult` (tool-execution.ts:296): the component rendered for
    /// the tool result; `None` falls back to [`create_result_fallback`].
    fn render_result(
        &self,
        result: &ToolResultState,
        options: ResultRenderOptions,
        theme: &Theme,
        context: &ToolRenderContext,
    ) -> Option<StdBox<dyn Component>>;

    /// `renderShell` (tool-execution.ts:105-113): `None` when the definition
    /// does not provide one (upstream `undefined`), so the merge can tell
    /// "absent" apart from an explicit `"default"`.
    fn render_shell(&self) -> Option<RenderShell>;
}

/// Component that renders a tool call and its result
/// (tool-execution.ts:13-377).
pub struct ToolExecutionComponent {
    // The three render shells; exactly one is displayed, selected by
    // `has_renderer_definition` + `render_shell` (tool-execution.ts:63-76).
    content_box: TuiBox,
    /// `contentTextRegion` (tool-execution.ts:50, 104 @ 9841914,
    /// `71026970a`): the generic fallback `Text` wrapped in a
    /// click-to-expand `MouseRegion`. Rebuilt in `update_display`
    /// (upstream mutates one long-lived Text object; the port recreates the
    /// stateless Text per rebuild, which is observably identical).
    content_text_region: Option<MouseRegion>,
    self_render_container: Container,
    renderer_state: RendererStateSlot,
    image_components: Vec<Image>,
    image_spacers: Vec<Spacer>,
    tool_name: String,
    tool_call_id: String,
    args: Value,
    expanded: bool,
    /// Click-to-expand toggle request recorded by the result regions'
    /// `MouseRegion` callbacks (`createResultRegion`, tool-execution.ts:
    /// 172-178 @ 9841914); applied by [`Self::drain_pending_expand_toggle`]
    /// after the dispatch returns (the component is locked then).
    pending_expand_toggle: Arc<AtomicBool>,
    /// `this.result` presence, shared with the same callbacks (upstream
    /// checks `!this.result` before toggling, tool-execution.ts:174).
    has_result: Arc<AtomicBool>,
    show_images: bool,
    image_width_cells: usize,
    /// `outputPad` (tool-execution.ts:50, #10557).
    output_pad: usize,
    is_partial: bool,
    tool_definition: Option<Arc<dyn ToolDefinition>>,
    built_in_tool_definition: Option<Arc<dyn ToolDefinition>>,
    render_handle: RenderHandle,
    cwd: String,
    execution_started: bool,
    args_complete: bool,
    result: Option<ToolResultState>,
    /// Inputs of `image_components`, so `update_display` can reuse images
    /// and keep their converted PNG data (`imageSources`,
    /// tool-execution.ts:42 @ a276dabe5, #10292).
    image_sources: Vec<ImageSource>,
    hide_component: bool,
    theme: Arc<Theme>,
    /// Last render width (`render` is `&self`; interior mutability keeps the
    /// context's `terminal_width` current without re-borrowing the tree).
    last_render_width: std::sync::atomic::AtomicUsize,
}

impl ToolExecutionComponent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tool_name: impl Into<String>,
        tool_call_id: impl Into<String>,
        args: Value,
        options: ToolExecutionOptions,
        tool_definition: Option<Arc<dyn ToolDefinition>>,
        theme: Arc<Theme>,
        render_handle: RenderHandle,
        cwd: impl Into<String>,
    ) -> Self {
        let tool_name = tool_name.into();
        // `builtInToolDefinition = createAllToolDefinitions(cwd)[toolName]`
        // (tool-execution.ts:57) — T17 registry, render hooks only.
        let built_in_tool_definition =
            crate::modes::interactive::tool_renderers::builtin_tool_definition(&tool_name);
        let mut component = Self {
            content_box: TuiBox::new(0, 0, None),
            content_text_region: None,
            self_render_container: Container::new(),
            renderer_state: RendererStateSlot::default(),
            image_components: Vec::new(),
            image_spacers: Vec::new(),
            tool_name,
            tool_call_id: tool_call_id.into(),
            args,
            expanded: false,
            pending_expand_toggle: Arc::new(AtomicBool::new(false)),
            has_result: Arc::new(AtomicBool::new(false)),
            show_images: options.show_images,
            image_width_cells: options.image_width_cells,
            output_pad: options.output_pad,
            is_partial: true,
            tool_definition,
            built_in_tool_definition,
            render_handle,
            cwd: cwd.into(),
            execution_started: false,
            args_complete: false,
            result: None,
            image_sources: Vec::new(),
            hide_component: false,
            theme,
            last_render_width: std::sync::atomic::AtomicUsize::new(0),
        };

        // Always create all shell variants (tool-execution.ts:65-76):
        // contentBox for default renderer-based composition, selfRenderContainer
        // for self-framing renderers, contentText for the generic fallback
        // (built lazily by `update_display` into `content_text_region`).
        let pending_bg = {
            let theme = Arc::clone(&component.theme);
            Box::new(move |t: &str| theme.bg("toolPendingBg", t))
        };
        component.content_box = TuiBox::new(1, 1, Some(pending_bg));

        component.update_display();
        component
    }

    /// `getRenderContext` (tool-execution.ts:115-133).
    fn get_render_context(&self) -> ToolRenderContext {
        ToolRenderContext {
            args: self.args.clone(),
            tool_call_id: self.tool_call_id.clone(),
            render_handle: self.render_handle.clone(),
            state: self.renderer_state.clone(),
            cwd: self.cwd.clone(),
            execution_started: self.execution_started,
            args_complete: self.args_complete,
            is_partial: self.is_partial,
            expanded: self.expanded,
            show_images: self.show_images,
            is_error: self.result.as_ref().is_some_and(|r| r.is_error),
            duration_ms: if self.is_partial {
                None
            } else {
                self.result.as_ref().and_then(|r| r.duration_ms)
            },
            output_pad: self.output_pad,
            terminal_width: self
                .last_render_width
                .load(std::sync::atomic::Ordering::Relaxed),
        }
    }

    /// `createCallFallback` (tool-execution.ts:135-137 @ 5257d0d5f): the
    /// generic header with the arguments (V16-13 FR-F R1).
    fn create_call_fallback(&self) -> Text {
        Text::new(
            super::super::tool_renderers::render_utils::format_tool_call_with_args(
                &self.tool_name,
                &self.args,
                &self.theme,
                self.expanded,
            ),
            0,
            0,
            None,
        )
    }

    /// `createResultRegion` (tool-execution.ts:172-178 @ 9841914,
    /// `71026970a`): wrap a rendered component so a left click toggles the
    /// expanded state (only once a result exists). The callback records the
    /// toggle; the component applies it after the dispatch returns (it is
    /// locked for `handle_mouse` — upstream flips `setExpanded` synchronously
    /// inside the callback).
    fn create_result_region(&self, component: StdBox<dyn Component>) -> MouseRegion {
        let pending = Arc::clone(&self.pending_expand_toggle);
        let has_result = Arc::clone(&self.has_result);
        MouseRegion::new(
            shared_component_from_boxed(component),
            Box::new(move |event| {
                if !has_result.load(Ordering::SeqCst)
                    || event.event_type != TuiMouseEventType::Click
                    || event.button != TuiMouseButton::Left
                {
                    return None;
                }
                pending.store(true, Ordering::SeqCst);
                Some(TuiMouseEventResult {
                    handled: true,
                    ..Default::default()
                })
            }),
        )
    }

    /// Apply a click-recorded expand toggle (`setExpanded(!this.expanded)`,
    /// tool-execution.ts:175 @ 9841914).
    fn drain_pending_expand_toggle(&mut self) {
        if self.pending_expand_toggle.swap(false, Ordering::SeqCst) && self.result.is_some() {
            let expanded = self.expanded;
            self.set_expanded(!expanded);
        }
    }

    /// `createResultFallback` (tool-execution.ts:139-145 @ 9841914,
    /// e14afc648/V14-11 FR-G): collapsed shows the first
    /// [`FALLBACK_PREVIEW_LINES`] **visual** lines with a
    /// `... (N more lines, …)` hint
    /// (`keyHint("app.tools.expand", "to expand")`); expanded shows the
    /// full output. Only the generic fallback — renderers and built-in
    /// tools are unaffected.
    fn create_result_fallback(&self) -> Option<StdBox<dyn Component>> {
        let output = get_text_output(self.result.as_ref(), self.show_images);
        if output.is_empty() {
            return None;
        }
        let styled = output
            .split('\n')
            .map(|line| self.theme.fg("toolOutput", line))
            .collect::<Vec<_>>()
            .join("\n");
        if self.expanded {
            // `new Text(`\n${styled}`)` (tool-execution.ts:316-322):
            // upstream renders a Text that wraps at width.
            return Some(StdBox::new(Text::new(styled, 0, 0, None)));
        }
        let theme = Arc::clone(&self.theme);
        Some(StdBox::new(
            VisualLinePreview::new(styled, FALLBACK_PREVIEW_LINES, VisualKeep::Start).with_hint(
                Box::new(move |hidden| {
                    format!(
                        "{} {}{}",
                        theme.fg("muted", &format!("... ({hidden} more lines,")),
                        key_hint(&theme, "app.tools.expand", "to expand"),
                        theme.fg("muted", ")"),
                    )
                }),
            ),
        ))
    }

    /// `updateArgs` (tool-execution.ts:147-150).
    pub fn update_args(&mut self, args: Value) {
        self.args = args;
        self.update_display();
    }

    /// `markExecutionStarted` (tool-execution.ts:152-156).
    pub fn mark_execution_started(&mut self) {
        self.execution_started = true;
        self.update_display();
        self.render_handle.request_render();
    }

    /// `setArgsComplete` (tool-execution.ts:158-162).
    pub fn set_args_complete(&mut self) {
        self.args_complete = true;
        self.update_display();
        self.render_handle.request_render();
    }

    /// `updateResult` (tool-execution.ts:164-176).
    pub fn update_result(&mut self, result: ToolResultState, is_partial: bool) {
        self.result = Some(result);
        self.has_result.store(true, Ordering::SeqCst);
        self.is_partial = is_partial;
        self.update_display();
    }

    /// `setExpanded` (tool-execution.ts:201-204).
    pub fn set_expanded(&mut self, expanded: bool) {
        self.expanded = expanded;
        self.update_display();
    }

    /// `setShowImages` (tool-execution.ts:206-209).
    pub fn set_show_images(&mut self, show: bool) {
        self.show_images = show;
        self.update_display();
    }

    /// `setOutputPad` (tool-execution.ts:207-210 @ 18336987a, #10557).
    pub fn set_output_pad(&mut self, output_pad: usize) {
        self.output_pad = output_pad;
        self.update_display();
    }

    /// `setImageWidthCells` (tool-execution.ts:211-214).
    pub fn set_image_width_cells(&mut self, width: usize) {
        self.image_width_cells = width.max(1);
        self.update_display();
    }

    /// `hasRendererDefinition` (tool-execution.ts:101-103): either the
    /// extension-registered definition or the built-in one (T17) counts.
    fn has_renderer_definition(&self) -> bool {
        self.tool_definition.is_some() || self.built_in_tool_definition.is_some()
    }

    /// `getRenderShell` (tool-execution.ts:105-113):
    /// `ext.renderShell ?? builtin.renderShell ?? "default"` — any explicit
    /// extension value wins, including `"default"`.
    fn get_render_shell(&self) -> RenderShell {
        self.tool_definition
            .as_ref()
            .and_then(|def| def.render_shell())
            .or_else(|| {
                self.built_in_tool_definition
                    .as_ref()
                    .and_then(|def| def.render_shell())
            })
            .unwrap_or(RenderShell::Default)
    }

    /// The renderer-produced component for the call part
    /// (tool-execution.ts:81-89, 269-284): the extension definition's
    /// `renderCall` wins; a missing definition or a `None` (failed/absent
    /// hook) falls through to the built-in definition's `renderCall`, then
    /// to the bold-tool-title fallback.
    fn render_call_component(&self) -> StdBox<dyn Component> {
        let context = self.get_render_context();
        if let Some(def) = &self.tool_definition
            && let Some(component) = def.render_call(&self.args, &self.theme, &context)
        {
            return component;
        }
        if let Some(def) = &self.built_in_tool_definition
            && let Some(component) = def.render_call(&self.args, &self.theme, &context)
        {
            return component;
        }
        StdBox::new(self.create_call_fallback())
    }

    /// The renderer-produced component for the result part
    /// (tool-execution.ts:91-99, 286-314): same per-hook merge as the call
    /// part — extension `renderResult`, then the built-in one, then the
    /// text-output fallback; `None` when there is no result output at all.
    fn render_result_component(&self) -> Option<StdBox<dyn Component>> {
        self.result.as_ref()?;
        let context = self.get_render_context();
        let result = self.result.clone().expect("checked above");
        let options = ResultRenderOptions {
            expanded: self.expanded,
            is_partial: self.is_partial,
        };
        if let Some(def) = &self.tool_definition
            && let Some(component) = def.render_result(&result, options, &self.theme, &context)
        {
            return Some(component);
        }
        if let Some(def) = &self.built_in_tool_definition
            && let Some(component) = def.render_result(&result, options, &self.theme, &context)
        {
            return Some(component);
        }
        self.create_result_fallback()
    }

    /// `updateDisplay` (tool-execution.ts:253-359).
    fn update_display(&mut self) {
        let bg_fn: rpi_tui::components::text::ColorFn = if self.is_partial {
            Box::new({
                let theme = Arc::clone(&self.theme);
                move |t: &str| theme.bg("toolPendingBg", t)
            })
        } else if self.result.as_ref().is_some_and(|r| r.is_error) {
            Box::new({
                let theme = Arc::clone(&self.theme);
                move |t: &str| theme.bg("toolErrorBg", t)
            })
        } else {
            Box::new({
                let theme = Arc::clone(&self.theme);
                move |t: &str| theme.bg("toolSuccessBg", t)
            })
        };

        self.hide_component = false;

        if self.has_renderer_definition() {
            match self.get_render_shell() {
                RenderShell::Default => {
                    // contentBox is used for default renderer-based
                    // composition (tool-execution.ts:66-68). Every rendered
                    // child is wrapped in a click-to-expand result region
                    // (tool-execution.ts:315-353 @ 9841914).
                    self.content_box.set_bg_fn(Some(bg_fn));
                    self.content_box.set_padding_x(self.output_pad);
                    self.content_box.clear();
                    // The result component is BUILT before the call component
                    // (V13-11, rpi#18): a renderer's `renderResult` may
                    // backfill shared state that its own `renderCall` reads
                    // — upstream `edit.ts:416-424` rebuilds the call
                    // component in place after backfilling, which in the
                    // rebuild-per-update port (T17) maps to building the
                    // call component after the result side effect has
                    // landed. Adding order stays call → result; all other
                    // renderers are stateless across the two hooks, so the
                    // swap is unobservable for them.
                    let result_component = self.render_result_component();
                    self.content_box.add_child(StdBox::new(
                        self.create_result_region(self.render_call_component()),
                    ));
                    if let Some(component) = result_component {
                        self.content_box
                            .add_child(StdBox::new(self.create_result_region(component)));
                    }
                }
                RenderShell::Self_ => {
                    // selfRenderContainer is used when the tool renders its
                    // own framing (tool-execution.ts:66-68). Same
                    // result-before-call BUILD order as the Default shell
                    // (see the note above, V13-11) and the same
                    // click-to-expand wrapping (tool-execution.ts:315-353).
                    self.self_render_container.clear();
                    let result_component = self.render_result_component();
                    self.self_render_container.add_child(StdBox::new(
                        self.create_result_region(self.render_call_component()),
                    ));
                    if let Some(component) = result_component {
                        self.self_render_container
                            .add_child(StdBox::new(self.create_result_region(component)));
                    }
                }
            }
            // Upstream's `hasContent` is always true on the renderer path
            // (the call renderer always adds a component,
            // tool-execution.ts:270-284), so the `hideComponent` check
            // (tool-execution.ts:356-358) is dead code there; the port omits
            // the unobservable variable.
        } else {
            // `this.contentText.setCustomBgFn(...)` / `setText(...)`
            // (tool-execution.ts:360-361): the generic fallback text is
            // rebuilt into its click-to-expand region
            // (`contentTextRegion`, tool-execution.ts:104).
            self.content_text_region = Some(self.create_result_region(StdBox::new(Text::new(
                self.format_tool_execution(),
                self.output_pad,
                1,
                Some(bg_fn),
            ))));
        }

        // Images are appended after the shell (tool-execution.ts:355-382 @
        // a276dabe5, #10292): reuse `Image` instances whose source bytes did
        // not change so their converted PNG data survives updates; the
        // component itself transcodes non-PNG data on Kitty terminals.
        let mut previous_images: Vec<Option<Image>> =
            self.image_components.drain(..).map(Some).collect();
        let previous_sources = std::mem::take(&mut self.image_sources);
        self.image_spacers.clear();
        if let Some(result) = &self.result {
            let caps = get_capabilities();
            let image_blocks: Vec<&ToolResultContentLoose> = result
                .content
                .iter()
                .filter(|c| c.kind == "image")
                .collect();
            for img in image_blocks {
                if caps.images.is_none() || !self.show_images {
                    continue;
                }
                let (Some(data), Some(mime_type)) = (&img.data, &img.mime_type) else {
                    continue;
                };
                let source = ImageSource {
                    data: data.clone(),
                    mime_type: mime_type.clone(),
                    width_cells: self.image_width_cells,
                };
                let index = self.image_components.len();
                let reused = previous_sources
                    .get(index)
                    .filter(|previous| **previous == source)
                    .and_then(|_| previous_images.get_mut(index).and_then(Option::take));
                let fallback_color = {
                    let theme = Arc::clone(&self.theme);
                    Box::new(move |s: &str| theme.fg("toolOutput", s))
                };
                let image_component = reused.unwrap_or_else(|| {
                    Image::new(
                        source.data.clone(),
                        source.mime_type.clone(),
                        ImageTheme { fallback_color },
                        Some(ImageOptions {
                            max_width_cells: Some(source.width_cells),
                            ..Default::default()
                        }),
                        None,
                    )
                });
                if source.mime_type != "image/png" {
                    // Lets extension-provided images use the transcoder too;
                    // the UI-level registration in `init` already covers the
                    // normal flow (interactive-mode.ts:1001, 2062).
                    let render_handle = self.render_handle.clone();
                    crate::utils::image_convert::ensure_png_transcoder(move || {
                        render_handle.request_render();
                    });
                }
                self.image_spacers.push(Spacer::new(1));
                self.image_components.push(image_component);
                self.image_sources.push(source);
            }
        }
    }

    /// `formatToolExecution` (tool-execution.ts:365-376).
    fn format_tool_execution(&self) -> String {
        let mut text = self.theme.fg("toolTitle", &Theme::bold(&self.tool_name));
        // JSON.stringify(args, null, 2); JS `undefined` has no Rust
        // equivalent, `Value::Null` serializes as "null" (same as JS).
        let content = serde_json::to_string_pretty(&self.args).unwrap_or_default();
        if !content.is_empty() {
            text.push_str(&format!("\n\n{content}"));
        }
        let output = get_text_output(self.result.as_ref(), self.show_images);
        if !output.is_empty() {
            text.push_str(&format!("\n{output}"));
        }
        text
    }
}

impl Component for ToolExecutionComponent {
    fn render(&self, width: usize) -> Vec<String> {
        self.last_render_width
            .store(width, std::sync::atomic::Ordering::Relaxed);
        if self.hide_component {
            return Vec::new();
        }

        if self.has_renderer_definition() && self.get_render_shell() == RenderShell::Self_ {
            // Self-framing renderers manage their own layout
            // (tool-execution.ts:226-248).
            let content_lines = self.self_render_container.render(width);
            if content_lines.is_empty() && self.image_components.is_empty() {
                return Vec::new();
            }
            let mut lines: Vec<String> = Vec::new();
            if !content_lines.is_empty() {
                lines.push(String::new());
                lines.extend(content_lines);
            }
            for (i, image_component) in self.image_components.iter().enumerate() {
                if let Some(spacer) = self.image_spacers.get(i) {
                    lines.extend(spacer.render(width));
                }
                lines.extend(image_component.render(width));
            }
            return lines;
        }

        // Container children: [Spacer(1), shell, ...images]
        // (tool-execution.ts:63-76, 321-354).
        let mut lines = vec![String::new()];
        if self.has_renderer_definition() {
            match self.get_render_shell() {
                RenderShell::Default => lines.extend(self.content_box.render(width)),
                RenderShell::Self_ => unreachable!("handled above"),
            }
        } else {
            lines.extend(
                self.content_text_region
                    .as_ref()
                    .map(|region| region.render(width))
                    .unwrap_or_default(),
            );
        }
        for (i, image_component) in self.image_components.iter().enumerate() {
            if let Some(spacer) = self.image_spacers.get(i) {
                lines.extend(spacer.render(width));
            }
            lines.extend(image_component.render(width));
        }
        lines
    }

    fn invalidate(&mut self) {
        self.content_box.invalidate();
        if let Some(content_text_region) = self.content_text_region.as_mut() {
            content_text_region.invalidate();
        }
        self.self_render_container.invalidate();
        self.update_display();
    }

    /// Mouse dispatch through the active render shell
    /// (`ToolExecutionComponent extends Container`, tool-execution.ts:14:
    /// `Container.prototype.handleMouse` hit-tests the children — the
    /// spacer row first, then the shell whose rows start at 1; the image
    /// components define no handlers, so dispatch stops at the shell).
    /// Click-recorded expand toggles are applied right after the dispatch
    /// returns.
    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<TuiMouseHandlerResult> {
        let result = if !self.hide_component && event.y >= 1 {
            let child_event =
                event.with_local(event.x, event.y - 1, event.width, (event.height - 1).max(0));
            if self.has_renderer_definition() {
                match self.get_render_shell() {
                    RenderShell::Default => self.content_box.handle_mouse(&child_event),
                    RenderShell::Self_ => self.self_render_container.handle_mouse(&child_event),
                }
            } else {
                self.content_text_region
                    .as_mut()
                    .and_then(|region| region.handle_mouse(&child_event))
            }
        } else {
            None
        };
        self.drain_pending_expand_toggle();
        result
    }

    fn set_expanded(&mut self, expanded: bool) {
        // Route trait-object calls (the `set_tools_expanded` chat walk,
        // upstream `isExpandable` duck-typing) to the inherent method;
        // inherent methods win on concrete receivers, so this does not
        // recurse.
        self.set_expanded(expanded);
    }

    fn set_output_pad(&mut self, output_pad: usize) {
        // `"setOutputPad" in child` walk equivalent (#10557): route to the
        // inherent method without recursing.
        ToolExecutionComponent::set_output_pad(self, output_pad);
    }

    fn set_show_images(&mut self, show: bool) {
        // `child instanceof ToolExecutionComponent` walk equivalent (#59):
        // route to the inherent method without recursing.
        ToolExecutionComponent::set_show_images(self, show);
    }

    fn set_image_width_cells(&mut self, width: usize) {
        // `child instanceof ToolExecutionComponent` walk equivalent (#59).
        ToolExecutionComponent::set_image_width_cells(self, width);
    }
}

/// `imageSources` entry (tool-execution.ts:42 @ a276dabe5, #10292): the
/// inputs of an `Image` component, so an instance is only reused while its
/// source bytes, mime type, and width are unchanged.
#[derive(Debug, Clone, PartialEq)]
struct ImageSource {
    data: String,
    mime_type: String,
    width_cells: usize,
}

/// `getTextOutput` (core/tools/render-utils.ts:38-60). `pub(crate)` for the
/// built-in tool renderers (`tool_renderers/`, T17).
pub(crate) fn get_text_output(result: Option<&ToolResultState>, show_images: bool) -> String {
    let Some(result) = result else {
        return String::new();
    };

    let text_blocks: Vec<&str> = result
        .content
        .iter()
        .filter(|c| c.kind == "text")
        .map(|c| c.text.as_deref().unwrap_or(""))
        .collect();
    let image_blocks: Vec<&ToolResultContentLoose> = result
        .content
        .iter()
        .filter(|c| c.kind == "image")
        .collect();

    let mut output = text_blocks
        .iter()
        .map(|t| sanitize_binary_output(&strip_ansi(t)).replace('\r', ""))
        .collect::<Vec<_>>()
        .join("\n");

    let caps = get_capabilities();
    if !image_blocks.is_empty() && (caps.images.is_none() || !show_images) {
        let image_indicators = image_blocks
            .iter()
            .map(|img| {
                let mime_type = img
                    .mime_type
                    .clone()
                    .unwrap_or_else(|| "image/unknown".into());
                let dims = match (&img.data, &img.mime_type) {
                    (Some(data), Some(mime)) => get_image_dimensions(data, mime),
                    _ => None,
                };
                image_fallback(&mime_type, dims, None)
            })
            .collect::<Vec<_>>()
            .join("\n");
        output = if output.is_empty() {
            image_indicators
        } else {
            format!("{output}\n{image_indicators}")
        };
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::themes::load_theme;
    use rpi_tui::tui::RenderHandle;

    /// Serializes tests that mutate the process-global terminal capabilities.
    static CAPS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn kitty_caps() -> rpi_tui::terminal_image::TerminalCapabilities {
        rpi_tui::terminal_image::TerminalCapabilities {
            images: Some(rpi_tui::terminal_image::ImageProtocol::Kitty),
            true_color: true,
            hyperlinks: false,
        }
    }

    /// A minimal 1x1 24-bit BMP (blue/green/red byte order + pad) as base64
    /// — decodable by the `image` crate.
    fn minimal_bmp(blue: u8, green: u8, red: u8) -> String {
        use base64::Engine;
        let mut bytes: Vec<u8> = Vec::with_capacity(58);
        bytes.extend_from_slice(b"BM");
        bytes.extend_from_slice(&58u32.to_le_bytes()); // file size
        bytes.extend_from_slice(&0u32.to_le_bytes()); // reserved
        bytes.extend_from_slice(&54u32.to_le_bytes()); // pixel data offset
        bytes.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER
        bytes.extend_from_slice(&1i32.to_le_bytes()); // width
        bytes.extend_from_slice(&1i32.to_le_bytes()); // height
        bytes.extend_from_slice(&1u16.to_le_bytes()); // planes
        bytes.extend_from_slice(&24u16.to_le_bytes()); // bpp
        bytes.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
        bytes.extend_from_slice(&4u32.to_le_bytes()); // image size
        bytes.extend_from_slice(&2835u32.to_le_bytes()); // x ppm
        bytes.extend_from_slice(&2835u32.to_le_bytes()); // y ppm
        bytes.extend_from_slice(&0u32.to_le_bytes()); // colors used
        bytes.extend_from_slice(&0u32.to_le_bytes()); // colors important
        bytes.extend_from_slice(&[blue, green, red, 0]); // BGR + pad
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn theme() -> Arc<Theme> {
        Arc::new(load_theme("dark", None).expect("builtin dark theme"))
    }

    fn make_component() -> ToolExecutionComponent {
        // A tool with no built-in render definition (T17): these tests
        // exercise the generic `formatToolExecution` fallback path.
        ToolExecutionComponent::new(
            "custom-tool",
            "call_1",
            serde_json::json!({"path": "src/main.rs"}),
            ToolExecutionOptions::default(),
            None,
            theme(),
            RenderHandle::new(|| {}),
            "/cwd",
        )
    }

    fn strip_ansi(input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let mut chars = input.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\u{1b}' if chars.peek() == Some(&'[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if c == 'm' {
                            break;
                        }
                    }
                }
                // OSC 8 hyperlink (`ESC]8;;..ESC\`): strip it too, so exact
                // assertions are independent of the process-global terminal
                // capability cache that other tests mutate.
                '\u{1b}' if chars.peek() == Some(&']') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if c == '\u{1b}' {
                            chars.next(); // `\` of the `ESC\` terminator
                            break;
                        }
                    }
                }
                _ => out.push(c),
            }
        }
        out
    }

    #[test]
    fn fallback_renders_title_and_args() {
        let component = make_component();
        let lines = component.render(60);
        let stripped = strip_ansi(&lines.join("\n"));
        assert!(stripped.contains("custom-tool"));
        assert!(stripped.contains("src/main.rs"));
        // Pending bg while partial.
        assert!(lines.iter().any(|l| l.contains("\u{1b}[48;")));
    }

    /// V16-13 FR-F R1 (render-utils.ts:74-96 @ 5257d0d5f): a definition
    /// without a `renderCall` hook falls back to `formatToolCallWithArgs`
    /// — collapsed `key=value` on the title line, expanded per-line.
    #[test]
    fn call_fallback_shows_arguments_without_a_render_call_hook() {
        let component = ToolExecutionComponent::new(
            "extension-tool",
            "call_1",
            serde_json::json!({"path": "src/main.rs", "count": 2}),
            ToolExecutionOptions::default(),
            Some(Arc::new(ShellStub(None))),
            theme(),
            RenderHandle::new(|| {}),
            "/cwd",
        );
        let collapsed = strip_ansi(&component.render(80).join("\n"));
        assert!(
            collapsed.contains("extension-tool path=\"src/main.rs\" count=2"),
            "{collapsed:?}"
        );

        let mut expanded = ToolExecutionComponent::new(
            "extension-tool",
            "call_2",
            serde_json::json!({"path": "src/main.rs", "count": 2}),
            ToolExecutionOptions::default(),
            Some(Arc::new(ShellStub(None))),
            theme(),
            RenderHandle::new(|| {}),
            "/cwd",
        );
        expanded.set_expanded(true);
        let expanded = strip_ansi(&expanded.render(80).join("\n"));
        assert!(expanded.contains("extension-tool"), "{expanded:?}");
        assert!(expanded.contains("path: src/main.rs"), "{expanded:?}");
        assert!(expanded.contains("count: 2"), "{expanded:?}");
    }

    /// Regression: the built-in codemode extension registers no
    /// `renderCall`/`renderResult` hooks, so without the T17 `codemode`
    /// renderer the tool resolved no definition at all and the generic
    /// `formatToolExecution` fallback printed the entire script (pretty
    /// JSON) and output unfolded. With the built-in renderer registered
    /// the script folds to its preview and the result renders the nested
    /// call rows with the script header hidden.
    #[test]
    fn codemode_calls_fold_through_the_builtin_renderer() {
        let script: String = (1..=40)
            .map(|index| format!("const v{index} = {index};\n"))
            .collect();
        let mut component = ToolExecutionComponent::new(
            "codemode",
            "call_1",
            serde_json::json!({"code": script}),
            ToolExecutionOptions::default(),
            None,
            theme(),
            RenderHandle::new(|| {}),
            "/cwd",
        );
        component.update_result(
            ToolResultState {
                content: vec![
                    ToolResultContentLoose::text(
                        "Script completed\nWall time 0.1 seconds\nOutput:\n",
                    ),
                    ToolResultContentLoose::text("done"),
                ],
                is_error: false,
                details: Some(serde_json::json!({
                    "calls": [
                        {"id": "call_1/1", "name": "read", "args": "{\"path\":\"a\"}",
                         "status": "ok", "durationMs": 5}
                    ]
                })),
                duration_ms: None,
            },
            false,
        );
        let stripped = strip_ansi(&component.render(80).join("\n"));
        // The script is folded, not printed in full.
        assert!(!stripped.contains("const v40"), "{stripped}");
        assert!(stripped.contains("more lines,"), "{stripped}");
        // The nested call renders; the script header is dropped.
        assert!(
            stripped.contains("✓ read {\"path\":\"a\"} 5ms"),
            "{stripped}"
        );
        assert!(!stripped.contains("Wall time"), "{stripped}");
        assert!(stripped.contains("done"), "{stripped}");
    }

    #[test]
    fn result_updates_background_by_state() {
        let mut component = make_component();
        component.update_result(
            ToolResultState {
                content: vec![ToolResultContentLoose::text("file contents")],
                is_error: false,
                details: None,
                duration_ms: None,
            },
            false,
        );
        let lines = component.render(60);
        let stripped = strip_ansi(&lines.join("\n"));
        assert!(stripped.contains("file contents"));

        let mut error_component = make_component();
        error_component.update_result(
            ToolResultState {
                content: vec![ToolResultContentLoose::text("permission denied")],
                is_error: true,
                details: None,
                duration_ms: None,
            },
            false,
        );
        let lines = error_component.render(60);
        assert!(lines.iter().any(|l| l.contains("\u{1b}[48;")));
        // isError bg differs from the success bg bytes.
        let mut success = make_component();
        success.update_result(
            ToolResultState {
                content: vec![ToolResultContentLoose::text("ok")],
                is_error: false,
                details: None,
                duration_ms: None,
            },
            false,
        );
        assert_ne!(success.render(60), error_component.render(60));
    }

    #[test]
    fn set_expanded_and_images_widths() {
        let mut component = make_component();
        component.set_expanded(true);
        component.set_image_width_cells(30);
        assert_eq!(component.image_width_cells, 30);
        component.set_image_width_cells(0);
        assert_eq!(component.image_width_cells, 1);
        component.set_show_images(false);
        assert!(!component.show_images);
        // The trait walk (#59) routes to the inherent setters, so a mounted
        // component updates in place.
        Component::set_show_images(&mut component, true);
        assert!(component.show_images);
        Component::set_image_width_cells(&mut component, 42);
        assert_eq!(component.image_width_cells, 42);
    }

    // ------------------------------------------------------------------
    // V14-11 FR-G: generic fallback preview folding (e14afc648,
    // tool-execution.ts:40, 139-145)
    // ------------------------------------------------------------------

    /// A component WITH a renderer definition whose hooks return `None` —
    /// the path where `createResultFallback` applies (a definition exists
    /// but neither its result hook nor a built-in renders the result).
    fn make_fallback_component() -> ToolExecutionComponent {
        ToolExecutionComponent::new(
            "custom-tool",
            "call_1",
            serde_json::json!({"path": "src/main.rs"}),
            ToolExecutionOptions::default(),
            Some(Arc::new(ShellStub(Some(RenderShell::Default)))),
            theme(),
            RenderHandle::new(|| {}),
            "/cwd",
        )
    }

    fn result_with_lines(count: usize) -> ToolResultState {
        let body: Vec<String> = (1..=count).map(|index| format!("l{index:02}")).collect();
        ToolResultState {
            content: vec![ToolResultContentLoose::text(body.join("\n"))],
            is_error: false,
            details: None,
            duration_ms: None,
        }
    }

    #[test]
    fn fallback_collapsed_previews_ten_lines_with_expand_hint() {
        let mut component = make_fallback_component();
        component.update_result(result_with_lines(15), false);
        let stripped = strip_ansi(&component.render(60).join("\n"));
        // First 10 lines shown…
        for index in 1..=10 {
            assert!(stripped.contains(&format!("l{index:02}")), "line {index}");
        }
        assert!(stripped.contains("... (5 more lines,"));
        assert!(stripped.contains("to expand"));
        // The 11th-15th lines are cut.
        for index in 11..=15 {
            assert!(!stripped.contains(&format!("l{index:02}")), "line {index}");
        }
    }

    #[test]
    fn fallback_expanded_shows_all_lines_without_hint() {
        let mut component = make_fallback_component();
        component.update_result(result_with_lines(15), false);
        component.set_expanded(true);
        let stripped = strip_ansi(&component.render(60).join("\n"));
        for index in 1..=15 {
            assert!(stripped.contains(&format!("l{index:02}")), "line {index}");
        }
        assert!(!stripped.contains("more lines"));
    }

    #[test]
    fn fallback_short_output_never_folds() {
        let mut component = make_fallback_component();
        component.update_result(result_with_lines(5), false);
        let stripped = strip_ansi(&component.render(60).join("\n"));
        assert!(stripped.contains("l05"));
        assert!(!stripped.contains("more lines"));
        // Exactly at the limit: no folding (remaining == 0).
        let mut boundary = make_component();
        boundary.update_result(result_with_lines(10), false);
        let stripped = strip_ansi(&boundary.render(60).join("\n"));
        assert!(stripped.contains("l10"));
        assert!(!stripped.contains("more lines"));
    }

    #[test]
    fn text_output_falls_back_to_image_indicators() {
        // The terminal-image capability cache is a process global; serialize
        // with the kitty test.
        let _guard = CAPS_LOCK.lock().unwrap();
        // With no terminal image capabilities, image blocks render as
        // fallback text via getTextOutput.
        rpi_tui::terminal_image::set_capabilities(rpi_tui::terminal_image::TerminalCapabilities {
            images: None,
            true_color: true,
            hyperlinks: false,
        });
        let mut component = make_component();
        component.update_result(
            ToolResultState {
                content: vec![ToolResultContentLoose::image("aGVsbG8=", "image/png")],
                is_error: false,
                details: None,
                duration_ms: None,
            },
            false,
        );
        let stripped = strip_ansi(&component.render(60).join("\n"));
        // imageFallback brackets the mime type: "[Image: [image/png]]"
        // (terminal-image.ts:482-488).
        assert!(stripped.contains("[Image: [image/png]]"));
    }

    // tool-execution-component.test.ts: "renders a text fallback for
    // non-PNG images without a transcoder" (#10292, a276dabe5). The
    // component's `ensure_png_transcoder` registers the real transcoder on
    // Kitty terminals, so the component-level fallback here comes from that
    // transcoder rejecting undecodable data; the truly transcoder-less path
    // is covered by the rpi-tui test
    // `renders_a_text_fallback_until_a_working_transcoder_is_registered`.
    #[test]
    fn non_png_kitty_images_render_a_text_fallback_when_conversion_fails() {
        let _transcoder_guard = crate::utils::image_convert::TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        crate::utils::image_convert::reset_png_transcoder_state();
        let _guard = CAPS_LOCK.lock().unwrap();
        rpi_tui::terminal_image::set_capabilities(kitty_caps());
        let mut component = make_component();
        component.update_result(
            ToolResultState {
                content: vec![ToolResultContentLoose::image(
                    "bm90LWFuLWltYWdl",
                    "image/jpeg",
                )],
                is_error: false,
                details: None,
                duration_ms: None,
            },
            false,
        );
        let stripped = strip_ansi(&component.render(60).join("\n"));
        assert!(
            stripped.contains("[Image: [image/jpeg]"),
            "fallback without a transcoder: {stripped:?}"
        );
        rpi_tui::terminal_image::reset_capabilities_cache();
    }

    /// A real 1x1 PNG with the given pixel color, as base64.
    fn png_with_pixel(red: u8, green: u8, blue: u8) -> String {
        use base64::Engine;
        let mut png = Vec::new();
        let mut image = image::RgbaImage::new(1, 1);
        image.put_pixel(0, 0, image::Rgba([red, green, blue, 255]));
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("encode png");
        base64::engine::general_purpose::STANDARD.encode(png)
    }

    // tool-execution-component.test.ts: "converts non-PNG tool images once
    // the transcoder loads" (#10292): the Image component keeps converted
    // data per instance and the source-keyed shared cache, so repeated
    // updates never re-convert, while a replaced image gets a fresh
    // conversion instead of a stale one (#8743, `21b8cc1a4`).
    #[test]
    fn converts_non_png_tool_images_once_a_transcoder_is_registered() {
        let _transcoder_guard = crate::utils::image_convert::TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let _guard = CAPS_LOCK.lock().unwrap();
        rpi_tui::terminal_image::set_capabilities(kitty_caps());
        let first_png = png_with_pixel(1, 2, 3);
        let second_png = png_with_pixel(4, 5, 6);
        let conversions: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        crate::utils::image_convert::install_test_transcoder(Arc::new({
            let first_png = first_png.clone();
            let second_png = second_png.clone();
            let conversions = Arc::clone(&conversions);
            move |_data: &str, _mime_type: &str| {
                let mut count = conversions.lock().unwrap_or_else(|p| p.into_inner());
                *count += 1;
                Some(if *count == 1 {
                    first_png.clone()
                } else {
                    second_png.clone()
                })
            }
        }));

        let first_bmp = minimal_bmp(0xF0, 0x00, 0x00);
        let mut component = make_component();
        component.update_result(
            ToolResultState {
                content: vec![ToolResultContentLoose::image(&first_bmp, "image/bmp")],
                is_error: false,
                details: None,
                duration_ms: None,
            },
            true,
        );
        let rendered = component.render(120).join("\n");
        assert!(rendered.contains(&first_png), "converted PNG rendered");
        assert!(*conversions.lock().unwrap_or_else(|p| p.into_inner()) == 1);

        // Same source: the Image instance and the shared cache keep the
        // conversion (no second call).
        component.update_result(
            ToolResultState {
                content: vec![ToolResultContentLoose::image(&first_bmp, "image/bmp")],
                is_error: false,
                details: None,
                duration_ms: None,
            },
            true,
        );
        let rendered = component.render(120).join("\n");
        assert!(rendered.contains(&first_png));
        assert_eq!(*conversions.lock().unwrap_or_else(|p| p.into_inner()), 1);

        // Replaced image: fresh conversion; the old one must not surface.
        let second_bmp = minimal_bmp(0x00, 0xF0, 0x00);
        component.update_result(
            ToolResultState {
                content: vec![ToolResultContentLoose::image(&second_bmp, "image/bmp")],
                is_error: false,
                details: None,
                duration_ms: None,
            },
            true,
        );
        let rendered = component.render(120).join("\n");
        assert!(rendered.contains(&second_png), "re-converted: {rendered:?}");
        assert!(!rendered.contains(&first_png), "stale conversion dropped");
        assert_eq!(*conversions.lock().unwrap_or_else(|p| p.into_inner()), 2);

        // Final real PNG: no conversion, raw bytes rendered.
        let final_png = png_with_pixel(9, 8, 7);
        component.update_result(
            ToolResultState {
                content: vec![ToolResultContentLoose::image(&final_png, "image/png")],
                is_error: false,
                details: None,
                duration_ms: None,
            },
            false,
        );
        let rendered = component.render(120).join("\n");
        assert!(rendered.contains(&final_png), "final PNG rendered");
        assert!(!rendered.contains(&second_png), "final PNG replaces");
        assert_eq!(*conversions.lock().unwrap_or_else(|p| p.into_inner()), 2);
        crate::utils::image_convert::reset_png_transcoder_state();
        rpi_tui::terminal_image::reset_capabilities_cache();
    }

    /// A stub render definition with no hooks and a configurable shell.
    struct ShellStub(Option<RenderShell>);

    impl ToolDefinition for ShellStub {
        fn render_call(
            &self,
            _args: &Value,
            _theme: &Theme,
            _context: &ToolRenderContext,
        ) -> Option<StdBox<dyn Component>> {
            None
        }

        fn render_result(
            &self,
            _result: &ToolResultState,
            _options: ResultRenderOptions,
            _theme: &Theme,
            _context: &ToolRenderContext,
        ) -> Option<StdBox<dyn Component>> {
            None
        }

        fn render_shell(&self) -> Option<RenderShell> {
            self.0
        }
    }

    #[test]
    fn render_shell_merge_follows_upstream_nullish_chain() {
        // tool-execution.ts:110: `ext.renderShell ?? builtin.renderShell ??
        // "default"` — any explicit extension value wins, including
        // `"default"`; an absent one inherits the built-in shell (edit's is
        // `"self"`).
        let with_shell = |shell: Option<RenderShell>| {
            ToolExecutionComponent::new(
                "edit",
                "call_1",
                serde_json::json!({"path": "a.txt"}),
                ToolExecutionOptions::default(),
                Some(Arc::new(ShellStub(shell))),
                theme(),
                RenderHandle::new(|| {}),
                "/cwd",
            )
        };
        assert_eq!(
            with_shell(Some(RenderShell::Default)).get_render_shell(),
            RenderShell::Default,
            "explicit extension \"default\" beats the built-in \"self\""
        );
        assert_eq!(
            with_shell(Some(RenderShell::Self_)).get_render_shell(),
            RenderShell::Self_
        );
        assert_eq!(
            with_shell(None).get_render_shell(),
            RenderShell::Self_,
            "absent extension shell inherits the built-in one"
        );
        // No extension definition at all → the built-in shell; an unknown
        // tool with no definition at all → the default box.
        assert_eq!(with_shell(None).get_render_shell(), RenderShell::Self_);
        assert_eq!(make_component().get_render_shell(), RenderShell::Default);
    }

    /// Temp dir for the V13-11 edit-preview tests (the preview reads the
    /// file from disk, so each test needs its own state).
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "rpi-tool-exec-v13-11-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("system clock before epoch")
                    .as_nanos(),
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            TempDir(dir)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The header line ("edit <path>") with its background asserted — the
    /// edit TuiBox pads by one cell, so the header is not lines[0].
    fn assert_header_bg(lines: &[String], theme: &Theme, bg: &str) {
        let header = lines
            .iter()
            .find(|l| strip_ansi(l).contains("edit src.txt"))
            .unwrap_or_else(|| panic!("header line not found in {lines:?}"));
        assert!(
            header.starts_with(theme.get_bg_ansi(bg)),
            "expected {bg} header, got: {header:?}"
        );
    }

    fn make_edit_component(
        tmp: &TempDir,
        old_text: &str,
        new_text: &str,
    ) -> (ToolExecutionComponent, Arc<Theme>) {
        let theme = theme();
        let component = ToolExecutionComponent::new(
            "edit",
            "call_1",
            serde_json::json!({
                "path": "src.txt",
                "edits": [{"oldText": old_text, "newText": new_text}]
            }),
            ToolExecutionOptions::default(),
            None,
            Arc::clone(&theme),
            RenderHandle::new(|| {}),
            tmp.path().to_string_lossy().into_owned(),
        );
        (component, theme)
    }

    fn successful_edit_result() -> ToolResultState {
        ToolResultState {
            content: vec![],
            is_error: false,
            details: Some(serde_json::json!({
                "diff": "-1 Hello, world!\n+1 Hello, testing!",
                "firstChangedLine": 1,
            })),
            duration_ms: None,
        }
    }

    /// V13-11 (rpi#18), race reproduction: the file on disk is the POST-edit
    /// content — the agent thread wrote it before the UI drained `MessageEnd`,
    /// so `set_args_complete` computes the preview against the edited file
    /// and `oldText` no longer matches → not-found error preview. The
    /// successful result then arrives; ONE `update_result` pass must show the
    /// backfilled diff instead of the stale error (building the call
    /// component before the result side effect froze the error on screen
    /// forever — the port missed upstream's in-place rebuild, edit.ts:416-424).
    #[test]
    fn edit_result_backfill_corrects_racy_error_preview() {
        let tmp = TempDir::new("race");
        // Simulate the race: "world" was already replaced by "testing".
        std::fs::write(tmp.path().join("src.txt"), "Hello, testing!").unwrap();
        let (mut component, theme) = make_edit_component(&tmp, "world", "testing");

        // argsComplete computes the preview synchronously: "world" is
        // absent from the post-edit file → error preview (issue #18 shows
        // "Could not find edits[0] …").
        component.set_args_complete();
        let lines = component.render(80);
        let stripped = strip_ansi(&lines.join("\n"));
        assert!(
            stripped.contains("Could not find"),
            "race not reproduced — no error preview: {stripped}"
        );
        assert_header_bg(&lines, &theme, "toolErrorBg");

        // The edit did land on disk; the successful result carries the diff.
        component.update_result(successful_edit_result(), false);
        let lines = component.render(80);
        let stripped = strip_ansi(&lines.join("\n"));
        assert!(
            stripped.contains("-1 Hello, world!"),
            "backfilled diff missing: {stripped}"
        );
        assert!(
            stripped.contains("+1 Hello, testing!"),
            "backfilled diff missing: {stripped}"
        );
        assert!(
            !stripped.contains("Could not find"),
            "stale error preview survived the result (rpi#18): {stripped}"
        );
        assert_header_bg(&lines, &theme, "toolSuccessBg");
    }

    /// V13-11 (rpi#18), true not-found variant: the preview error is real
    /// (the oldText never existed), and a successful result with a diff
    /// still overwrites it in the same `update_display` pass — upstream's
    /// `setEditPreview` backfill is unconditional on success
    /// (edit.ts:400-411).
    #[test]
    fn edit_result_backfill_corrects_true_not_found_preview() {
        let tmp = TempDir::new("not-found");
        std::fs::write(tmp.path().join("src.txt"), "Hello, world!").unwrap();
        let (mut component, theme) = make_edit_component(&tmp, "missing", "testing");

        component.set_args_complete();
        let lines = component.render(80);
        let stripped = strip_ansi(&lines.join("\n"));
        assert!(
            stripped.contains("Could not find"),
            "expected a true not-found preview: {stripped}"
        );
        assert_header_bg(&lines, &theme, "toolErrorBg");

        component.update_result(successful_edit_result(), false);
        let lines = component.render(80);
        let stripped = strip_ansi(&lines.join("\n"));
        assert!(
            stripped.contains("-1 Hello, world!") && stripped.contains("+1 Hello, testing!"),
            "backfilled diff missing: {stripped}"
        );
        assert!(
            !stripped.contains("Could not find"),
            "stale error preview survived the result: {stripped}"
        );
        assert_header_bg(&lines, &theme, "toolSuccessBg");
    }

    // ------------------------------------------------------------------
    // V14-14: result-region click-to-expand (tool-execution.ts:172-178,
    // 315-353 @ 9841914, 71026970a — FR-A R9)
    // ------------------------------------------------------------------

    use rpi_tui::tui::{TuiMouseButton, TuiMouseEvent, TuiMouseEventType, TuiMouseHandlerResult};

    fn tool_click(y: isize) -> TuiMouseEvent {
        TuiMouseEvent {
            event_type: TuiMouseEventType::Click,
            button: TuiMouseButton::Left,
            x: 2,
            y,
            screen_x: 2,
            screen_y: y,
            width: 60,
            height: 20,
            shift: false,
            alt: false,
            ctrl: false,
            wheel_delta: None,
            click_count: Some(1),
        }
    }

    #[test]
    fn clicking_the_result_region_toggles_expanded() {
        let _caps = CAPS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut component = make_fallback_component();
        component.update_result(result_with_lines(30), false);
        let collapsed = strip_ansi(&component.render(60).join("\n"));
        assert!(
            collapsed.contains("more lines,"),
            "collapsed preview shows the expand hint"
        );

        // Row 0 is the leading spacer, row 1 the Box's top padding; the
        // clickable content starts at row 2.
        match component.handle_mouse(&tool_click(2)) {
            Some(TuiMouseHandlerResult::Event(event)) => assert!(event.handled),
            _ => panic!("click on the result region is handled"),
        }
        let expanded = strip_ansi(&component.render(60).join("\n"));
        assert!(
            !expanded.contains("more lines,"),
            "expanded output has no hint"
        );
        assert!(expanded.lines().count() > collapsed.lines().count());

        // Click again to collapse.
        component.handle_mouse(&tool_click(2));
        let re_collapsed = strip_ansi(&component.render(60).join("\n"));
        assert!(re_collapsed.contains("more lines,"));

        // Right-button clicks are ignored (the region declines and the
        // renderer's selection fallback runs instead).
        let mut right = tool_click(2);
        right.button = TuiMouseButton::Right;
        assert!(component.handle_mouse(&right).is_none());

        // Non-click types are ignored (press starts selection upstream).
        let mut press = tool_click(2);
        press.event_type = TuiMouseEventType::Press;
        assert!(component.handle_mouse(&press).is_none());
        assert!(strip_ansi(&component.render(60).join("\n")).contains("more lines,"));
    }

    #[test]
    fn clicks_before_any_result_are_declined() {
        // Upstream: `if (!this.result || ...) return undefined`
        // (tool-execution.ts:174) — a click on a pending tool falls through
        // to the selection path.
        let _caps = CAPS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut component = make_component();
        assert!(component.handle_mouse(&tool_click(1)).is_none());
    }
}
