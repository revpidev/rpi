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
//! - `maybeConvertImagesForKitty` converts non-PNG images to PNG
//!   synchronously (upstream is async via photon/sharp,
//!   tool-execution.ts:178-199 + utils/image-convert.ts); the rendered
//!   output is identical, and the EXIF orientation pass of upstream
//!   `convertImageBytesToPng` is not applied (no EXIF metadata is present in
//!   typical tool-captured images).
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
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rpi_tui::components::image::{Image, ImageOptions, ImageTheme};
use rpi_tui::components::r#box::Box as TuiBox;
use rpi_tui::components::spacer::Spacer;
use rpi_tui::components::text::Text;
use rpi_tui::terminal_image::{
    get_capabilities, get_image_dimensions, image_fallback, ImageProtocol,
};
use rpi_tui::tui::{Component, Container, RenderHandle};
use serde_json::Value;

use crate::core::themes::Theme;
use crate::tools::sanitize::{sanitize_binary_output, strip_ansi};

/// `ToolExecutionOptions` (tool-execution.ts:8-11).
#[derive(Debug, Clone, Copy)]
pub struct ToolExecutionOptions {
    pub show_images: bool,
    pub image_width_cells: usize,
}

impl Default for ToolExecutionOptions {
    fn default() -> Self {
        Self {
            show_images: true,
            image_width_cells: 60,
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
    content_text: Text,
    self_render_container: Container,
    renderer_state: RendererStateSlot,
    image_components: Vec<Image>,
    image_spacers: Vec<Spacer>,
    tool_name: String,
    tool_call_id: String,
    args: Value,
    expanded: bool,
    show_images: bool,
    image_width_cells: usize,
    is_partial: bool,
    tool_definition: Option<Arc<dyn ToolDefinition>>,
    built_in_tool_definition: Option<Arc<dyn ToolDefinition>>,
    render_handle: RenderHandle,
    cwd: String,
    execution_started: bool,
    args_complete: bool,
    result: Option<ToolResultState>,
    converted_images: HashMap<usize, (String, String)>,
    hide_component: bool,
    theme: Arc<Theme>,
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
            content_text: Text::new("", 0, 0, None),
            self_render_container: Container::new(),
            renderer_state: RendererStateSlot::default(),
            image_components: Vec::new(),
            image_spacers: Vec::new(),
            tool_name,
            tool_call_id: tool_call_id.into(),
            args,
            expanded: false,
            show_images: options.show_images,
            image_width_cells: options.image_width_cells,
            is_partial: true,
            tool_definition,
            built_in_tool_definition,
            render_handle,
            cwd: cwd.into(),
            execution_started: false,
            args_complete: false,
            result: None,
            converted_images: HashMap::new(),
            hide_component: false,
            theme,
        };

        // Always create all shell variants (tool-execution.ts:65-76):
        // contentBox for default renderer-based composition, selfRenderContainer
        // for self-framing renderers, contentText for the generic fallback.
        let pending_bg = {
            let theme = Arc::clone(&component.theme);
            Box::new(move |t: &str| theme.bg("toolPendingBg", t))
        };
        component.content_box = TuiBox::new(1, 1, Some(pending_bg.clone()));
        component.content_text = Text::new("", 1, 1, Some(pending_bg));

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
        }
    }

    /// `createCallFallback` (tool-execution.ts:135-137).
    fn create_call_fallback(&self) -> Text {
        Text::new(
            self.theme.fg("toolTitle", &Theme::bold(&self.tool_name)),
            0,
            0,
            None,
        )
    }

    /// `createResultFallback` (tool-execution.ts:139-145).
    fn create_result_fallback(&self) -> Option<Text> {
        let output = get_text_output(self.result.as_ref(), self.show_images);
        if output.is_empty() {
            return None;
        }
        Some(Text::new(self.theme.fg("toolOutput", &output), 0, 0, None))
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
        self.is_partial = is_partial;
        self.update_display();
        self.maybe_convert_images_for_kitty();
    }

    /// `maybeConvertImagesForKitty` (tool-execution.ts:178-199), converted
    /// synchronously (see module doc).
    fn maybe_convert_images_for_kitty(&mut self) {
        let caps = get_capabilities();
        if caps.images != Some(ImageProtocol::Kitty) {
            return;
        }
        let Some(result) = &self.result else {
            return;
        };
        let image_blocks: Vec<(usize, &ToolResultContentLoose)> = result
            .content
            .iter()
            .enumerate()
            .filter(|(_, c)| c.kind == "image")
            .collect();
        for (index, img) in image_blocks {
            let (Some(data), Some(mime_type)) = (&img.data, &img.mime_type) else {
                continue;
            };
            if mime_type == "image/png" {
                continue;
            }
            if self.converted_images.contains_key(&index) {
                continue;
            }
            if let Some(converted) = convert_to_png(data, mime_type) {
                self.converted_images.insert(index, converted);
            }
        }
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
        if let Some(def) = &self.tool_definition {
            if let Some(component) = def.render_call(&self.args, &self.theme, &context) {
                return component;
            }
        }
        if let Some(def) = &self.built_in_tool_definition {
            if let Some(component) = def.render_call(&self.args, &self.theme, &context) {
                return component;
            }
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
        if let Some(def) = &self.tool_definition {
            if let Some(component) = def.render_result(&result, options, &self.theme, &context) {
                return Some(component);
            }
        }
        if let Some(def) = &self.built_in_tool_definition {
            if let Some(component) = def.render_result(&result, options, &self.theme, &context) {
                return Some(component);
            }
        }
        self.create_result_fallback()
            .map(|text| StdBox::new(text) as StdBox<dyn Component>)
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
                    // composition (tool-execution.ts:66-68).
                    self.content_box.set_bg_fn(Some(bg_fn));
                    self.content_box.clear();
                    self.content_box.add_child(self.render_call_component());
                    if let Some(component) = self.render_result_component() {
                        self.content_box.add_child(component);
                    }
                }
                RenderShell::Self_ => {
                    // selfRenderContainer is used when the tool renders its
                    // own framing (tool-execution.ts:66-68).
                    self.self_render_container.clear();
                    self.self_render_container
                        .add_child(self.render_call_component());
                    if let Some(component) = self.render_result_component() {
                        self.self_render_container.add_child(component);
                    }
                }
            }
            // Upstream's `hasContent` is always true on the renderer path
            // (the call renderer always adds a component,
            // tool-execution.ts:270-284), so the `hideComponent` check
            // (tool-execution.ts:356-358) is dead code there; the port omits
            // the unobservable variable.
        } else {
            self.content_text.set_custom_bg_fn(Some(bg_fn));
            self.content_text.set_text(self.format_tool_execution());
        }

        // Images are appended after the shell (tool-execution.ts:321-354).
        self.image_components.clear();
        self.image_spacers.clear();
        if let Some(result) = &self.result {
            let caps = get_capabilities();
            let image_blocks: Vec<(usize, &ToolResultContentLoose)> = result
                .content
                .iter()
                .enumerate()
                .filter(|(_, c)| c.kind == "image")
                .collect();
            for (i, img) in image_blocks {
                if caps.images.is_none() || !self.show_images {
                    continue;
                }
                let (Some(data), Some(mime_type)) = (&img.data, &img.mime_type) else {
                    continue;
                };
                let converted = self.converted_images.get(&i);
                let image_data = converted.map(|c| c.0.as_str()).unwrap_or(data.as_str());
                let image_mime_type = converted
                    .map(|c| c.1.as_str())
                    .unwrap_or(mime_type.as_str());
                if caps.images == Some(ImageProtocol::Kitty) && image_mime_type != "image/png" {
                    continue;
                }
                self.image_spacers.push(Spacer::new(1));
                let fallback_color = {
                    let theme = Arc::clone(&self.theme);
                    Box::new(move |s: &str| theme.fg("toolOutput", s))
                };
                let image_component = Image::new(
                    image_data.to_string(),
                    image_mime_type.to_string(),
                    ImageTheme { fallback_color },
                    Some(ImageOptions {
                        max_width_cells: Some(self.image_width_cells),
                        ..Default::default()
                    }),
                    None,
                );
                self.image_components.push(image_component);
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
            lines.extend(self.content_text.render(width));
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
        self.content_text.invalidate();
        self.self_render_container.invalidate();
        self.update_display();
    }

    fn set_expanded(&mut self, expanded: bool) {
        // Route trait-object calls (the `set_tools_expanded` chat walk,
        // upstream `isExpandable` duck-typing) to the inherent method;
        // inherent methods win on concrete receivers, so this does not
        // recurse.
        self.set_expanded(expanded);
    }
}

/// `convertToPng` (utils/image-convert.ts:31-48): decode any image and
/// re-encode as PNG for the Kitty protocol. Synchronous in the port (see the
/// module doc); returns `None` on decode/encode failure (upstream returns
/// `null` when photon is unavailable).
fn convert_to_png(base64_data: &str, mime_type: &str) -> Option<(String, String)> {
    if mime_type == "image/png" {
        return Some((base64_data.to_string(), "image/png".to_string()));
    }
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(base64_data)
        .ok()?;
    let image = image::load_from_memory(&bytes).ok()?;
    let mut png_bytes = Vec::new();
    image
        .write_to(
            &mut std::io::Cursor::new(&mut png_bytes),
            image::ImageFormat::Png,
        )
        .ok()?;
    Some((
        base64::engine::general_purpose::STANDARD.encode(png_bytes),
        "image/png".to_string(),
    ))
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

    #[test]
    fn result_updates_background_by_state() {
        let mut component = make_component();
        component.update_result(
            ToolResultState {
                content: vec![ToolResultContentLoose::text("file contents")],
                is_error: false,
                details: None,
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
            },
            false,
        );
        let stripped = strip_ansi(&component.render(60).join("\n"));
        // imageFallback brackets the mime type: "[Image: [image/png]]"
        // (terminal-image.ts:482-488).
        assert!(stripped.contains("[Image: [image/png]]"));
    }

    #[test]
    fn kitty_images_skip_non_png_without_conversion() {
        // The terminal-image capability cache is a process global; serialize
        // with the fallback test.
        let _guard = CAPS_LOCK.lock().unwrap();
        rpi_tui::terminal_image::set_capabilities(rpi_tui::terminal_image::TerminalCapabilities {
            images: Some(ImageProtocol::Kitty),
            true_color: true,
            hyperlinks: false,
        });
        // A bogus base64 JPEG cannot be converted; the image must be skipped
        // (tool-execution.ts:339), not crash.
        let mut component = make_component();
        component.update_result(
            ToolResultState {
                content: vec![ToolResultContentLoose::image(
                    "bm90LWFuLWltYWdl",
                    "image/jpeg",
                )],
                is_error: false,
                details: None,
            },
            false,
        );
        let _ = component.render(60);
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
}
