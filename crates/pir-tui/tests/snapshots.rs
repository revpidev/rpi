//! Component render snapshot golden files (T11 gate requirement).
//!
//! Each case renders a base component at typical parameters through the
//! public `Component::render(width)` contract and compares the raw ANSI line
//! output (style sequences verbatim) against a golden file in
//! `tests/snapshots/<name>.snap`. The snapshots are observations of the
//! frozen upstream behavior — when an intentional upstream behavior change
//! alters the output, regenerate the goldens with:
//!
//! ```sh
//! PIR_UPDATE_SNAPSHOTS=1 cargo test -p pir-tui --test snapshots
//! ```
//!
//! and review the diff before committing.

use std::fs;
use std::path::PathBuf;

use std::sync::Arc;

use pir_tui::components::input::Input;
use pir_tui::components::loader::{Loader, LoaderIndicatorOptions};
use pir_tui::components::r#box::Box as TuiBox;
use pir_tui::components::select_list::{SelectItem, SelectList, SelectListTheme};
use pir_tui::components::spacer::Spacer;
use pir_tui::components::text::Text;
use pir_tui::components::truncated_text::TruncatedText;
use pir_tui::tui::{Component, Container, Focusable, RenderHandle};

fn snapshot_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots")
}

/// Compare `lines` against `tests/snapshots/<name>.snap`; rewrite the golden
/// when `PIR_UPDATE_SNAPSHOTS=1` is set.
fn assert_snapshot(name: &str, lines: &[String]) {
    let actual = format!("{}\n", lines.join("\n"));
    let path = snapshot_dir().join(format!("{name}.snap"));

    if std::env::var_os("PIR_UPDATE_SNAPSHOTS").is_some() {
        fs::create_dir_all(snapshot_dir())
            .unwrap_or_else(|err| panic!("create snapshot dir: {err}"));
        fs::write(&path, &actual).unwrap_or_else(|err| panic!("write {path:?}: {err}"));
        return;
    }

    let expected = fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "missing/unreadable snapshot {path:?}: {err}; regenerate with PIR_UPDATE_SNAPSHOTS=1"
        )
    });
    assert_eq!(
        actual, expected,
        "snapshot {name} drifted; if intentional, regenerate with PIR_UPDATE_SNAPSHOTS=1 \
         (escapes shown via debug formatting)\nactual:   {actual:?}\nexpected: {expected:?}"
    );
}

fn cyan(text: &str) -> String {
    format!("\x1b[36m{text}\x1b[0m")
}

fn blue_bg(text: &str) -> String {
    format!("\x1b[44m{text}\x1b[49m")
}

// --- Text (upstream components/text.ts) -------------------------------------

#[test]
fn text_plain() {
    let text = Text::new("The quick brown fox", 0, 0, None);
    assert_snapshot("text_plain", &text.render(80));
}

#[test]
fn text_padded() {
    let text = Text::new("padded line", 2, 1, None);
    assert_snapshot("text_padded", &text.render(40));
}

#[test]
fn text_ansi_wrap() {
    // ANSI-styled segment wrapped at a narrow width: style sequences must
    // survive wrapping intact.
    let text = Text::new(
        "\x1b[31mred\x1b[0m plain words that wrap around the narrow width",
        0,
        0,
        None,
    );
    assert_snapshot("text_ansi_wrap", &text.render(20));
}

#[test]
fn text_background() {
    let text = Text::new("on blue", 1, 0, Some(Box::new(blue_bg)));
    assert_snapshot("text_background", &text.render(30));
}

// --- Container (upstream Container in tui.ts) -------------------------------

#[test]
fn container_nested() {
    let mut container = Container::new();
    container.add_child(Box::new(Text::new("first", 0, 0, None)));
    container.add_child(Box::new(Spacer::new(1)));
    container.add_child(Box::new(Text::new("second", 1, 0, None)));
    assert_snapshot("container_nested", &container.render(40));
}

// --- Spacer (upstream components/spacer.ts) ---------------------------------

#[test]
fn spacer_lines() {
    assert_snapshot("spacer_lines", &Spacer::new(2).render(20));
}

// --- TruncatedText (upstream components/truncated-text.ts) ------------------

#[test]
fn truncated_text_overflow() {
    let text = TruncatedText::new("a very long single line that must be truncated", 1, 1);
    assert_snapshot("truncated_text_overflow", &text.render(20));
}

#[test]
fn truncated_text_ansi() {
    let text = TruncatedText::new("\x1b[32mgreen text that is far too long\x1b[0m", 0, 0);
    assert_snapshot("truncated_text_ansi", &text.render(15));
}

// --- Box (upstream components/box.ts) ---------------------------------------

#[test]
fn box_with_background() {
    let mut boxed = TuiBox::new(1, 1, Some(Box::new(blue_bg)));
    boxed.add_child(Box::new(Text::new("boxed", 0, 0, None)));
    assert_snapshot("box_with_background", &boxed.render(30));
}

// --- Loader (upstream components/loader.ts) ---------------------------------

/// Fixed frame 0 of the default spinner: built with a one-hour frame
/// interval, so no tick can land before `stop()` and the frame is pinned
/// deterministically at 0 even under CI load.
#[test]
fn loader_frame0() {
    let mut loader = Loader::new_with_interval(
        RenderHandle::new(|| {}),
        cyan,
        std::string::ToString::to_string,
        "Loading…",
        3_600_000, // 1h in ms: the first tick is a long way off
    );
    loader.stop();
    assert_snapshot("loader_frame0", &loader.render(40));
}

/// Single custom frame: no animation thread at all (frames.len() <= 1),
/// rendered verbatim (loader.ts `renderIndicatorVerbatim`).
#[test]
fn loader_custom_frame() {
    let loader = Loader::new(
        RenderHandle::new(|| {}),
        cyan,
        std::string::ToString::to_string,
        "static indicator",
        Some(LoaderIndicatorOptions {
            frames: Some(vec!["⏳".to_string()]),
            interval_ms: None,
        }),
    );
    assert_snapshot("loader_custom_frame", &loader.render(40));
}

// --- Input (upstream components/input.ts) ------------------------------------

#[test]
fn input_empty() {
    let mut input = Input::new();
    input.set_focused(true);
    assert_snapshot("input_empty", &input.render(20));
}

#[test]
fn input_with_text() {
    let mut input = Input::new();
    input.set_focused(true);
    input.set_value("hello world");
    input.handle_input("\x05"); // Ctrl+E - cursor to end
    assert_snapshot("input_with_text", &input.render(20));
}

#[test]
fn input_cursor_middle() {
    let mut input = Input::new();
    input.set_focused(true);
    input.set_value("hello world");
    input.handle_input("\x01"); // Ctrl+A
    for _ in 0..5 {
        input.handle_input("\x1b[C");
    }
    assert_snapshot("input_cursor_middle", &input.render(20));
}

#[test]
fn input_horizontal_scroll() {
    let mut input = Input::new();
    input.set_focused(true);
    input.set_value("a very long line that exceeds the input width and scrolls");
    input.handle_input("\x05"); // Ctrl+E - cursor at end
    assert_snapshot("input_horizontal_scroll", &input.render(20));
}

// --- SelectList (upstream components/select-list.ts) -------------------------

fn select_item(value: &str, description: Option<&str>) -> SelectItem {
    SelectItem {
        value: value.to_string(),
        label: value.to_string(),
        description: description.map(str::to_string),
    }
}

fn select_items() -> Vec<SelectItem> {
    vec![
        select_item("build", Some("Build the project")),
        select_item("run", Some("Run the application")),
        select_item("test", Some("Run tests")),
        select_item("deploy", Some("Deploy to production")),
        select_item("lint", Some("Run the linter")),
        select_item("clean", Some("Clean build artifacts")),
        select_item("format", Some("Format the codebase")),
        select_item("check", Some("Type-check the codebase")),
        select_item("publish", Some("Publish the package")),
        select_item("bump-version", Some("Bump the version number")),
        select_item("generate-docs", Some("Generate API documentation")),
        select_item(
            "very-long-command-name-that-needs-truncation",
            Some("Truncated primary column"),
        ),
    ]
}

#[test]
fn select_list_default() {
    let list = SelectList::new(
        select_items(),
        5,
        Arc::new(SelectListTheme::identity()),
        None,
    );
    assert_snapshot("select_list_default", &list.render(80));
}

#[test]
fn select_list_filtered() {
    let mut list = SelectList::new(
        select_items(),
        5,
        Arc::new(SelectListTheme::identity()),
        None,
    );
    list.set_filter("de");
    assert_snapshot("select_list_filtered", &list.render(80));
}

#[test]
fn select_list_scroll_window() {
    let mut list = SelectList::new(
        select_items(),
        5,
        Arc::new(SelectListTheme::identity()),
        None,
    );
    for _ in 0..8 {
        list.handle_input("\x1b[B"); // down x8
    }
    assert_snapshot("select_list_scroll_window", &list.render(80));
}

#[test]
fn select_list_no_match() {
    let mut list = SelectList::new(
        select_items(),
        5,
        Arc::new(SelectListTheme::identity()),
        None,
    );
    list.set_filter("zzz");
    assert_snapshot("select_list_no_match", &list.render(80));
}

// --- Editor (upstream components/editor.ts) --------------------------------

use std::future::Future;
use std::pin::Pin;

use pir_tui::autocomplete::{
    AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions, CompletionResult,
    GetSuggestionsOptions,
};
use pir_tui::components::editor::{Editor, EditorOptions, EditorTheme};
use pir_tui::terminal::Terminal;
use pir_tui::tui::Tui;

/// Fixed-size virtual terminal for editor snapshots (upstream
/// test/virtual-terminal.ts).
struct FixedTerminal {
    columns: u16,
    rows: u16,
}

impl FixedTerminal {
    fn new(columns: u16, rows: u16) -> Self {
        Self { columns, rows }
    }
}

impl Terminal for FixedTerminal {
    fn start(
        &mut self,
        _on_input: pir_tui::terminal::InputHandler,
        _on_resize: pir_tui::terminal::ResizeHandler,
    ) {
    }
    fn stop(&mut self) {}
    fn drain_input(
        &mut self,
        _max_ms: Option<u64>,
        _idle_ms: Option<u64>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
    fn write(&mut self, _data: &str) {}
    fn columns(&self) -> u16 {
        self.columns
    }
    fn rows(&self) -> u16 {
        self.rows
    }
    fn kitty_protocol_active(&self) -> bool {
        true
    }
    fn move_by(&mut self, _lines: i32) {}
    fn hide_cursor(&mut self) {}
    fn show_cursor(&mut self) {}
    fn clear_line(&mut self) {}
    fn clear_from_cursor(&mut self) {}
    fn clear_screen(&mut self) {}
    fn set_title(&mut self, _title: &str) {}
    fn set_progress(&mut self, _active: bool) {}
}

fn editor_at(columns: usize, rows: usize) -> Editor {
    let tui = Tui::new(Box::new(FixedTerminal::new(columns as u16, rows as u16)));
    Editor::new(
        tui,
        EditorTheme {
            border_color: Box::new(|text: &str| text.to_string()),
            select_list: Arc::new(SelectListTheme::identity()),
        },
        EditorOptions::default(),
    )
}

#[test]
fn editor_empty() {
    let mut editor = editor_at(80, 24);
    editor.set_focused(true);
    assert_snapshot("editor_empty", &editor.render(40));
}

#[test]
fn editor_multiline() {
    let mut editor = editor_at(80, 24);
    editor.set_focused(true);
    editor.set_text("line one\nline two\nline three");
    editor.handle_input("\x05"); // Ctrl+E - cursor to end
    assert_snapshot("editor_multiline", &editor.render(40));
}

#[test]
fn editor_wrap_layout() {
    let mut editor = editor_at(80, 24);
    editor.set_focused(true);
    editor.set_text("The quick brown fox jumps over the lazy dog while the sun sets slowly");
    editor.handle_input("\x01"); // Ctrl+A - cursor at start
    for _ in 0..12 {
        editor.handle_input("\x1b[C");
    }
    assert_snapshot("editor_wrap_layout", &editor.render(30));
}

#[test]
fn editor_scroll_indicators() {
    let mut editor = editor_at(80, 24);
    editor.set_focused(true);
    editor.set_text(
        &(0..20)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    editor.render(10);
    for _ in 0..12 {
        editor.handle_input("\x1b[A");
    }
    assert_snapshot("editor_scroll_indicators", &editor.render(10));
}

#[test]
fn editor_paste_marker() {
    let mut editor = editor_at(80, 24);
    editor.set_focused(true);
    editor.handle_input("A");
    let big_content = "line\n".repeat(20);
    let big_content = big_content.trim_end();
    editor.handle_input(&format!("\x1b[200~{big_content}\x1b[201~"));
    editor.handle_input("B");
    assert_snapshot("editor_paste_marker", &editor.render(40));
}

/// Provider returning a fixed suggestion list for a prefix (mock for the
/// autocomplete snapshot).
struct SnapshotProvider {
    items: Vec<AutocompleteItem>,
    prefix: &'static str,
}

impl AutocompleteProvider for SnapshotProvider {
    fn trigger_characters(&self) -> &[char] {
        &[]
    }

    fn get_suggestions(
        &self,
        _lines: &[String],
        _cursor_line: usize,
        _cursor_col: usize,
        _options: &GetSuggestionsOptions,
    ) -> Option<AutocompleteSuggestions> {
        Some(AutocompleteSuggestions {
            items: self.items.clone(),
            prefix: self.prefix.to_string(),
        })
    }

    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> CompletionResult {
        let line = lines.get(cursor_line).cloned().unwrap_or_default();
        let before = line[..line
            .char_indices()
            .nth(cursor_col.saturating_sub(prefix.chars().count()))
            .map(|(byte, _)| byte)
            .unwrap_or(line.len())]
            .to_string();
        let after = line[line
            .char_indices()
            .nth(cursor_col)
            .map(|(byte, _)| byte)
            .unwrap_or(line.len())..]
            .to_string();
        let mut new_lines = lines.to_vec();
        new_lines[cursor_line] = format!("{before}{}{after}", item.value);
        CompletionResult {
            lines: new_lines,
            cursor_line,
            cursor_col: cursor_col - prefix.chars().count() + item.value.chars().count(),
        }
    }
}

#[test]
fn editor_autocomplete_popup() {
    let mut editor = editor_at(80, 24);
    editor.set_focused(true);
    editor.set_autocomplete_provider(Arc::new(SnapshotProvider {
        items: vec![
            AutocompleteItem {
                value: "src/".to_string(),
                label: "src/".to_string(),
                description: Some("source directory".to_string()),
            },
            AutocompleteItem {
                value: "src.txt".to_string(),
                label: "src.txt".to_string(),
                description: Some("source listing".to_string()),
            },
            AutocompleteItem {
                value: "src/main.ts".to_string(),
                label: "main.ts".to_string(),
                description: Some("entry point".to_string()),
            },
        ],
        prefix: "src",
    }));

    // Type "src" then Tab: two+ suggestions open the picker.
    for ch in "src".chars() {
        editor.handle_input(&ch.to_string());
    }
    editor.handle_input("\t");
    editor.handle_input("\x1b[B"); // move selection down one
    assert_snapshot("editor_autocomplete_popup", &editor.render(60));
}

#[test]
fn editor_autocomplete_slash_menu() {
    let mut editor = editor_at(80, 24);
    editor.set_focused(true);
    editor.set_autocomplete_provider(Arc::new(SnapshotProvider {
        items: vec![
            AutocompleteItem {
                value: "/model".to_string(),
                label: "model".to_string(),
                description: Some("Switch model — Change the active model".to_string()),
            },
            AutocompleteItem {
                value: "/help".to_string(),
                label: "help".to_string(),
                description: Some("Show help".to_string()),
            },
            AutocompleteItem {
                value: "/load-skills".to_string(),
                label: "load-skills".to_string(),
                description: Some("Load skills — Load agent skills".to_string()),
            },
        ],
        prefix: "/",
    }));

    editor.handle_input("/");
    editor.handle_input("l");
    assert_snapshot("editor_autocomplete_slash_menu", &editor.render(60));
}

// --- Markdown (upstream components/markdown.ts) ------------------------------
//
// The markdown snapshots use the identity theme (no ANSI codes) so the
// goldens are plain text. Cases that depend on the terminal capability cache
// pin it with `set_capabilities` under a shared lock (cargo runs test
// binaries with parallel threads).

use pir_tui::components::image::{Image, ImageOptions, ImageTheme};
use pir_tui::components::markdown::{Markdown, MarkdownTheme};
use pir_tui::components::settings_list::{
    SettingItem, SettingsList, SettingsListOptions, SettingsListTheme, SubmenuDone,
};
use pir_tui::terminal_image::{
    reset_capabilities_cache, set_capabilities, ImageDimensions, ImageProtocol,
    TerminalCapabilities,
};

use std::sync::Mutex as StdMutex;

/// Serializes snapshot cases that mutate the global capabilities cache.
static SNAPSHOT_CAPS_LOCK: StdMutex<()> = StdMutex::new(());

fn identity_markdown(text: &str) -> Markdown {
    Markdown::new(text, 0, 0, Arc::new(MarkdownTheme::identity()), None, None)
}

#[test]
fn markdown_heading_list_task() {
    let markdown = identity_markdown(
        "# Title\n\n## Subtitle\n\n### Sub with `code`\n\n- Item 1\n  - Nested 1.1\n  - Nested 1.2\n- Item 2\n\n1. First\n2. Second\n\n- [ ] todo\n- [x] done",
    );
    assert_snapshot("markdown_heading_list_task", &markdown.render(60));
}

#[test]
fn markdown_table() {
    let markdown = identity_markdown(
        "| Command | Description | Example |\n| --- | --- | --- |\n| npm install | Install all dependencies | npm install |\n| npm run build | Build the project | npm run build |",
    );
    assert_snapshot("markdown_table", &markdown.render(50));
}

#[test]
fn markdown_code_block_and_streaming_fence() {
    let markdown = identity_markdown(
        "```ts\nconst hello = \"world\";\n```\n\n> quoted line\n\n---\n\ntext after",
    );
    assert_snapshot("markdown_code_block", &markdown.render(60));

    // Streamed partial closing fence: the final fence character has not
    // arrived yet, so the code block must not shrink or flicker.
    let streaming = identity_markdown("```ts\nconst x = 1;\n``");
    assert_snapshot("markdown_streaming_fence", &streaming.render(60));
}

#[test]
fn markdown_quote_link_strike() {
    let _guard = SNAPSHOT_CAPS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    set_capabilities(TerminalCapabilities {
        images: None,
        true_color: false,
        hyperlinks: false,
    });
    let markdown = identity_markdown(
        "> A quote with **bold**, `code` and ~~strike~~\n\n[link text](https://example.com) and ~~deleted~~ text\n\n> 1. nested ordered\n> - nested bullet",
    );
    let lines = markdown.render(60);
    reset_capabilities_cache();
    assert_snapshot("markdown_quote_link_strike", &lines);
}

// --- SettingsList (upstream components/settings-list.ts) --------------------

fn setting_item(id: &str, label: &str, current_value: &str) -> SettingItem {
    SettingItem {
        id: id.to_string(),
        label: label.to_string(),
        description: None,
        current_value: current_value.to_string(),
        values: None,
        submenu: None,
    }
}

#[test]
fn settings_list_main() {
    let mut settings = SettingsList::new(
        vec![
            SettingItem {
                id: "theme".into(),
                label: "Theme".into(),
                description: Some("Color theme of the interface".into()),
                current_value: "dark".into(),
                values: Some(vec!["dark".into(), "light".into()]),
                submenu: None,
            },
            SettingItem {
                id: "model".into(),
                label: "Model".into(),
                description: None,
                current_value: "claude-sonnet-4-5".into(),
                values: None,
                submenu: None,
            },
            SettingItem {
                id: "temperature".into(),
                label: "Temperature".into(),
                description: Some("Sampling temperature for completions".into()),
                current_value: "0.7".into(),
                values: None,
                submenu: None,
            },
            setting_item("verbose", "Verbose output", "off"),
            setting_item("wrap", "Wrap long lines", "on"),
            setting_item("spell", "Spell checking", "off"),
        ],
        3,
        Arc::new(SettingsListTheme::identity()),
        None,
    );
    // Move down so the scroll window shifts and the indicator shows.
    settings.handle_input("\x1b[B");
    settings.handle_input("\x1b[B");
    let lines = settings.render(50);
    assert_snapshot("settings_list_main", &lines);
}

#[test]
fn settings_list_search() {
    let mut settings = SettingsList::new(
        vec![
            setting_item("alpha", "Alpha model", "gpt-4o"),
            setting_item("beta", "Beta channel", "off"),
            setting_item("gamma", "Gamma rays", "3.0"),
        ],
        5,
        Arc::new(SettingsListTheme::identity()),
        Some(SettingsListOptions {
            enable_search: true,
        }),
    );
    settings.handle_input("a");
    settings.handle_input("l");
    let lines = settings.render(50);
    assert_snapshot("settings_list_search", &lines);
}

/// A stub submenu component that renders a fixed hint line; used to snapshot
/// the submenu state of a SettingsList.
struct SubmenuHintStub {
    lines: Vec<String>,
}

impl pir_tui::tui::Component for SubmenuHintStub {
    fn render(&self, _width: usize) -> Vec<String> {
        self.lines.clone()
    }
}

#[test]
fn settings_list_submenu_hint() {
    let mut settings = SettingsList::new(
        vec![
            SettingItem {
                id: "model".into(),
                label: "Model".into(),
                description: None,
                current_value: "claude".into(),
                values: None,
                submenu: Some(Box::new(|current_value: &str, _done: SubmenuDone| {
                    assert_eq!(current_value, "claude");
                    Box::new(SubmenuHintStub {
                        lines: vec![
                            "  Model".to_string(),
                            "  ─────".to_string(),
                            "  Esc to go back".to_string(),
                        ],
                    })
                })),
            },
            setting_item("theme", "Theme", "dark"),
        ],
        5,
        Arc::new(SettingsListTheme::identity()),
        None,
    );
    settings.handle_input("\r");
    let lines = settings.render(50);
    assert_snapshot("settings_list_submenu_hint", &lines);
}

// --- Image (upstream components/image.ts) -----------------------------------

const SNAPSHOT_IMAGE_DIMS: ImageDimensions = ImageDimensions {
    width_px: 800.0,
    height_px: 600.0,
};

#[test]
fn image_kitty_sequence() {
    let _guard = SNAPSHOT_CAPS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    set_capabilities(TerminalCapabilities {
        images: Some(ImageProtocol::Kitty),
        true_color: true,
        hyperlinks: true,
    });
    let image = Image::new(
        "iVBORw0KGgo=",
        "image/png",
        ImageTheme {
            fallback_color: Box::new(|text: &str| text.to_string()),
        },
        Some(ImageOptions {
            image_id: Some(42),
            ..Default::default()
        }),
        Some(SNAPSHOT_IMAGE_DIMS),
    );
    let lines = image.render(80);
    reset_capabilities_cache();
    assert_snapshot("image_kitty_sequence", &lines);
}

#[test]
fn image_fallback_text() {
    let _guard = SNAPSHOT_CAPS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    set_capabilities(TerminalCapabilities {
        images: None,
        true_color: false,
        hyperlinks: false,
    });
    let image = Image::new(
        "iVBORw0KGgo=",
        "image/png",
        ImageTheme {
            fallback_color: Box::new(|text: &str| format!("\x1b[33m{text}\x1b[0m")),
        },
        Some(ImageOptions {
            filename: Some("photo.png".to_string()),
            ..Default::default()
        }),
        Some(SNAPSHOT_IMAGE_DIMS),
    );
    let lines = image.render(80);
    reset_capabilities_cache();
    assert_snapshot("image_fallback_text", &lines);
}
