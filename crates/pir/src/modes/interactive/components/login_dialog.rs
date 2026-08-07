//! Port of `login-dialog.ts` @ pi 0.82.1 (2efa728) — T12-S5a.
//!
//! The upstream class holds a content `Container` that `show*` methods
//! append to or clear (login-dialog.ts:96-220). The port renders from a
//! single [`LoginDialogState`] instead.
//!
//! Intentional differences:
//! - The theme is injected (`Arc<Theme>`) instead of read from the global
//!   `theme` getter (theme.ts:799-816); the [`DynamicBorder`] color fn is
//!   explicit (dynamic-border.ts:14).
//! - Content is replaced per mode, not accumulated: upstream `showPrompt`,
//!   `showInfo`, `showWaiting` and `showProgress` append to the existing
//!   children (so e.g. the auth URL stays visible above a later prompt,
//!   login-dialog.ts:154-156); the port renders only the latest mode. The
//!   integration layer should re-issue a `show_*` for every state it wants
//!   on screen (T13 `AuthEvent` handling). For the device-code flow the
//!   follow-up `showWaiting` line is folded into
//!   [`LoginDialogState::DeviceCode::waiting`] so both stay visible.
//! - `select` prompts render inside the dialog ([`LoginDialogState::Select`])
//!   instead of swapping the editor container for an
//!   `ExtensionSelectorComponent` (upstream `showAuthSelect`,
//!   interactive-mode.ts:5204-5235) — the `AuthInteraction` adapter stays
//!   mounted and the restore step is skipped.
//! - Pending-input promises (`inputResolver`/`inputRejecter`,
//!   login-dialog.ts:16-17) are replaced by the `on_submit_manual` /
//!   `on_prompt_confirm` / `on_select` / `on_cancel` callback fields; Enter
//!   is intercepted in [`Component::handle_input`] because a Rust closure
//!   cannot borrow the component mutably (upstream wires `input.onSubmit`,
//!   login-dialog.ts:56-64).
//! - `on_cancel` replaces `onComplete(false, "Login cancelled")`
//!   (login-dialog.ts:83-91). There is no `AbortSignal`: the flow-cancellation
//!   contract is that the integration layer tears the login down when
//!   `on_cancel` fires (upstream `cancel` aborts `signal`,
//!   login-dialog.ts:73-75, 84).
//! - [`LoginDialogComponent::show_auth`] fires `on_begin_oauth` instead of
//!   opening the browser (upstream calls `openBrowser(url)`,
//!   login-dialog.ts:111); [`LoginDialogComponent::show_device_code`] fires
//!   `on_begin_device_code` (upstream does nothing there — a T13 convenience
//!   hook). Both are `Option` fields so the hooks stay inert until wired.
//! - No `requestRender` (upstream `this.tui.requestRender()`, login-dialog.ts
//!   :112, 130, …): the tree wrapper owns render scheduling.
//! - Waiting/progress lines are plain dim `Text` lines exactly as upstream;
//!   the port does not depend on the (cross-group) `bordered_loader`.

use std::sync::Arc;

use pir_ai::auth::interaction::{AuthInfoLink, SelectOption};
use pir_tui::components::input::Input;
use pir_tui::components::spacer::Spacer;
use pir_tui::components::text::Text;
use pir_tui::keybindings::get_keybindings;
use pir_tui::tui::{Component, Focusable};

use crate::core::themes::Theme;
use crate::modes::interactive::components::dynamic_border::DynamicBorder;
use crate::modes::interactive::components::keybinding_hints::key_hint;

/// Display mode of the dialog, set by the `show_*` methods
/// (login-dialog.ts:96-220).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginDialogState {
    /// `showAuth` (login-dialog.ts:96-113).
    Auth {
        url: String,
        instructions: Option<String>,
    },
    /// `showDeviceCode` (login-dialog.ts:118-131) plus the follow-up
    /// `showWaiting` line (interactive-mode.ts:5264-5266). Upstream appends
    /// both to the content container; the port's render-from-state model
    /// folds the waiting message into the state so the integration layer can
    /// re-issue a single show call (module docs: content replaced per mode).
    DeviceCode {
        user_code: String,
        verification_uri: String,
        waiting: Option<String>,
    },
    /// `showManualInput` (login-dialog.ts:136-148).
    ManualInput { prompt: String },
    /// `showPrompt` (login-dialog.ts:154-176).
    Prompt {
        message: String,
        placeholder: Option<String>,
    },
    /// In-dialog `select` prompt. Upstream swaps the editor container for a
    /// separate `ExtensionSelectorComponent` (interactive-mode.ts:5204-5235
    /// `showAuthSelect`); the port folds the selection into the dialog so
    /// the `AuthInteraction` adapter stays mounted (intentional difference,
    /// see module docs).
    Select {
        message: String,
        options: Vec<SelectOption>,
        selected: usize,
    },
    /// `showInfo` (login-dialog.ts:189-202).
    Info {
        message: String,
        links: Vec<AuthInfoLink>,
        show_close_hint: bool,
    },
    /// `showWaiting` (login-dialog.ts:207-212).
    Waiting { message: String },
    /// `showProgress` (login-dialog.ts:217-220).
    Progress { message: String },
    /// `showDetails` (login-dialog.ts:179-186).
    Details(Vec<String>),
}

/// Login dialog component - replaces editor during OAuth login flow
/// (login-dialog.ts:11-233).
/// `onSelect` callback (select prompt resolution).
pub type SelectCallback = Box<dyn FnMut(&str) + Send>;

pub struct LoginDialogComponent {
    theme: Arc<Theme>,
    title: String,
    state: Option<LoginDialogState>,
    input: Input,
    /// Echo of the last submitted value (`replaceInputWithSubmittedText`,
    /// login-dialog.ts:77-81): after a submit the input is replaced by a
    /// `> value` line until the next `show_*` call.
    submitted: Option<String>,
    top_border: DynamicBorder,
    bottom_border: DynamicBorder,

    // Focusable implementation - propagate to input for IME cursor
    // positioning (login-dialog.ts:20-28).
    focused: bool,

    /// Fired by [`Self::show_auth`] (upstream `openBrowser`, login-dialog.ts:111).
    pub on_begin_oauth: Option<Box<dyn FnMut() + Send>>,
    /// Fired by [`Self::show_device_code`]; no upstream equivalent.
    pub on_begin_device_code: Option<Box<dyn FnMut() + Send>>,
    /// Submit of a manual-input prompt (login-dialog.ts:144-147).
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    pub on_submit_manual: Option<Box<dyn FnMut(&str) + Send>>,
    /// Submit of a text prompt (login-dialog.ts:172-175).
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    pub on_prompt_confirm: Option<Box<dyn FnMut(&str) + Send>>,
    /// Selection of a `select` prompt option (upstream `showAuthSelect`
    /// resolves with the option id, interactive-mode.ts:5221-5223).
    pub on_select: Option<SelectCallback>,
    /// Cancel / escape (upstream `cancel`, login-dialog.ts:83-91).
    pub on_cancel: Option<Box<dyn FnMut() + Send>>,
}

/// `(text) => theme.fg("border", text)` (dynamic-border.ts:14).
fn border_color(theme: &Arc<Theme>) -> Box<dyn Fn(&str) -> String + Send + Sync> {
    let theme = theme.clone();
    Box::new(move |text| theme.fg("border", text))
}

/// OSC 8 hyperlink with BEL terminator (login-dialog.ts:99).
fn osc8_link(url: &str, text: &str) -> String {
    format!("\x1b]8;;{url}\x07{text}\x1b]8;;\x07")
}

/// Click hint depends on the platform (login-dialog.ts:102).
fn click_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "Cmd+click to open"
    } else {
        "Ctrl+click to open"
    }
}

impl LoginDialogComponent {
    /// `constructor` (login-dialog.ts:30-71). `provider_name_override`
    /// defaults to the provider id; `title_override` defaults to
    /// `Login to {providerName}`. `onComplete` (login-dialog.ts:18) is
    /// represented by the [`Self::on_cancel`] field, set after construction.
    pub fn new(
        theme: Arc<Theme>,
        provider_id: &str,
        provider_name_override: Option<&str>,
        title_override: Option<&str>,
    ) -> Self {
        let provider_name = provider_name_override.unwrap_or(provider_id);
        let title = match title_override {
            Some(title) => title.to_string(),
            None => format!("Login to {provider_name}"),
        };
        let top_border = DynamicBorder::new(border_color(&theme));
        let bottom_border = DynamicBorder::new(border_color(&theme));

        Self {
            theme,
            title,
            state: None,
            input: Input::new(),
            submitted: None,
            top_border,
            bottom_border,
            focused: false,
            on_begin_oauth: None,
            on_begin_device_code: None,
            on_submit_manual: None,
            on_prompt_confirm: None,
            on_select: None,
            on_cancel: None,
        }
    }

    /// The current display mode.
    pub fn state(&self) -> Option<&LoginDialogState> {
        self.state.as_ref()
    }

    /// `showAuth` (login-dialog.ts:96-113).
    pub fn show_auth(&mut self, url: &str, instructions: Option<&str>) {
        self.state = Some(LoginDialogState::Auth {
            url: url.to_string(),
            instructions: instructions.map(str::to_string),
        });
        self.submitted = None;
        if let Some(on_begin_oauth) = self.on_begin_oauth.as_mut() {
            on_begin_oauth();
        }
    }

    /// `showDeviceCode` (login-dialog.ts:118-131) + `showWaiting`
    /// (interactive-mode.ts:5264-5266). `waiting` is the message the
    /// integration layer wants visible under the code (upstream appends it
    /// via a separate `showWaiting` call; the port renders one state).
    pub fn show_device_code(
        &mut self,
        user_code: &str,
        verification_uri: &str,
        waiting: Option<&str>,
    ) {
        self.state = Some(LoginDialogState::DeviceCode {
            user_code: user_code.to_string(),
            verification_uri: verification_uri.to_string(),
            waiting: waiting.map(str::to_string),
        });
        self.submitted = None;
        if let Some(on_begin_device_code) = self.on_begin_device_code.as_mut() {
            on_begin_device_code();
        }
    }

    /// `showManualInput` (login-dialog.ts:136-148).
    pub fn show_manual_input(&mut self, prompt: &str) {
        self.input.set_value("");
        self.state = Some(LoginDialogState::ManualInput {
            prompt: prompt.to_string(),
        });
        self.submitted = None;
    }

    /// `showPrompt` (login-dialog.ts:154-176).
    pub fn show_prompt(&mut self, message: &str, placeholder: Option<&str>) {
        self.input.set_value("");
        self.state = Some(LoginDialogState::Prompt {
            message: message.to_string(),
            placeholder: placeholder.map(str::to_string),
        });
        self.submitted = None;
    }

    /// In-dialog `select` prompt (upstream `showAuthSelect`,
    /// interactive-mode.ts:5204-5235). The first option is pre-selected;
    /// Enter fires [`Self::on_select`] with the option id.
    pub fn show_select(&mut self, message: &str, options: Vec<SelectOption>) {
        self.state = Some(LoginDialogState::Select {
            message: message.to_string(),
            options,
            selected: 0,
        });
        self.submitted = None;
    }

    /// `showDetails` (login-dialog.ts:179-186).
    pub fn show_details(&mut self, lines: Vec<String>) {
        self.state = Some(LoginDialogState::Details(lines));
        self.submitted = None;
    }

    /// `showInfo` (login-dialog.ts:189-202).
    pub fn show_info(&mut self, message: &str, links: Vec<AuthInfoLink>, show_close_hint: bool) {
        self.state = Some(LoginDialogState::Info {
            message: message.to_string(),
            links,
            show_close_hint,
        });
        self.submitted = None;
    }

    /// `showWaiting` (login-dialog.ts:207-212).
    pub fn show_waiting(&mut self, message: &str) {
        self.state = Some(LoginDialogState::Waiting {
            message: message.to_string(),
        });
        self.submitted = None;
    }

    /// `showProgress` (login-dialog.ts:217-220).
    pub fn show_progress(&mut self, message: &str) {
        self.state = Some(LoginDialogState::Progress {
            message: message.to_string(),
        });
        self.submitted = None;
    }

    /// `cancel` (login-dialog.ts:83-91): rejects the pending input (here:
    /// clears it) and notifies `on_cancel`.
    fn cancel(&mut self) {
        self.submitted = None;
        if let Some(on_cancel) = self.on_cancel.as_mut() {
            on_cancel();
        }
    }

    /// Whether the input is currently on screen and accepting text.
    fn input_active(&self) -> bool {
        self.submitted.is_none()
            && matches!(
                self.state,
                Some(LoginDialogState::ManualInput { .. }) | Some(LoginDialogState::Prompt { .. })
            )
    }

    /// Select-mode navigation (upstream `ExtensionSelectorComponent` arrows,
    /// extension-selector.ts:247-252).
    fn select_move(&mut self, delta: isize) {
        if let Some(LoginDialogState::Select {
            options, selected, ..
        }) = self.state.as_mut()
        {
            if options.is_empty() {
                return;
            }
            let len = options.len() as isize;
            *selected = ((*selected as isize + delta).rem_euclid(len)) as usize;
        }
    }

    /// The input line, or the `> value` echo after a submit
    /// (login-dialog.ts:77-81).
    fn push_input_or_submitted(&self, lines: &mut Vec<String>, width: usize) {
        if let Some(submitted) = &self.submitted {
            lines.extend(Text::new(format!("> {submitted}"), 0, 0, None).render(width));
        } else {
            lines.extend(self.input.render(width));
        }
    }

    fn content_lines(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        match &self.state {
            None => {}
            Some(LoginDialogState::Auth { url, instructions }) => {
                lines.extend(Spacer::new(1).render(width));
                let linked_url = osc8_link(url, url);
                lines.extend(
                    Text::new(self.theme.fg("accent", &linked_url), 1, 0, None).render(width),
                );
                let hyperlink = osc8_link(url, click_hint());
                lines.extend(Text::new(self.theme.fg("dim", &hyperlink), 1, 0, None).render(width));
                if let Some(instructions) = instructions {
                    lines.extend(Spacer::new(1).render(width));
                    lines.extend(
                        Text::new(self.theme.fg("warning", instructions), 1, 0, None).render(width),
                    );
                }
            }
            Some(LoginDialogState::DeviceCode {
                user_code,
                verification_uri,
                waiting,
            }) => {
                lines.extend(Spacer::new(1).render(width));
                let linked_url = osc8_link(verification_uri, verification_uri);
                lines.extend(
                    Text::new(self.theme.fg("accent", &linked_url), 1, 0, None).render(width),
                );
                let hyperlink = osc8_link(verification_uri, click_hint());
                lines.extend(Text::new(self.theme.fg("dim", &hyperlink), 1, 0, None).render(width));
                lines.extend(Spacer::new(1).render(width));
                lines.extend(
                    Text::new(
                        self.theme
                            .fg("warning", &format!("Enter code: {user_code}")),
                        1,
                        0,
                        None,
                    )
                    .render(width),
                );
                if let Some(waiting) = waiting {
                    lines.extend(Spacer::new(1).render(width));
                    lines
                        .extend(Text::new(self.theme.fg("dim", waiting), 1, 0, None).render(width));
                    lines.extend(
                        Text::new(
                            format!(
                                "({})",
                                key_hint(&self.theme, "tui.select.cancel", "to cancel")
                            ),
                            1,
                            0,
                            None,
                        )
                        .render(width),
                    );
                }
            }
            Some(LoginDialogState::ManualInput { prompt }) => {
                lines.extend(Spacer::new(1).render(width));
                lines.extend(Text::new(self.theme.fg("dim", prompt), 1, 0, None).render(width));
                self.push_input_or_submitted(&mut lines, width);
                lines.extend(
                    Text::new(
                        format!(
                            "({})",
                            key_hint(&self.theme, "tui.select.cancel", "to cancel")
                        ),
                        1,
                        0,
                        None,
                    )
                    .render(width),
                );
            }
            Some(LoginDialogState::Prompt {
                message,
                placeholder,
            }) => {
                lines.extend(Spacer::new(1).render(width));
                lines.extend(Text::new(self.theme.fg("text", message), 1, 0, None).render(width));
                if let Some(placeholder) = placeholder {
                    lines.extend(
                        Text::new(
                            self.theme.fg("dim", &format!("e.g., {placeholder}")),
                            1,
                            0,
                            None,
                        )
                        .render(width),
                    );
                }
                self.push_input_or_submitted(&mut lines, width);
                lines.extend(
                    Text::new(
                        format!(
                            "({}{})",
                            key_hint(&self.theme, "tui.select.cancel", "to cancel,"),
                            key_hint(&self.theme, "tui.select.confirm", "to submit"),
                        ),
                        1,
                        0,
                        None,
                    )
                    .render(width),
                );
            }
            Some(LoginDialogState::Select {
                message,
                options,
                selected,
            }) => {
                lines.extend(Spacer::new(1).render(width));
                lines.extend(Text::new(self.theme.fg("text", message), 1, 0, None).render(width));
                for (index, option) in options.iter().enumerate() {
                    let label = if index == *selected {
                        format!("→ {}", self.theme.fg("accent", &option.label))
                    } else {
                        format!("  {}", self.theme.fg("text", &option.label))
                    };
                    lines.extend(Text::new(label, 1, 0, None).render(width));
                }
                lines.extend(
                    Text::new(
                        format!(
                            "({}{})",
                            key_hint(&self.theme, "tui.select.cancel", "to cancel,"),
                            key_hint(&self.theme, "tui.select.confirm", "to select"),
                        ),
                        1,
                        0,
                        None,
                    )
                    .render(width),
                );
            }
            Some(LoginDialogState::Info {
                message,
                links,
                show_close_hint,
            }) => {
                lines.extend(Spacer::new(1).render(width));
                lines.extend(Text::new(self.theme.fg("text", message), 1, 0, None).render(width));
                for link in links {
                    let text = match &link.label {
                        Some(label) => format!("{label}: {}", link.url),
                        None => link.url.clone(),
                    };
                    let hyperlink = osc8_link(&link.url, &text);
                    lines.extend(
                        Text::new(self.theme.fg("accent", &hyperlink), 1, 0, None).render(width),
                    );
                }
                if *show_close_hint {
                    lines.extend(Spacer::new(1).render(width));
                    lines.extend(
                        Text::new(
                            format!(
                                "({})",
                                key_hint(&self.theme, "tui.select.cancel", "to close")
                            ),
                            1,
                            0,
                            None,
                        )
                        .render(width),
                    );
                }
            }
            Some(LoginDialogState::Waiting { message }) => {
                lines.extend(Spacer::new(1).render(width));
                lines.extend(Text::new(self.theme.fg("dim", message), 1, 0, None).render(width));
                lines.extend(
                    Text::new(
                        format!(
                            "({})",
                            key_hint(&self.theme, "tui.select.cancel", "to cancel")
                        ),
                        1,
                        0,
                        None,
                    )
                    .render(width),
                );
            }
            Some(LoginDialogState::Progress { message }) => {
                lines.extend(Text::new(self.theme.fg("dim", message), 1, 0, None).render(width));
            }
            Some(LoginDialogState::Details(detail_lines)) => {
                lines.extend(Spacer::new(1).render(width));
                for line in detail_lines {
                    lines.extend(Text::new(line.clone(), 1, 0, None).render(width));
                }
            }
        }
        lines
    }
}

impl Component for LoginDialogComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        lines.extend(self.top_border.render(width));
        // Title (login-dialog.ts:48).
        let title = self.theme.fg("accent", &Theme::bold(&self.title));
        lines.extend(Text::new(title, 1, 0, None).render(width));
        lines.extend(self.content_lines(width));
        lines.extend(self.bottom_border.render(width));
        lines
    }

    fn handle_input(&mut self, data: &str) {
        let kb = get_keybindings();
        let kb = kb.read().unwrap_or_else(|poisoned| poisoned.into_inner());

        // Escape / Ctrl+C cancels (login-dialog.ts:225-228).
        if kb.matches_id(data, "tui.select.cancel") {
            self.cancel();
            return;
        }

        // Select mode navigates with up/down and confirms with Enter
        // (upstream `showAuthSelect` selector, interactive-mode.ts:5216-5228).
        if matches!(self.state, Some(LoginDialogState::Select { .. })) {
            if kb.matches_id(data, "tui.select.up") {
                self.select_move(-1);
            } else if kb.matches_id(data, "tui.select.down") {
                self.select_move(1);
            } else if kb.matches_id(data, "tui.select.confirm") || data == "\n" {
                let id = match &self.state {
                    Some(LoginDialogState::Select {
                        options, selected, ..
                    }) => options.get(*selected).map(|option| option.id.clone()),
                    _ => None,
                };
                if let Some(id) = id {
                    if let Some(on_select) = self.on_select.as_mut() {
                        on_select(&id);
                    }
                }
            }
            return;
        }

        if !self.input_active() {
            return;
        }

        // Enter submits the pending prompt (upstream `input.onSubmit`,
        // login-dialog.ts:56-64).
        if kb.matches_id(data, "tui.select.confirm") || data == "\n" {
            let value = self.input.get_value().to_string();
            self.submitted = Some(value.clone());
            match &self.state {
                Some(LoginDialogState::ManualInput { .. }) => {
                    if let Some(on_submit_manual) = self.on_submit_manual.as_mut() {
                        on_submit_manual(&value);
                    }
                }
                Some(LoginDialogState::Prompt { .. }) => {
                    if let Some(on_prompt_confirm) = self.on_prompt_confirm.as_mut() {
                        on_prompt_confirm(&value);
                    }
                }
                _ => {}
            }
            return;
        }

        // Pass to input (login-dialog.ts:230-231). Drop the keybinding read
        // guard first: `Input::handle_input` takes its own read lock on the
        // same global (std RwLock is not reentrant against a queued writer).
        drop(kb);
        self.input.handle_input(data);
    }
}

impl Focusable for LoginDialogComponent {
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.input.set_focused(focused);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::themes::load_theme;
    use crate::modes::interactive::interactive_mode::install_global_keybindings;
    use pir_tui::utils::visible_width;
    use std::sync::Mutex;

    fn theme() -> Arc<Theme> {
        Arc::new(load_theme("dark", None).expect("builtin dark theme"))
    }

    fn dialog() -> LoginDialogComponent {
        LoginDialogComponent::new(theme(), "anthropic", Some("Anthropic"), None)
    }

    #[test]
    fn manual_input_submit_fires_on_submit_manual_and_echoes() {
        install_global_keybindings();
        let submitted: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let submitted_cb = Arc::clone(&submitted);
        let mut component = dialog();
        component.on_submit_manual = Some(Box::new(move |value| {
            submitted_cb.lock().unwrap().push(value.to_string())
        }));

        component.show_manual_input("Enter code:");
        for ch in "123456".chars() {
            component.handle_input(&ch.to_string());
        }
        component.handle_input("\r");

        assert_eq!(*submitted.lock().unwrap(), vec!["123456"]);
        let rendered = component.render(60).join("\n");
        assert!(rendered.contains("Enter code:"));
        assert!(rendered.contains("> 123456"));
        assert!(rendered.contains("to cancel"));
    }

    #[test]
    fn prompt_submit_fires_on_prompt_confirm() {
        install_global_keybindings();
        let confirmed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let confirmed_cb = Arc::clone(&confirmed);
        let mut component = dialog();
        component.on_prompt_confirm = Some(Box::new(move |value| {
            confirmed_cb.lock().unwrap().push(value.to_string())
        }));

        component.show_prompt("Enter your API key", Some("sk-..."));
        for ch in "sk-test".chars() {
            component.handle_input(&ch.to_string());
        }
        component.handle_input("\r");

        assert_eq!(*confirmed.lock().unwrap(), vec!["sk-test"]);
        let rendered = component.render(60).join("\n");
        assert!(rendered.contains("Enter your API key"));
        assert!(rendered.contains("e.g., sk-..."));
        assert!(rendered.contains("> sk-test"));
        assert!(rendered.contains("to submit"));
    }

    #[test]
    fn escape_cancels_from_any_mode() {
        install_global_keybindings();
        let cancelled = Arc::new(Mutex::new(0));
        let cancelled_cb = Arc::clone(&cancelled);
        let mut component = dialog();
        component.on_cancel = Some(Box::new(move || *cancelled_cb.lock().unwrap() += 1));

        component.show_waiting("Waiting for authentication...");
        component.handle_input("\x1b");
        assert_eq!(*cancelled.lock().unwrap(), 1);
        // Ctrl+C matches the same id.
        component.handle_input("\x03");
        assert_eq!(*cancelled.lock().unwrap(), 2);
    }

    #[test]
    fn cancel_wins_over_pending_input() {
        install_global_keybindings();
        let cancelled = Arc::new(Mutex::new(0));
        let submitted: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let cancelled_cb = Arc::clone(&cancelled);
        let submitted_cb = Arc::clone(&submitted);
        let mut component = dialog();
        component.on_cancel = Some(Box::new(move || *cancelled_cb.lock().unwrap() += 1));
        component.on_submit_manual = Some(Box::new(move |value| {
            submitted_cb.lock().unwrap().push(value.to_string())
        }));

        component.show_manual_input("Enter code:");
        component.handle_input("a");
        component.handle_input("\x1b");

        assert_eq!(*cancelled.lock().unwrap(), 1);
        assert!(submitted.lock().unwrap().is_empty());
    }

    #[test]
    fn show_auth_renders_hyperlinks_and_fires_begin_oauth() {
        install_global_keybindings();
        let began = Arc::new(Mutex::new(0));
        let began_cb = Arc::clone(&began);
        let mut component = dialog();
        component.on_begin_oauth = Some(Box::new(move || *began_cb.lock().unwrap() += 1));

        component.show_auth(
            "https://example.com/auth",
            Some("Open the link and confirm."),
        );
        assert_eq!(*began.lock().unwrap(), 1);

        let rendered = component.render(80).join("\n");
        assert!(rendered.contains("\x1b]8;;https://example.com/auth\x07"));
        assert!(rendered.contains("https://example.com/auth\x1b]8;;\x07"));
        assert!(rendered.contains(if cfg!(target_os = "macos") {
            "Cmd+click to open"
        } else {
            "Ctrl+click to open"
        }));
        assert!(rendered.contains("Open the link and confirm."));
    }

    #[test]
    fn show_device_code_renders_uri_and_code() {
        install_global_keybindings();
        let began = Arc::new(Mutex::new(0));
        let began_cb = Arc::clone(&began);
        let mut component = dialog();
        component.on_begin_device_code = Some(Box::new(move || *began_cb.lock().unwrap() += 1));

        component.show_device_code("ABC-123", "https://example.com/device", None);
        assert_eq!(*began.lock().unwrap(), 1);

        let rendered = component.render(80).join("\n");
        assert!(rendered.contains("https://example.com/device"));
        assert!(rendered.contains("Enter code: ABC-123"));
        // Without a waiting message no cancel hint is shown below the code.
        assert!(!rendered.contains("to cancel"));

        // The OAuth integration layer folds the `showWaiting` line into the
        // device-code view (interactive-mode.ts:5264-5266).
        component.show_device_code(
            "ABC-123",
            "https://example.com/device",
            Some("Waiting for authentication..."),
        );
        let rendered = component.render(80).join("\n");
        assert!(rendered.contains("Waiting for authentication..."));
        assert!(rendered.contains("Enter code: ABC-123"));
        assert!(rendered.contains("to cancel"));
    }

    #[test]
    fn select_prompt_navigates_and_fires_on_select() {
        install_global_keybindings();
        let selected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let selected_cb = Arc::clone(&selected);
        let mut component = dialog();
        component.on_select = Some(Box::new(move |id| {
            selected_cb.lock().unwrap().push(id.to_string())
        }));

        component.show_select(
            "Choose an account:",
            vec![
                SelectOption {
                    id: "acct-1".to_string(),
                    label: "Account one".to_string(),
                    description: None,
                },
                SelectOption {
                    id: "acct-2".to_string(),
                    label: "Account two".to_string(),
                    description: None,
                },
            ],
        );

        // Enter selects the pre-selected first option.
        component.handle_input("\r");
        assert_eq!(*selected.lock().unwrap(), vec!["acct-1"]);
        let rendered = component.render(60).join("\n");
        assert!(rendered.contains("Choose an account:"));
        assert!(rendered.contains("Account one"));
        assert!(rendered.contains("Account two"));
        assert!(rendered.contains("→"));
        assert!(rendered.contains("to select"));

        // Down + Enter selects the second option.
        component.handle_input("\x1b[B");
        component.handle_input("\r");
        assert_eq!(*selected.lock().unwrap(), vec!["acct-1", "acct-2"]);

        // Escape cancels instead of selecting.
        component.handle_input("\x1b");
        assert_eq!(*selected.lock().unwrap(), vec!["acct-1", "acct-2"]);
    }

    #[test]
    fn all_modes_render_within_width_and_title() {
        install_global_keybindings();
        let mut component = dialog();
        component.show_info(
            "Authentication is configured outside pi.",
            vec![AuthInfoLink {
                url: "https://example.com/docs".to_string(),
                label: Some("Docs".to_string()),
            }],
            true,
        );
        for width in [10usize, 40, 100] {
            for line in component.render(width) {
                assert!(
                    visible_width(&line) <= width,
                    "line {line:?} exceeds width {width}"
                );
            }
        }
        assert!(component
            .render(40)
            .join("\n")
            .contains("Login to Anthropic"));

        // Progress and Details modes.
        component.show_progress("Waiting for browser...");
        assert!(component
            .render(40)
            .join("\n")
            .contains("Waiting for browser..."));
        component.show_details(vec!["line one".to_string(), "line two".to_string()]);
        let rendered = component.render(40).join("\n");
        assert!(rendered.contains("line one"));
        assert!(rendered.contains("line two"));
    }

    #[test]
    fn show_manual_input_clears_previous_buffered_input() {
        install_global_keybindings();
        let submitted: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let submitted_cb = Arc::clone(&submitted);
        let mut component = dialog();
        component.on_submit_manual = Some(Box::new(move |value| {
            submitted_cb.lock().unwrap().push(value.to_string())
        }));

        // Keys typed while no input is on screen are dropped.
        component.show_waiting("waiting");
        component.handle_input("x");
        component.show_manual_input("Enter code:");
        component.handle_input("\r");
        assert_eq!(*submitted.lock().unwrap(), vec![""]);
    }
}
