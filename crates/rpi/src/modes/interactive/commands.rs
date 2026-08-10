//! T12-S5b slash command handlers (data/display class) — ports of the
//! `handle*Command` methods of
//! `packages/coding-agent/src/modes/interactive/interactive-mode.ts` @ pi
//! 0.82.1 (2efa728).
//!
//! The data/display command family runs on the run loop / drain thread and
//! renders into the chat container (lock contract: command handlers may lock
//! components freely — unlike component callbacks). The handlers that need
//! the runtime (import / session replacement) live on [`InteractiveMode`];
//! the rest extend [`InteractiveUi`].
//!
//! Intentional differences (vs upstream):
//! - Clipboard (`handle_copy_command`): upstream prefers native clipboard
//!   tools (clipboard-rs addon, pbcopy/clip, termux-clipboard-set, wl-copy,
//!   xclip/xsel) and falls back to OSC 52 (utils/clipboard.ts:44-152); rpi
//!   has no clipboard library, so [`InteractiveUi::copy_to_clipboard`] writes
//!   the OSC 52 escape directly (`emitOsc52`, utils/clipboard.ts:26-33) with
//!   the same 100 KB encoded-length cap. Native-tool integration is a TODO
//!   hook.
//! - Export (`handle_export_command`): the JSONL branch uses the local
//!   `AgentSession::export_to_jsonl`; the HTML branch uses
//!   `AgentSession::export_to_html` (T14 W5; no `renderedTools`
//!   pre-rendering — export_html.rs header).
//! - Share (`handle_share_command`): gh calls go through the injectable
//!   `ShareRunner` (core/share.rs); the cancellable loader's abort and the
//!   gist worker's completion settle in the drain (`UiCommand::ShareAbort` /
//!   `ShareCompleted`) instead of upstream's inline promise continuation
//!   (component lock contract).
//! - Import (`handle_import_command`): the extension confirmation prompt
//!   (`showExtensionConfirm`, interactive-mode.ts:5474-5478) and the
//!   missing-session-cwd prompt (interactive-mode.ts:5489-5501) are T15
//!   extension hooks; local import proceeds directly and reports
//!   failures uniformly.
//! - Changelog (`handle_changelog_command`): no local changelog asset (T15
//!   hook), so the placeholder branch of the upstream ternary renders; the
//!   border + title frame is kept.
//! - Hotkeys (`handle_hotkeys_command`): the extension-registered shortcuts
//!   section (interactive-mode.ts:5835-5848) is a T15 hook; the built-in
//!   sections are identical to upstream.
//! - Debug (`handle_debug_command`): `Tui` has no public full-render API,
//!   so the "all rendered lines" section (interactive-mode.ts:5877-5890) is
//!   a T14 hook; the log carries the header + agent-message JSONL sections.
//! - Session (`handle_session_command`): the cache-waste section
//!   (interactive-mode.ts:5648, 5692-5699) is a T14 hook — no local
//!   `computeCacheWaste` (cache-stats port).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine;
use rpi_agent::session::SessionEntry;
use rpi_tui::components::markdown::Markdown;
use rpi_tui::components::spacer::Spacer;
use rpi_tui::components::text::Text;

use crate::core::session_manager::now_iso8601;
use crate::core::themes::Theme;
use crate::core::usage_totals::get_usage_cost_breakdown;
use crate::modes::interactive::components::bordered_loader::BorderedLoaderComponent;
use crate::modes::interactive::components::keybinding_hints::key_display_text;
use crate::modes::interactive::components::util::to_locale_string;
use crate::modes::interactive::components::DynamicBorder;
use crate::modes::interactive::footer::format_tokens;
use crate::modes::interactive::interactive_mode::{
    InteractiveMode, InteractiveUi, ShareState, UiCommand,
};

/// `MAX_OSC52_ENCODED_LENGTH` (utils/clipboard.ts:20).
const MAX_OSC52_ENCODED_LENGTH: usize = 100_000;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `getPathCommandArgument` (interactive-mode.ts:5438-5465): extract the
/// path argument of `/export` / `/import` — the quoted string when the first
/// char is a quote, otherwise the first whitespace-delimited token; `None`
/// when the text is the bare command or has no argument.
fn get_path_command_argument(text: &str, command: &str) -> Option<String> {
    let args_string = text.strip_prefix(&format!("{command} "))?.trim_start();
    if args_string.is_empty() {
        return None;
    }
    let first_char = args_string.chars().next()?;
    if first_char == '"' || first_char == '\'' {
        let closing_quote_index = args_string[1..].find(first_char)? + 1;
        return Some(args_string[1..closing_quote_index].to_string());
    }
    Some(
        args_string
            .split_whitespace()
            .next()
            .unwrap_or(args_string)
            .to_string(),
    )
}

impl InteractiveUi {
    /// `handleSessionCommand` (interactive-mode.ts:5644-5705): render the
    /// session stats + usage breakdown into the chat.
    pub(crate) fn handle_session_command(&self) {
        let stats = self.session().get_session_stats();
        let session_name = self.session().session_name();
        let entries = lock(&self.session().session_manager()).get_entries();
        let known_entries: Vec<SessionEntry> = entries
            .iter()
            .filter_map(|entry| entry.known())
            .cloned()
            .collect();
        let usage_breakdown = get_usage_cost_breakdown(&known_entries);
        // TODO(T14): cache-waste section (computeCacheWaste,
        // interactive-mode.ts:5648, 5692-5699) — needs the cache-stats port.

        let mut info = format!("{}\n\n", Theme::bold("Session Info"));
        if let Some(session_name) = &session_name {
            info.push_str(&format!(
                "{} {session_name}\n",
                lock(&self.theme).fg("dim", "Name:")
            ));
        }
        info.push_str(&format!(
            "{} {}\n",
            lock(&self.theme).fg("dim", "File:"),
            stats.session_file.as_deref().unwrap_or("In-memory")
        ));
        info.push_str(&format!(
            "{} {}\n\n",
            lock(&self.theme).fg("dim", "ID:"),
            stats.session_id
        ));
        info.push_str(&format!("{}\n", Theme::bold("Messages")));
        info.push_str(&format!(
            "{} {}\n",
            lock(&self.theme).fg("dim", "Total:"),
            to_locale_string(stats.total_messages)
        ));
        info.push_str(&format!(
            "{} {}\n",
            lock(&self.theme).fg("dim", "User:"),
            to_locale_string(stats.user_messages)
        ));
        info.push_str(&format!(
            "{} {}\n",
            lock(&self.theme).fg("dim", "Assistant:"),
            to_locale_string(stats.assistant_messages)
        ));
        info.push_str(&format!(
            "{} {} calls, {} results\n\n",
            lock(&self.theme).fg("dim", "Tools:"),
            to_locale_string(stats.tool_calls),
            to_locale_string(stats.tool_results)
        ));
        info.push_str(&format!("{}\n", Theme::bold("Tokens")));
        // "Input" is the full prompt volume; with cache activity split it
        // into cached (served from cache) vs uncached — the only
        // provider-independent split (interactive-mode.ts:5667-5672).
        let input = stats.tokens.input;
        let cache_read = stats.tokens.cache_read;
        let cache_write = stats.tokens.cache_write;
        let prompt_tokens = input + cache_read + cache_write;
        info.push_str(&format!(
            "{} {}\n",
            lock(&self.theme).fg("dim", "Input:"),
            to_locale_string(prompt_tokens)
        ));
        if prompt_tokens > 0 && (cache_read > 0 || cache_write > 0) {
            let hit_rate = format!(
                "({:.1}%)",
                (cache_read as f64 / prompt_tokens as f64) * 100.0
            );
            let cached_label = lock(&self.theme).fg("dim", "Cached:");
            let hit_rate_label = lock(&self.theme).fg("dim", &hit_rate);
            info.push_str(&format!(
                "  {} {} {}\n",
                cached_label,
                to_locale_string(cache_read),
                hit_rate_label
            ));
            let written = if cache_write > 0 {
                format!(
                    " {}",
                    lock(&self.theme).fg(
                        "dim",
                        &format!("({} written to cache)", to_locale_string(cache_write))
                    )
                )
            } else {
                String::new()
            };
            info.push_str(&format!(
                "  {} {}{written}\n",
                lock(&self.theme).fg("dim", "Uncached:"),
                to_locale_string(input + cache_write)
            ));
        }
        info.push_str(&format!(
            "{} {}\n",
            lock(&self.theme).fg("dim", "Output:"),
            to_locale_string(stats.tokens.output)
        ));
        info.push_str(&format!(
            "{} {}\n",
            lock(&self.theme).fg("dim", "Total:"),
            to_locale_string(stats.tokens.total)
        ));

        if stats.cost > 0.0 {
            info.push_str(&format!("\n{}\n", Theme::bold("Cost")));
            info.push_str(&format!(
                "{} ${:.3}",
                lock(&self.theme).fg("dim", "Total:"),
                stats.cost
            ));
            if usage_breakdown.len() > 1 {
                for entry in &usage_breakdown {
                    let key_label = lock(&self.theme).fg("dim", &format!("{}:", entry.key));
                    let tokens_label = lock(&self.theme)
                        .fg("dim", &format!("({} tokens)", format_tokens(entry.tokens)));
                    info.push_str(&format!(
                        "\n  {} ${:.3} {}",
                        key_label, entry.cost, tokens_label
                    ));
                }
            }
        }

        let mut chat = lock(&self.chat_container);
        chat.children.push(Box::new(Spacer::new(1)));
        chat.children.push(Box::new(Text::new(info, 1, 0, None)));
        self.render_handle.request_render();
    }

    /// `handleNameCommand` (interactive-mode.ts:5620-5642): set the session
    /// name, or show the current name / usage warning when the argument is
    /// empty.
    pub(crate) fn handle_name_command(&self, text: &str) {
        // `text.replace(/^\/name\s*/, "").trim()` (interactive-mode.ts:5621).
        let name = text
            .trim_start()
            .strip_prefix("/name")
            .map(|rest| rest.trim_start().trim())
            .unwrap_or("");
        if name.is_empty() {
            if let Some(current_name) = self.session().session_name() {
                let mut chat = lock(&self.chat_container);
                chat.children.push(Box::new(Spacer::new(1)));
                chat.children.push(Box::new(Text::new(
                    lock(&self.theme).fg("dim", &format!("Session name: {current_name}")),
                    1,
                    0,
                    None,
                )));
            } else {
                self.show_warning("Usage: /name <name>");
            }
            self.render_handle.request_render();
            return;
        }

        self.session().set_session_name(name);
        let session_name = self.session().session_name();
        if session_name.as_deref() != Some(name) {
            self.show_warning(&format!(
                "Session name was normalized from {} to {}",
                serde_json::to_string(name).unwrap_or_else(|_| format!("\"{name}\"")),
                serde_json::to_string(&session_name).unwrap_or_else(|_| "null".to_string())
            ));
        }
        let mut chat = lock(&self.chat_container);
        chat.children.push(Box::new(Spacer::new(1)));
        chat.children.push(Box::new(Text::new(
            lock(&self.theme).fg(
                "dim",
                &format!(
                    "Session name set: {}",
                    session_name.unwrap_or_else(|| name.to_string())
                ),
            ),
            1,
            0,
            None,
        )));
        self.render_handle.request_render();
    }

    /// `handleCopyCommand` (interactive-mode.ts:5605-5618): copy the last
    /// assistant message through the local clipboard hook.
    pub(crate) fn handle_copy_command(&self) {
        let Some(text) = self.session().get_last_assistant_text() else {
            self.show_error("No agent messages to copy yet.");
            return;
        };
        match self.copy_to_clipboard(&text) {
            Ok(()) => self.show_status("Copied last agent message to clipboard"),
            Err(error) => self.show_error(&error),
        }
    }

    /// Clipboard write hook. Upstream `copyToClipboard` (utils/clipboard.ts:
    /// 44-152) tries native clipboard tools (clipboard-rs addon, pbcopy /
    /// clip / termux-clipboard-set / wl-copy / xclip / xsel) before falling
    /// back to OSC 52; rpi has no clipboard library, so this port writes the
    /// OSC 52 escape directly (`emitOsc52`, utils/clipboard.ts:26-33) with
    /// the same encoded-length cap. TODO: native tool integration.
    fn copy_to_clipboard(&self, text: &str) -> Result<(), String> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(text);
        if encoded.len() > MAX_OSC52_ENCODED_LENGTH {
            return Err("Text too large to copy via OSC 52 (100KB limit)".to_string());
        }
        let osc52 = format!("\x1b]52;c;{encoded}\x07");
        self.ui.with_terminal(|terminal| terminal.write(&osc52));
        Ok(())
    }

    /// `handleExportCommand` (interactive-mode.ts:5422-5436): `.jsonl`
    /// suffix → JSONL branch export, anything else (or no path) → HTML.
    pub(crate) fn handle_export_command(&self, args: &str) {
        let output_path = get_path_command_argument(args, "/export");
        if let Some(jsonl_path) = output_path
            .as_deref()
            .filter(|path| path.ends_with(".jsonl"))
        {
            match self.session().export_to_jsonl(Some(jsonl_path)) {
                Ok(file_path) => {
                    self.show_status(&format!("Session exported to: {}", file_path.display()))
                }
                Err(error) => self.show_error(&format!(
                    "Failed to export session: {}",
                    error.raw_message()
                )),
            }
            return;
        }
        match self.session().export_to_html(output_path.as_deref()) {
            Ok(file_path) => self.show_status(&format!("Session exported to: {file_path}")),
            Err(error) => self.show_error(&format!(
                "Failed to export session: {}",
                error.raw_message()
            )),
        }
    }

    /// `handleShareCommand` (interactive-mode.ts:5511-5603): gh auth check →
    /// export a temp HTML → cancellable loader while a worker thread runs
    /// `gh gist create --public=false` → gist ID → viewer URL. Completion
    /// and abort arrive as `UiCommand::ShareCompleted` / `ShareAbort` and
    /// settle in the drain (component lock contract).
    pub(crate) fn handle_share_command(self: &Arc<Self>) {
        use crate::core::share::GhAuthStatus;

        let runner = lock(&self.share_runner).clone();
        match runner.auth_status() {
            GhAuthStatus::NotInstalled => {
                self.show_error(
                    "GitHub CLI (gh) is not installed. Install it from https://cli.github.com/",
                );
                return;
            }
            GhAuthStatus::NotLoggedIn => {
                self.show_error("GitHub CLI is not logged in. Run 'gh auth login' first.");
                return;
            }
            GhAuthStatus::Ok => {}
        }

        // `path.join(os.tmpdir(), "session.html")` (interactive-mode.ts:5526).
        // Divergence (D-045 补记): the file lives in a per-invocation unique
        // subdirectory so two concurrent rpi instances cannot overwrite each
        // other's export (which would publish the wrong session). The
        // basename stays `session.html` — gh uses it as the gist file name.
        let share_dir = std::env::temp_dir().join(format!(
            "rpi-share-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let tmp_file = share_dir.join("session.html");
        if let Err(error) = std::fs::create_dir_all(&share_dir) {
            self.show_error(&format!("Failed to export session: {error}"));
            return;
        }
        if let Err(error) = self
            .session()
            .export_to_html(Some(&tmp_file.to_string_lossy()))
        {
            // T14 review: clean up the (now empty) unique temp directory on
            // the failure path so failed attempts do not leak `rpi-share-*`
            // directories in /tmp.
            crate::core::share::cleanup_share_tmp_file(&tmp_file);
            self.show_error(&format!(
                "Failed to export session: {}",
                error.raw_message()
            ));
            return;
        }
        // The exported HTML may hold private conversation content; keep it
        // readable by this user only (T14 review).
        crate::core::share::restrict_share_tmp_file_permissions(&tmp_file);

        // Cancellable loader replacing the editor
        // (interactive-mode.ts:5529-5538).
        let cancelled = Arc::new(AtomicBool::new(false));
        *lock(&self.share_state) = Some(ShareState {
            cancelled: Arc::clone(&cancelled),
            tmp_file: tmp_file.clone(),
        });
        let mut loader = BorderedLoaderComponent::new(
            self.render_handle.clone(),
            Arc::clone(&lock(&self.theme)),
            "Creating gist...",
            None,
        );
        {
            // `loader.onAbort` (interactive-mode.ts:5549-5554) — component
            // callback, so it only flags + queues; the drain settles.
            let abort_flag = Arc::clone(&cancelled);
            let ui = self.upgrade_self();
            loader.on_abort = Some(Box::new(move || {
                abort_flag.store(true, Ordering::Relaxed);
                if let Some(ui) = &ui {
                    ui.push(UiCommand::ShareAbort);
                    ui.render_handle.request_render();
                }
            }));
        }
        self.show_selector(rpi_tui::tui::shared_component(loader));

        // `spawn("gh", ["gist", "create", "--public=false", tmpFile])`
        // (interactive-mode.ts:5563) on a worker thread — the run loop must
        // keep pumping while gh waits on the network.
        let worker_ui = Arc::clone(self);
        std::thread::spawn(move || {
            let outcome = runner.gist_create(&tmp_file, cancelled);
            worker_ui.push(UiCommand::ShareCompleted(outcome));
            worker_ui.render_handle.request_render();
        });
    }

    /// `handleChangelogCommand` (interactive-mode.ts:5707-5726). No local
    /// changelog asset (T15 hook), so the placeholder branch of the upstream
    /// ternary renders; the border + title frame is kept for parity.
    pub(crate) fn handle_changelog_command(&self) {
        let changelog_markdown = "No changelog entries found.";
        let border_theme = Arc::clone(&lock(&self.theme));
        let border = Box::new(move |text: &str| border_theme.fg("border", text));

        let mut chat = lock(&self.chat_container);
        chat.children.push(Box::new(Spacer::new(1)));
        chat.children
            .push(Box::new(DynamicBorder::new(border.clone())));
        chat.children.push(Box::new(Text::new(
            Theme::bold(&lock(&self.theme).fg("accent", "What's New")),
            1,
            0,
            None,
        )));
        chat.children.push(Box::new(Spacer::new(1)));
        chat.children.push(Box::new(Markdown::new(
            changelog_markdown,
            1,
            1,
            Arc::clone(&lock(&self.markdown_theme)),
            None,
            None,
        )));
        chat.children.push(Box::new(DynamicBorder::new(border)));
        self.render_handle.request_render();
    }

    /// `handleHotkeysCommand` (interactive-mode.ts:5742-5857): render the
    /// built-in keybinding tables as markdown. The extension-registered
    /// shortcuts section (interactive-mode.ts:5835-5848) is a T15 hook.
    pub(crate) fn handle_hotkeys_command(&self) {
        // Navigation keybindings.
        let cursor_up = key_display_text("tui.editor.cursorUp");
        let cursor_down = key_display_text("tui.editor.cursorDown");
        let cursor_left = key_display_text("tui.editor.cursorLeft");
        let cursor_right = key_display_text("tui.editor.cursorRight");
        let cursor_word_left = key_display_text("tui.editor.cursorWordLeft");
        let cursor_word_right = key_display_text("tui.editor.cursorWordRight");
        let cursor_line_start = key_display_text("tui.editor.cursorLineStart");
        let cursor_line_end = key_display_text("tui.editor.cursorLineEnd");
        let jump_forward = key_display_text("tui.editor.jumpForward");
        let jump_backward = key_display_text("tui.editor.jumpBackward");
        let page_up = key_display_text("tui.editor.pageUp");
        let page_down = key_display_text("tui.editor.pageDown");

        // Editing keybindings.
        let submit = key_display_text("tui.input.submit");
        let new_line = key_display_text("tui.input.newLine");
        let delete_word_backward = key_display_text("tui.editor.deleteWordBackward");
        let delete_word_forward = key_display_text("tui.editor.deleteWordForward");
        let delete_to_line_start = key_display_text("tui.editor.deleteToLineStart");
        let delete_to_line_end = key_display_text("tui.editor.deleteToLineEnd");
        let yank = key_display_text("tui.editor.yank");
        let yank_pop = key_display_text("tui.editor.yankPop");
        let undo = key_display_text("tui.editor.undo");
        let tab = key_display_text("tui.input.tab");

        // App keybindings.
        let interrupt = key_display_text("app.interrupt");
        let clear = key_display_text("app.clear");
        let exit = key_display_text("app.exit");
        let suspend = key_display_text("app.suspend");
        let cycle_thinking_level = key_display_text("app.thinking.cycle");
        let cycle_model_forward = key_display_text("app.model.cycleForward");
        let select_model = key_display_text("app.model.select");
        let expand_tools = key_display_text("app.tools.expand");
        let toggle_thinking = key_display_text("app.thinking.toggle");
        let external_editor = key_display_text("app.editor.external");
        let cycle_model_backward = key_display_text("app.model.cycleBackward");
        let copy_message = key_display_text("app.message.copy");
        let follow_up = key_display_text("app.message.followUp");
        let dequeue = key_display_text("app.message.dequeue");
        let paste_image = key_display_text("app.clipboard.pasteImage");

        // The markdown template matches the upstream hotkeys string
        // (interactive-mode.ts:5786-5832); the win32 new-line annotation is
        // skipped (rpi is not Windows-specific here).
        let hotkeys = format!(
            r#"
**Navigation**
| Key | Action |
|-----|--------|
| `{cursor_up}` / `{cursor_down}` / `{cursor_left}` / `{cursor_right}` | Move cursor / browse history |
| `{cursor_word_left}` / `{cursor_word_right}` | Move by word |
| `{cursor_line_start}` | Start of line |
| `{cursor_line_end}` | End of line |
| `{jump_forward}` | Jump forward to character |
| `{jump_backward}` | Jump backward to character |
| `{page_up}` / `{page_down}` | Scroll by page |

**Editing**
| Key | Action |
|-----|--------|
| `{submit}` | Send message |
| `{new_line}` | New line |
| `{delete_word_backward}` | Delete word backwards |
| `{delete_word_forward}` | Delete word forwards |
| `{delete_to_line_start}` | Delete to start of line |
| `{delete_to_line_end}` | Delete to end of line |
| `{yank}` | Paste the most-recently-deleted text |
| `{yank_pop}` | Cycle through the deleted text after pasting |
| `{undo}` | Undo |

**Other**
| Key | Action |
|-----|--------|
| `{tab}` | Path completion / accept autocomplete |
| `{interrupt}` | Cancel autocomplete / abort streaming |
| `{clear}` | Clear editor (first) / exit (second) |
| `{exit}` | Exit (when editor is empty) |
| `{suspend}` | Suspend to background |
| `{cycle_thinking_level}` | Cycle thinking level |
| `{cycle_model_forward}` / `{cycle_model_backward}` | Cycle models |
| `{select_model}` | Open model selector |
| `{expand_tools}` | Toggle tool output expansion |
| `{toggle_thinking}` | Toggle thinking block visibility |
| `{external_editor}` | Edit message in external editor |
| `{copy_message}` | Copy last assistant message |
| `{follow_up}` | Queue follow-up message |
| `{dequeue}` | Restore queued messages |
| `{paste_image}` | Paste image or text from clipboard |
| `/` | Slash commands |
| `!` | Run bash command |
| `!!` | Run bash command (excluded from context) |
"#
        );

        // TODO(T15): extension-registered shortcuts section
        // (extensionRunner.getShortcuts, interactive-mode.ts:5835-5848).
        let border_theme = Arc::clone(&lock(&self.theme));
        let border = Box::new(move |text: &str| border_theme.fg("border", text));

        let mut chat = lock(&self.chat_container);
        chat.children.push(Box::new(Spacer::new(1)));
        chat.children
            .push(Box::new(DynamicBorder::new(border.clone())));
        chat.children.push(Box::new(Text::new(
            Theme::bold(&lock(&self.theme).fg("accent", "Keyboard Shortcuts")),
            1,
            0,
            None,
        )));
        chat.children.push(Box::new(Spacer::new(1)));
        chat.children.push(Box::new(Markdown::new(
            hotkeys.trim(),
            1,
            1,
            Arc::clone(&lock(&self.markdown_theme)),
            None,
            None,
        )));
        chat.children.push(Box::new(DynamicBorder::new(border)));
        self.render_handle.request_render();
    }

    /// `handleDebugCommand` (interactive-mode.ts:5874-5905): write the debug
    /// log to `{agent_dir}/rpi-debug.log` and confirm in the chat.
    pub(crate) fn handle_debug_command(&self) {
        let width = self.ui.with_terminal(|terminal| terminal.columns());
        let height = self.ui.terminal_rows();
        // TODO(T14): the "all rendered lines" section
        // (`this.ui.render(width)` + visibleWidth,
        // interactive-mode.ts:5877-5890) — `Tui` has no public full-render
        // API. Until then Total lines is 0 and the section is omitted; the
        // agent-message JSONL section is written.
        let mut debug_data = format!("Debug output at {}\n", now_iso8601());
        debug_data.push_str(&format!("Terminal: {width}x{height}\n"));
        debug_data.push_str("Total lines: 0\n\n");
        debug_data.push_str("=== Agent messages (JSONL) ===\n");
        for message in self.session().messages() {
            debug_data.push_str(&serde_json::to_string(&message).unwrap_or_default());
            debug_data.push('\n');
        }
        debug_data.push('\n');

        let debug_log_path = crate::config::get_agent_dir().join("rpi-debug.log");
        if let Some(dir) = debug_log_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(error) = std::fs::write(&debug_log_path, debug_data) {
            self.show_error(&format!("Failed to write debug log: {error}"));
            return;
        }

        let written_label = lock(&self.theme).fg("accent", "✓ Debug log written");
        let path_label = lock(&self.theme).fg("muted", &debug_log_path.display().to_string());
        let mut chat = lock(&self.chat_container);
        chat.children.push(Box::new(Spacer::new(1)));
        chat.children.push(Box::new(Text::new(
            format!("{}\n{}", written_label, path_label),
            1,
            1,
            None,
        )));
        self.render_handle.request_render();
    }

    /// `handleCompactCommand` (interactive-mode.ts:6018-6026). Errors are
    /// ignored — compaction failures surface through the event stream.
    pub(crate) async fn handle_compact_command(&self, args: &str) {
        // `/compact ` prefix is 9 chars (`text.slice(9).trim()`,
        // interactive-mode.ts:2753); `/compact` alone passes None.
        let custom_instructions = args
            .strip_prefix("/compact ")
            .map(str::trim)
            .filter(|instructions| !instructions.is_empty());
        let _ = self.session().compact(custom_instructions).await;
    }

    /// `/quit` (interactive-mode.ts:2783-2787): signal the run loop to shut
    /// down.
    pub(crate) fn handle_quit_command(&self) {
        let _ = self.shutdown_tx.send(true);
    }
}

impl InteractiveMode {
    /// `handleImportCommand` (interactive-mode.ts:5467-5509): replace the
    /// current session with the one in the JSONL file.
    pub(crate) async fn handle_import_command(&mut self, args: &str) {
        let Some(input_path) = get_path_command_argument(args, "/import") else {
            self.ui_state.show_error("Usage: /import <path.jsonl>");
            return;
        };
        // TODO(T15): extension confirmation prompt (showExtensionConfirm,
        // interactive-mode.ts:5474-5478).
        match self.runtime.import_from_jsonl(&input_path, None).await {
            Ok(true) => self.ui_state.show_status("Import cancelled"),
            Ok(false) => {
                // Full rebind (upstream `rebindCurrentSession` via the
                // `setRebindSession` hook, interactive-mode.ts:4143-4145):
                // detach the old subscription, swap `ui_state.session`,
                // rebuild the chat from the imported entries, re-subscribe.
                self.rebind_session_ui().await;
                self.ui_state
                    .show_status(&format!("Session imported from: {input_path}"));
            }
            // Upstream distinguishes MissingSessionCwdError (cwd prompt,
            // interactive-mode.ts:5489-5501) and SessionImportFileNotFoundError
            // (interactive-mode.ts:5503-5505); the cwd prompt is a T15
            // extension hook — local reports all failures uniformly.
            Err(error) => self.ui_state.show_error(&format!(
                "Failed to import session: {}",
                error.raw_message()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::interactive::interactive_mode::InteractiveModeOptions;
    use crate::modes::interactive::test_support::{
        build_test_session, install_noop_product_transports, TestTerminal,
    };
    use rpi_agent::messages::AgentMessage;
    use rpi_ai::types::{ApiKind, AssistantMessage, AssistantRole, StopReason, Usage};
    use rpi_tui::tui::Component;

    // ---------------------------------------------------------------------
    // Fixtures
    // ---------------------------------------------------------------------

    fn assistant_message(
        content: Vec<rpi_ai::types::AssistantContent>,
        stop_reason: StopReason,
    ) -> AgentMessage {
        AgentMessage::Assistant(AssistantMessage {
            role: AssistantRole::Assistant,
            content,
            api: ApiKind("openai-completions".into()),
            provider: "custom".to_string(),
            model: "m1".to_string(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason,
            error_message: None,
            timestamp: 1_700_000_000_000,
        })
    }

    fn text_content(text: &str) -> rpi_ai::types::AssistantContent {
        rpi_ai::types::AssistantContent::Text(rpi_ai::types::TextContent {
            text: text.to_string(),
            text_signature: None,
        })
    }

    fn user_message(text: &str) -> AgentMessage {
        AgentMessage::User(rpi_ai::types::UserMessage {
            role: rpi_ai::types::UserRole::User,
            content: rpi_ai::types::UserContent::Text(text.to_string()),
            timestamp: 1_700_000_000_000,
        })
    }

    /// Build a mode over the test session and terminal (same pattern as the
    /// interactive_mode.rs tests).
    async fn mode_harness() -> (InteractiveMode, Arc<TestTerminal>) {
        let harness = build_test_session().await;
        let terminal = Arc::new(TestTerminal::new());
        let mode = InteractiveMode::with_terminal(
            harness.runtime,
            InteractiveModeOptions::default(),
            Box::new(TestTerminal::clone(&terminal)),
        );
        // M1 (T14 review): no product-endpoint requests from unit tests.
        install_noop_product_transports(&mode);
        (mode, terminal)
    }

    /// Strip ANSI SGR escape sequences (test-local; the rpi-tui helpers are
    /// module-private).
    fn strip_ansi(text: &str) -> String {
        let mut out = String::new();
        let mut in_escape = false;
        for c in text.chars() {
            if in_escape {
                if c.is_ascii_alphabetic() {
                    in_escape = false;
                }
                continue;
            }
            if c == '\x1b' {
                in_escape = true;
                continue;
            }
            out.push(c);
        }
        out
    }

    fn chat_render(ui: &InteractiveUi) -> String {
        lock(&ui.chat_container).render(60).join("\n")
    }

    fn seed_user_message(session: &crate::core::agent_session::AgentSession, text: &str) {
        let manager = session.session_manager();
        let mut manager = manager.lock().unwrap_or_else(|e| e.into_inner());
        manager
            .append_message(user_message(text))
            .expect("append user message");
    }

    /// Serialize env access for tests that must set `RPI_CODING_AGENT_DIR`
    /// (the debug-log path is read through the env).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    // ---------------------------------------------------------------------
    // /session
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn session_command_renders_stats() {
        let (mode, _terminal) = mode_harness().await;
        let ui = &mode.ui_state;
        seed_user_message(&ui.session(), "first question");
        ui.handle_session_command();
        let rendered = strip_ansi(&chat_render(ui));
        assert!(rendered.contains("Session Info"), "rendered: {rendered}");
        assert!(rendered.contains("Messages"), "rendered: {rendered}");
        assert!(rendered.contains("Tokens"), "rendered: {rendered}");
        assert!(rendered.contains("User: 1"), "user count: {rendered}");
        assert!(
            rendered.contains("In-memory"),
            "in-memory session file label: {rendered}"
        );
        assert!(rendered.contains("User: 1"), "user count: {rendered}");
    }

    // ---------------------------------------------------------------------
    // /name
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn name_command_sets_session_name() {
        let (mode, _terminal) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.handle_name_command("/name  my session  ");
        assert_eq!(ui.session().session_name().as_deref(), Some("my session"));
        let rendered = chat_render(ui);
        assert!(
            rendered.contains("Session name set: my session"),
            "rendered: {rendered}"
        );
    }

    #[tokio::test]
    async fn name_command_empty_shows_current_name() {
        let (mode, _terminal) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.handle_name_command("/name");
        let rendered = chat_render(ui);
        assert!(
            rendered.contains("Usage: /name <name>"),
            "no current name → usage warning: {rendered}"
        );

        ui.session().set_session_name("existing");
        ui.handle_name_command("/name  ");
        let rendered = chat_render(ui);
        assert!(
            rendered.contains("Session name: existing"),
            "current name shown: {rendered}"
        );
    }

    // ---------------------------------------------------------------------
    // /copy
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn copy_command_empty_shows_error() {
        let (mode, _terminal) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.handle_copy_command();
        let rendered = chat_render(ui);
        assert!(
            rendered.contains("No agent messages to copy yet."),
            "rendered: {rendered}"
        );
    }

    #[tokio::test]
    async fn copy_command_writes_osc52() {
        let (mode, terminal) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.session().agent().set_messages(vec![assistant_message(
            vec![text_content("hello")],
            StopReason::Stop,
        )]);
        ui.handle_copy_command();
        let writes = terminal.writes();
        assert!(
            writes.contains("\x1b]52;c;") && writes.ends_with('\x07'),
            "OSC 52 written: {:?}",
            writes
        );
        let rendered = chat_render(ui);
        assert!(
            rendered.contains("Copied last agent message to clipboard"),
            "rendered: {rendered}"
        );
    }

    // ---------------------------------------------------------------------
    // /export
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn export_command_writes_jsonl_file() {
        let (mode, _terminal) = mode_harness().await;
        let ui = &mode.ui_state;
        seed_user_message(&ui.session(), "export me");
        let tmp = crate::modes::interactive::test_support::TempDir::new();
        let output = tmp.path().join("session.jsonl");
        ui.handle_export_command(&format!("/export {}", output.display()));
        assert!(output.exists(), "jsonl file written");
        let contents = std::fs::read_to_string(&output).expect("read jsonl");
        assert!(
            contents.contains("export me"),
            "message in jsonl: {contents}"
        );
        let rendered = chat_render(ui);
        assert!(
            rendered.contains("Session exported to:"),
            "rendered: {rendered}"
        );
    }

    #[tokio::test]
    async fn export_command_html_in_memory_session_errors() {
        let (mode, _terminal) = mode_harness().await;
        let ui = &mode.ui_state;
        // The harness session is in-memory → the upstream
        // "Cannot export in-memory session to HTML" error
        // (index.ts:244-246) surfaces via the shared export error wrapper.
        ui.handle_export_command("/export out.html");
        let rendered = chat_render(ui);
        // The 60-col test render wraps the message; assert the unwrapped
        // fragments.
        assert!(
            rendered.contains("Failed to export session: Cannot export in-memory"),
            "rendered: {rendered}"
        );
        assert!(rendered.contains("session to HTML"), "rendered: {rendered}");
    }

    // ---------------------------------------------------------------------
    // /share
    // ---------------------------------------------------------------------

    use crate::core::share::{GhAuthStatus, GistCreateOutcome, ShareRunner};

    struct MockShareRunner {
        auth: GhAuthStatus,
        outcome: GistCreateOutcome,
        /// Recorded gist file argument (assert the temp HTML path).
        gist_file: Mutex<Option<std::path::PathBuf>>,
    }

    impl MockShareRunner {
        fn ok() -> Self {
            MockShareRunner {
                auth: GhAuthStatus::Ok,
                outcome: GistCreateOutcome {
                    code: Some(0),
                    stdout: "https://gist.github.com/user/abc123\n".to_string(),
                    stderr: String::new(),
                },
                gist_file: Mutex::new(None),
            }
        }
    }

    impl ShareRunner for MockShareRunner {
        fn auth_status(&self) -> GhAuthStatus {
            self.auth
        }

        fn gist_create(
            &self,
            file: &std::path::Path,
            _cancelled: Arc<AtomicBool>,
        ) -> GistCreateOutcome {
            *lock(&self.gist_file) = Some(file.to_path_buf());
            self.outcome.clone()
        }
    }

    /// Drain until `pred` matches the chat (the gist worker reports back on
    /// a thread, so the test polls the queue).
    fn drain_until(ui: &InteractiveUi, pred: impl Fn(&str) -> bool) -> String {
        for _ in 0..200 {
            ui.drain_events();
            let rendered = chat_render(ui);
            if pred(&rendered) {
                return rendered;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        chat_render(ui)
    }

    #[tokio::test]
    async fn share_command_gh_not_installed() {
        let (mode, _terminal) = mode_harness().await;
        let ui = &mode.ui_state;
        let mut runner = MockShareRunner::ok();
        runner.auth = GhAuthStatus::NotInstalled;
        ui.set_share_runner(Arc::new(runner));
        ui.handle_share_command();
        let rendered = chat_render(ui);
        // 60-col render wraps; assert the unwrapped fragments.
        assert!(
            rendered.contains("GitHub CLI (gh) is not installed."),
            "rendered: {rendered}"
        );
        assert!(
            rendered.contains("https://cli.github.com/"),
            "rendered: {rendered}"
        );
    }

    #[tokio::test]
    async fn share_command_gh_not_logged_in() {
        let (mode, _terminal) = mode_harness().await;
        let ui = &mode.ui_state;
        let mut runner = MockShareRunner::ok();
        runner.auth = GhAuthStatus::NotLoggedIn;
        ui.set_share_runner(Arc::new(runner));
        ui.handle_share_command();
        let rendered = chat_render(ui);
        // 60-col render wraps; assert the unwrapped fragments.
        assert!(
            rendered.contains("GitHub CLI is not logged in."),
            "rendered: {rendered}"
        );
        assert!(rendered.contains("gh auth login"), "rendered: {rendered}");
    }

    #[tokio::test]
    async fn share_command_in_memory_session_fails_export() {
        let (mode, _terminal) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.set_share_runner(Arc::new(MockShareRunner::ok()));
        ui.handle_share_command();
        let rendered = chat_render(ui);
        // The 60-col test render wraps the message; assert the unwrapped
        // fragments.
        assert!(
            rendered.contains("Failed to export session: Cannot export in-memory"),
            "rendered: {rendered}"
        );
        assert!(rendered.contains("session to HTML"), "rendered: {rendered}");
    }

    /// File-backed `/share` harness: gist create succeeds/fails against the
    /// mock; completion settles through the drain.
    struct ShareHarness {
        mode: InteractiveMode,
        /// Outlives `mode` (field drop order): the file-backed session
        /// manager's directory.
        _tmp: crate::modes::interactive::test_support::TempDir,
    }

    async fn share_harness(runner: Arc<dyn ShareRunner>) -> ShareHarness {
        // A file-backed session so the temp HTML export works.
        let tmp = crate::modes::interactive::test_support::TempDir::new();
        let session_file = tmp.path().join("s.jsonl");
        std::fs::write(
            &session_file,
            concat!(
                "{\"type\":\"session\",\"version\":3,\"id\":\"sess-share\",\"timestamp\":\"2025-01-01T00:00:00.000Z\",\"cwd\":\"/tmp\"}\n",
                "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2025-01-01T00:00:00.000Z\",\"message\":{\"role\":\"user\",\"content\":\"share me\",\"timestamp\":1}}\n",
            ),
        )
        .expect("write session file");
        let manager = crate::core::session_manager::SessionManager::open(
            &session_file,
            Some(tmp.path()),
            None,
        )
        .expect("open session");
        let harness =
            crate::modes::interactive::test_support::build_test_session_with_manager(Some(manager))
                .await;
        let terminal = Arc::new(TestTerminal::new());
        let mode = InteractiveMode::with_terminal(
            harness.runtime,
            InteractiveModeOptions::default(),
            Box::new(TestTerminal::clone(&terminal)),
        );
        mode.ui_state.set_share_runner(runner);
        ShareHarness { mode, _tmp: tmp }
    }

    /// Success / failure / parse-failure settle through the gist worker +
    /// drain. The temp payload lives in a unique `rpi-share-{pid}-{nanos}/`
    /// subdirectory (basename `session.html`, interactive-mode.ts:5526).
    #[tokio::test]
    async fn share_command_completion_paths() {
        // Success: viewer URL + gist URL; temp payload cleaned up.
        let runner = Arc::new(MockShareRunner::ok());
        let harness = share_harness(runner.clone()).await;
        let ui = &harness.mode.ui_state;
        ui.handle_share_command();
        let rendered = drain_until(ui, |r| r.contains("Share URL:"));
        assert!(
            rendered.contains("Share URL: https://revpi.dev/session/#abc123"),
            "rendered: {rendered}"
        );
        assert!(
            rendered.contains("Gist: https://gist.github.com/user/abc123"),
            "rendered: {rendered}"
        );
        // The gist payload is the temp HTML (interactive-mode.ts:5526,
        // 5563) and the drain cleaned it up (interactive-mode.ts:5543-5545).
        let gist_file = lock(&runner.gist_file).clone().expect("gist file recorded");
        assert_eq!(gist_file.file_name().expect("basename"), "session.html");
        let share_dir = gist_file.parent().expect("share dir").to_path_buf();
        assert!(
            share_dir
                .file_name()
                .expect("dir name")
                .to_string_lossy()
                .starts_with("rpi-share-"),
            "unique share dir: {}",
            share_dir.display()
        );
        assert!(!gist_file.exists(), "temp html cleaned up after success");
        assert!(!share_dir.exists(), "share dir cleaned up after success");
        drop(harness);

        // gh exits non-zero → trimmed stderr (interactive-mode.ts:5578-5582).
        let mut failing = MockShareRunner::ok();
        failing.outcome = GistCreateOutcome {
            code: Some(1),
            stdout: String::new(),
            stderr: "  HTTP 403: forbidden  \n".to_string(),
        };
        let harness = share_harness(Arc::new(failing)).await;
        let ui = &harness.mode.ui_state;
        ui.handle_share_command();
        let rendered = drain_until(ui, |r| r.contains("Failed to create gist"));
        assert!(
            rendered.contains("Failed to create gist: HTTP 403: forbidden"),
            "rendered: {rendered}"
        );
        drop(harness);

        // Unparseable gh stdout → gist ID error (interactive-mode.ts:5588-5590).
        let mut empty = MockShareRunner::ok();
        empty.outcome = GistCreateOutcome {
            code: Some(0),
            stdout: "   \n".to_string(),
            stderr: String::new(),
        };
        let harness = share_harness(Arc::new(empty)).await;
        let ui = &harness.mode.ui_state;
        ui.handle_share_command();
        let rendered = drain_until(ui, |r| r.contains("Failed to parse gist ID"));
        assert!(
            rendered.contains("Failed to parse gist ID from gh output"),
            "rendered: {rendered}"
        );
    }

    /// The loader's abort settles the share with "Share cancelled"
    /// (interactive-mode.ts:5549-5554); the late worker completion is
    /// ignored (single `take` on the share state).
    #[tokio::test]
    async fn share_command_abort_cancels() {
        struct BlockingRunner {
            entered: Arc<AtomicBool>,
        }
        impl ShareRunner for BlockingRunner {
            fn auth_status(&self) -> GhAuthStatus {
                GhAuthStatus::Ok
            }

            fn gist_create(
                &self,
                _file: &std::path::Path,
                cancelled: Arc<AtomicBool>,
            ) -> GistCreateOutcome {
                // Signals the wait loop below (T14 review: the previous
                // wait probed the pre-D-045 fixed `{tmpdir}/session.html`
                // path, which the unique `rpi-share-*` layout never
                // produces — the loop spun for its full duration).
                self.entered.store(true, Ordering::Relaxed);
                for _ in 0..2000 {
                    if cancelled.load(Ordering::Relaxed) {
                        // Killed child (upstream `proc.kill()`).
                        return GistCreateOutcome {
                            code: None,
                            stdout: String::new(),
                            stderr: String::new(),
                        };
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                panic!("abort flag never reached the gist worker");
            }
        }

        let entered = Arc::new(AtomicBool::new(false));
        let harness = share_harness(Arc::new(BlockingRunner {
            entered: Arc::clone(&entered),
        }))
        .await;
        let ui = &harness.mode.ui_state;
        ui.handle_share_command();
        // Wait for the worker to be in flight (the runner's entry flag —
        // the gist file itself is written by `gh`, which the mock never
        // spawns).
        for _ in 0..200 {
            if entered.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(entered.load(Ordering::Relaxed), "gist worker never started");
        // The loader's on_abort queues `ShareAbort` (component callback →
        // drain); invoke the queued path directly.
        ui.push(UiCommand::ShareAbort);
        let rendered = drain_until(ui, |r| r.contains("Share cancelled"));
        assert!(rendered.contains("Share cancelled"), "rendered: {rendered}");
        // The late completion is a no-op: no gist error surfaces.
        let rendered = drain_until(ui, |r| r.contains("Failed to create gist"));
        assert!(
            !rendered.contains("Failed to create gist"),
            "late completion ignored: {rendered}"
        );
    }

    // ---------------------------------------------------------------------
    // /import
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn import_command_missing_arg_shows_usage() {
        let (mut mode, _terminal) = mode_harness().await;
        mode.handle_import_command("/import").await;
        let rendered = chat_render(&mode.ui_state);
        assert!(
            rendered.contains("Usage: /import <path.jsonl>"),
            "rendered: {rendered}"
        );
    }

    #[tokio::test]
    async fn import_command_missing_file_reports_error() {
        let (mut mode, _terminal) = mode_harness().await;
        let tmp = crate::modes::interactive::test_support::TempDir::new();
        let missing = tmp.path().join("nope.jsonl");
        mode.handle_import_command(&format!("/import {}", missing.display()))
            .await;
        let rendered = chat_render(&mode.ui_state);
        assert!(
            rendered.contains("Failed to import session: File not found"),
            "rendered: {rendered}"
        );
    }

    // ---------------------------------------------------------------------
    // /changelog
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn changelog_command_renders_placeholder() {
        let (mode, _terminal) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.handle_changelog_command();
        let rendered = chat_render(ui);
        assert!(rendered.contains("What's New"), "rendered: {rendered}");
        assert!(
            rendered.contains("No changelog entries found."),
            "rendered: {rendered}"
        );
        // The border frame is present (structure parity).
        assert!(rendered.contains('─'), "rendered: {rendered}");
    }

    // ---------------------------------------------------------------------
    // /hotkeys
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn hotkeys_command_renders_tables() {
        let (mode, _terminal) = mode_harness().await;
        let ui = &mode.ui_state;
        // The keybinding globals must be installed for key_display_text.
        crate::modes::interactive::interactive_mode::install_global_keybindings();
        ui.handle_hotkeys_command();
        let rendered = strip_ansi(&chat_render(ui));
        assert!(
            rendered.contains("Keyboard Shortcuts"),
            "rendered: {rendered}"
        );
        assert!(rendered.contains("Navigation"), "rendered: {rendered}");
        assert!(rendered.contains("Editing"), "rendered: {rendered}");
        assert!(rendered.contains("Other"), "rendered: {rendered}");
        assert!(rendered.contains("Move cursor"), "rendered: {rendered}");
        assert!(rendered.contains("Slash commands"), "rendered: {rendered}");
    }

    // ---------------------------------------------------------------------
    // /debug
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn debug_command_writes_log_with_messages() {
        let (mode, _terminal) = mode_harness().await;
        let ui = &mode.ui_state;
        ui.session().agent().set_messages(vec![assistant_message(
            vec![text_content("debug me")],
            StopReason::Stop,
        )]);

        let tmp = crate::modes::interactive::test_support::TempDir::new();
        let agent_dir = tmp.path().join("agent-debug");

        let log_path = {
            let _guard = lock(&ENV_LOCK);
            std::env::set_var("RPI_CODING_AGENT_DIR", &agent_dir);
            // Resolved while the env override is active (get_agent_dir reads
            // the env at call time).
            let log_path = crate::config::get_agent_dir().join("rpi-debug.log");
            ui.handle_debug_command();
            std::env::remove_var("RPI_CODING_AGENT_DIR");
            log_path
        };

        let contents = std::fs::read_to_string(&log_path).expect("debug log written");
        assert!(
            contents.starts_with("Debug output at "),
            "header: {contents}"
        );
        assert!(contents.contains("Terminal: 80x24"), "contents: {contents}");
        assert!(
            contents.contains("=== Agent messages (JSONL) ==="),
            "contents: {contents}"
        );
        assert!(contents.contains("debug me"), "contents: {contents}");

        let rendered = chat_render(ui);
        assert!(
            rendered.contains("✓ Debug log written"),
            "rendered: {rendered}"
        );
        assert!(rendered.contains("rpi-debug.log"), "rendered: {rendered}");
    }

    // ---------------------------------------------------------------------
    // /quit
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn quit_command_signals_shutdown() {
        let (mode, _terminal) = mode_harness().await;
        assert!(!*mode.shutdown_rx.borrow());
        mode.ui_state.handle_quit_command();
        assert!(*mode.shutdown_rx.borrow(), "shutdown signal sent");
    }

    // ---------------------------------------------------------------------
    // path argument parsing
    // ---------------------------------------------------------------------

    #[test]
    fn path_argument_parses_quoted_and_plain() {
        assert_eq!(get_path_command_argument("/export", "/export"), None);
        assert_eq!(get_path_command_argument("/export ", "/export"), None);
        assert_eq!(
            get_path_command_argument("/export out/session.jsonl", "/export"),
            Some("out/session.jsonl".to_string())
        );
        assert_eq!(
            get_path_command_argument("/import \"a b/c.jsonl\" rest", "/import"),
            Some("a b/c.jsonl".to_string())
        );
        assert_eq!(
            get_path_command_argument("/import 'x.jsonl'", "/import"),
            Some("x.jsonl".to_string())
        );
    }
}
