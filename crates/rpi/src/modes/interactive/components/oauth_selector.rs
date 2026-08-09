//! Port of `oauth-selector.ts` @ pi 0.82.1 (2efa728) — T12-S5a.
//!
//! Intentional differences:
//! - The theme is injected (`Arc<Theme>`) instead of read from the global
//!   `theme` getter (theme.ts:799-816); the [`DynamicBorder`] color fn is
//!   explicit (dynamic-border.ts:14).
//! - `AuthSelectorProvider.method` (`ApiKeyAuth | OAuthAuth`) is represented
//!   by the display name only (`method_name`); the trait objects are only
//!   used for `method?.name` in the fuzzy search text (oauth-selector.ts:107).
//! - [`AuthSelectorProvider::from_provider`] mirrors upstream
//!   `getLoginProviderOptions` (interactive-mode.ts:4845-4875): one entry per
//!   configured auth method (a provider offering both methods appears twice)
//!   and the entries are sorted by name (byte order; upstream
//!   `localeCompare`).
//! - `on_select` receives the full selected [`AuthSelectorProvider`] (the
//!   row carries `auth_type` / `method_name`); upstream passes
//!   `(providerId, authType)`, oauth-selector.ts:202. The T13 login wiring
//!   uses the row to pick the login flow (api-key dialog vs ambient info vs
//!   the OAuth stub).
//! - `status` is injected per row; the component never queries the runtime
//!   (upstream reads `getProviderAuthStatus` while building the list,
//!   interactive-mode.ts:4848-4853). Statuses come from `check_auth` /
//!   `listCredentials` in the integration layer (T13).
//! - Rows are re-derived in `render` instead of being rebuilt as child
//!   components (upstream `updateList`, oauth-selector.ts:114-162).
//! - Escape/Ctrl+C cancels directly (oauth-selector.ts:205-207). The T12-S5a
//!   task brief's generic rule "Esc: clear a non-empty search first,
//!   otherwise on_cancel" does
//!   not match this file — that behavior belongs to scoped-models-selector.ts
//!   (scoped-models-selector.ts:350-359), a different component. Kept
//!   upstream-faithful; revisit if the integration layer wants it.

use std::sync::Arc;

use rpi_ai::auth::types::{AuthCheck, AuthType};
use rpi_ai::models::Provider;
use rpi_tui::components::input::Input;
use rpi_tui::components::spacer::Spacer;
use rpi_tui::components::truncated_text::TruncatedText;
use rpi_tui::fuzzy::fuzzy_filter;
use rpi_tui::keybindings::get_keybindings;
use rpi_tui::tui::{Component, Focusable};

use crate::core::themes::Theme;
use crate::modes::interactive::components::dynamic_border::DynamicBorder;

/// `AuthSelectorProvider` (oauth-selector.ts:14-20): one selectable row.
/// `method` is collapsed to its display name plus the login capability.
#[derive(Debug, Clone)]
pub struct AuthSelectorProvider {
    pub id: String,
    pub name: String,
    pub auth_type: AuthType,
    pub method_name: Option<String>,
    /// Whether the auth method exposes an interactive `login` (upstream
    /// `method?.login`, interactive-mode.ts:4928). OAuth methods always have
    /// one upstream; ambient-only api-key methods do not.
    pub method_login: bool,
    pub status: Option<AuthCheck>,
}

impl AuthSelectorProvider {
    /// Mirror of upstream `getLoginProviderOptions`
    /// (interactive-mode.ts:4845-4875): one entry per configured auth method
    /// (`oauth` first, then `api_key`), sorted by name. `status` is whatever
    /// the caller already resolved (e.g. `ModelRuntime::check_auth`).
    pub fn from_provider(provider: &Arc<dyn Provider>, status: Option<AuthCheck>) -> Vec<Self> {
        let mut options = Vec::new();
        let auth = provider.auth();
        if auth.oauth.is_some() {
            options.push(Self {
                id: provider.id().to_string(),
                name: provider.name().to_string(),
                auth_type: AuthType::Oauth,
                method_name: auth.oauth.as_ref().map(|method| method.name().to_string()),
                method_login: true,
                status: status.clone(),
            });
        }
        if auth.api_key.is_some() {
            options.push(Self {
                id: provider.id().to_string(),
                name: provider.name().to_string(),
                auth_type: AuthType::ApiKey,
                method_name: auth
                    .api_key
                    .as_ref()
                    .map(|method| method.name().to_string()),
                method_login: auth
                    .api_key
                    .as_ref()
                    .is_some_and(|method| method.supports_login()),
                status,
            });
        }
        options.sort_by(|a, b| a.name.cmp(&b.name));
        options
    }
}

/// `"login" | "logout"` mode (oauth-selector.ts:46).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthSelectorMode {
    Login,
    Logout,
}

impl AuthSelectorMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthSelectorMode::Login => "login",
            AuthSelectorMode::Logout => "logout",
        }
    }

    /// Selector title (oauth-selector.ts:72).
    fn title(self) -> &'static str {
        match self {
            AuthSelectorMode::Login => "Select provider to configure:",
            AuthSelectorMode::Logout => "Select provider to logout:",
        }
    }
}

/// `formatAuthSelectorProviderType` (oauth-selector.ts:22-24).
fn format_auth_selector_provider_type(auth_type: &AuthType) -> &'static str {
    match auth_type {
        AuthType::Oauth => "subscription",
        AuthType::ApiKey => "API key",
    }
}

/// Wire-format auth type label (`provider.authType`).
fn auth_type_str(auth_type: &AuthType) -> &'static str {
    match auth_type {
        AuthType::Oauth => "oauth",
        AuthType::ApiKey => "api_key",
    }
}

/// `isEnvSource` — upstream inline regex
/// `/^[A-Z][A-Z0-9_]*(?:, [A-Z][A-Z0-9_]*)*$/` (oauth-selector.ts:177).
fn is_env_source(source: &str) -> bool {
    source.split(", ").all(is_env_token) && !source.is_empty()
}

/// `[A-Z][A-Z0-9_]*` token.
fn is_env_token(token: &str) -> bool {
    let mut chars = token.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_uppercase())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Component that renders an auth provider selector
/// (oauth-selector.ts:29-214).
pub struct OAuthSelectorComponent {
    theme: Arc<Theme>,
    mode: AuthSelectorMode,
    search_input: Input,
    all_providers: Vec<AuthSelectorProvider>,
    filtered_providers: Vec<AuthSelectorProvider>,
    selected_index: usize,
    show_auth_type_labels: bool,
    top_border: DynamicBorder,
    bottom_border: DynamicBorder,

    // Focusable implementation - propagate to search input for IME cursor
    // positioning (oauth-selector.ts:33-40).
    focused: bool,

    /// `onSelectCallback` — called with the selected provider row. The row
    /// carries `auth_type` / `method_name`, which the T13 login wiring needs
    /// to pick the flow (upstream passes `(providerId, authType)`,
    /// oauth-selector.ts:202).
    on_select: Box<dyn FnMut(&AuthSelectorProvider) + Send>,
    /// `onCancelCallback`.
    on_cancel: Box<dyn FnMut() + Send>,
}

impl OAuthSelectorComponent {
    /// `constructor` (oauth-selector.ts:51-100).
    pub fn new(
        theme: Arc<Theme>,
        mode: AuthSelectorMode,
        providers: Vec<AuthSelectorProvider>,
        on_select: Box<dyn FnMut(&AuthSelectorProvider) + Send>,
        on_cancel: Box<dyn FnMut() + Send>,
        initial_search_input: Option<String>,
    ) -> Self {
        // `showAuthTypeLabels`: more than one distinct auth type
        // (oauth-selector.ts:63).
        let mut distinct_auth_types = Vec::new();
        for provider in &providers {
            if !distinct_auth_types.contains(&provider.auth_type) {
                distinct_auth_types.push(provider.auth_type);
            }
        }

        let mut search_input = Input::new();
        if let Some(initial) = &initial_search_input {
            search_input.set_value(initial);
        }
        // Upstream sets `searchInput.onSubmit` (oauth-selector.ts:80-85), but
        // `handleInput` consumes `tui.select.confirm` before it ever reaches
        // the input, so the port leaves it unset; confirm is handled in
        // [`Component::handle_input`].

        let filtered_providers = providers.clone();
        let top_border = DynamicBorder::new(border_color(&theme));
        let bottom_border = DynamicBorder::new(border_color(&theme));
        let mut component = Self {
            theme,
            mode,
            search_input,
            all_providers: providers,
            filtered_providers,
            selected_index: 0,
            show_auth_type_labels: distinct_auth_types.len() > 1,
            top_border,
            bottom_border,
            focused: false,
            on_select,
            on_cancel,
        };
        component.filter_providers(initial_search_input.as_deref().unwrap_or(""));
        component
    }

    /// `filterProviders` (oauth-selector.ts:102-112): fuzzy search over
    /// `name id authType methodName`.
    fn filter_providers(&mut self, query: &str) {
        self.filtered_providers = if query.is_empty() {
            self.all_providers.clone()
        } else {
            fuzzy_filter(self.all_providers.clone(), query, |provider| {
                format!(
                    "{} {} {} {}",
                    provider.name,
                    provider.id,
                    auth_type_str(&provider.auth_type),
                    provider.method_name.as_deref().unwrap_or(""),
                )
            })
        };
        self.selected_index = self
            .selected_index
            .min(self.filtered_providers.len().saturating_sub(1));
    }

    /// `updateList` (oauth-selector.ts:114-162): rows for the visible window
    /// plus the scroll indicator / empty message.
    fn list_rows(&self) -> Vec<String> {
        let max_visible = 8;
        let len = self.filtered_providers.len();
        let mut rows = Vec::new();

        if len == 0 {
            let message = if self.all_providers.is_empty() {
                match self.mode {
                    AuthSelectorMode::Login => "No providers available",
                    AuthSelectorMode::Logout => "No providers logged in. Use /login first.",
                }
            } else {
                "No matching providers"
            };
            rows.push(self.theme.fg("muted", &format!("  {message}")));
            return rows;
        }

        let start_index = self
            .selected_index
            .saturating_sub(max_visible / 2)
            .min(len.saturating_sub(max_visible));
        let end_index = (start_index + max_visible).min(len);

        for (index, provider) in self.filtered_providers[start_index..end_index]
            .iter()
            .enumerate()
        {
            rows.push(self.render_provider_row(provider, start_index + index));
        }

        if start_index > 0 || end_index < len {
            rows.push(
                self.theme
                    .fg("muted", &format!("  ({}/{})", self.selected_index + 1, len)),
            );
        }

        rows
    }

    /// One provider row (oauth-selector.ts:128-145).
    fn render_provider_row(&self, provider: &AuthSelectorProvider, index: usize) -> String {
        let is_selected = index == self.selected_index;
        let status_indicator = self.format_status_indicator(provider);
        let auth_type_label = if self.show_auth_type_labels {
            self.theme.fg(
                "muted",
                &format!(
                    " [{}]",
                    format_auth_selector_provider_type(&provider.auth_type)
                ),
            )
        } else {
            String::new()
        };
        if is_selected {
            let prefix = self.theme.fg("accent", "→ ");
            let text = self.theme.fg("accent", &provider.name);
            format!("{prefix}{text}{auth_type_label}{status_indicator}")
        } else {
            let text = format!("  {}", self.theme.fg("text", &provider.name));
            format!("{text}{auth_type_label}{status_indicator}")
        }
    }

    /// `formatStatusIndicator` (oauth-selector.ts:164-181).
    fn format_status_indicator(&self, provider: &AuthSelectorProvider) -> String {
        let Some(status) = &provider.status else {
            return self.theme.fg("muted", " • unconfigured");
        };
        if status.kind != provider.auth_type {
            let label = if status.kind == AuthType::Oauth {
                "subscription configured"
            } else {
                "API key configured"
            };
            return format!(
                "{}{}",
                self.theme.fg("muted", " • "),
                self.theme.fg("warning", label)
            );
        }
        let Some(source) = &status.source else {
            return self.theme.fg("success", " ✓ configured");
        };
        if source == "OAuth" || source == "stored credential" {
            return self.theme.fg("success", " ✓ configured");
        }
        let display = if is_env_source(source) {
            format!("env: {source}")
        } else {
            source.clone()
        };
        self.theme.fg("success", &format!(" ✓ {display}"))
    }
}

/// `(text) => theme.fg("border", text)` (dynamic-border.ts:14).
fn border_color(theme: &Arc<Theme>) -> Box<dyn Fn(&str) -> String + Send + Sync> {
    let theme = theme.clone();
    Box::new(move |text| theme.fg("border", text))
}

impl Component for OAuthSelectorComponent {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        lines.extend(self.top_border.render(width));
        lines.extend(Spacer::new(1).render(width));
        // Title (oauth-selector.ts:72-73).
        let title = self.theme.fg("accent", &Theme::bold(self.mode.title()));
        lines.extend(TruncatedText::new(title, 1, 0).render(width));
        lines.extend(Spacer::new(1).render(width));
        lines.extend(self.search_input.render(width));
        lines.extend(Spacer::new(1).render(width));
        for row in self.list_rows() {
            lines.extend(TruncatedText::new(row, 1, 0).render(width));
        }
        lines.extend(Spacer::new(1).render(width));
        lines.extend(self.bottom_border.render(width));
        lines
    }

    fn handle_input(&mut self, data: &str) {
        let kb = get_keybindings();
        let kb = kb.read().unwrap_or_else(|poisoned| poisoned.into_inner());

        // Up arrow (oauth-selector.ts:186-190)
        if kb.matches_id(data, "tui.select.up") {
            if self.filtered_providers.is_empty() {
                return;
            }
            self.selected_index = self.selected_index.saturating_sub(1);
        }
        // Down arrow (oauth-selector.ts:192-196)
        else if kb.matches_id(data, "tui.select.down") {
            if self.filtered_providers.is_empty() {
                return;
            }
            self.selected_index = (self.selected_index + 1).min(self.filtered_providers.len() - 1);
        }
        // Enter (oauth-selector.ts:198-203)
        else if kb.matches_id(data, "tui.select.confirm") {
            if let Some(selected) = self.filtered_providers.get(self.selected_index) {
                let selected = selected.clone();
                (self.on_select)(&selected);
            }
        }
        // Escape or Ctrl+C (oauth-selector.ts:205-207)
        else if kb.matches_id(data, "tui.select.cancel") {
            (self.on_cancel)();
        }
        // Pass everything else to search input (oauth-selector.ts:209-212)
        else {
            drop(kb);
            self.search_input.handle_input(data);
            let query = self.search_input.get_value().to_string();
            self.filter_providers(&query);
        }
    }
}

impl Focusable for OAuthSelectorComponent {
    fn focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        self.search_input.set_focused(focused);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::themes::load_theme;
    use crate::modes::interactive::interactive_mode::install_global_keybindings;
    use rpi_tui::keybindings::get_keybindings;
    use std::sync::Mutex;

    fn theme() -> Arc<Theme> {
        Arc::new(load_theme("dark", None).expect("builtin dark theme"))
    }

    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    fn provider(
        id: &str,
        name: &str,
        auth_type: AuthType,
        status: Option<AuthCheck>,
    ) -> AuthSelectorProvider {
        AuthSelectorProvider {
            id: id.to_string(),
            name: name.to_string(),
            auth_type,
            method_name: None,
            method_login: auth_type == AuthType::Oauth,
            status,
        }
    }

    #[allow(clippy::too_many_arguments)] // mirrors the upstream constructor
    #[allow(clippy::type_complexity)] // mirrors the upstream callback type
    fn setup() -> (
        OAuthSelectorComponent,
        Arc<Mutex<Vec<String>>>,
        Arc<Mutex<usize>>,
    ) {
        install_global_keybindings();
        let selected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let cancelled: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let selected_cb = Arc::clone(&selected);
        let cancelled_cb = Arc::clone(&cancelled);
        let component = OAuthSelectorComponent::new(
            theme(),
            AuthSelectorMode::Login,
            vec![
                provider("anthropic", "Anthropic", AuthType::Oauth, None),
                provider("openai", "OpenAI", AuthType::ApiKey, None),
            ],
            Box::new(move |provider| selected_cb.lock().unwrap().push(provider.id.clone())),
            Box::new(move || *cancelled_cb.lock().unwrap() += 1),
            None,
        );
        (component, selected, cancelled)
    }

    /// Whether the global keybindings match `data` for `tui.select.*` ids
    /// (test helper, avoids hardcoding the raw sequences twice).
    fn matches(data: &str, id: &str) -> bool {
        let kb = get_keybindings();
        kb.read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .matches_id(data, id)
    }

    #[test]
    fn enter_confirms_the_selected_provider() {
        let (mut component, selected, _cancelled) = setup();
        component.handle_input("\r");
        assert_eq!(*selected.lock().unwrap(), vec!["anthropic"]);
    }

    #[test]
    fn escape_cancels() {
        let (mut component, _selected, cancelled) = setup();
        component.handle_input("\x1b");
        assert_eq!(*cancelled.lock().unwrap(), 1);
        // Ctrl+C matches the same id (keybindings.ts: tui.select.cancel).
        assert!(matches("\x03", "tui.select.cancel"));
    }

    #[test]
    fn up_and_down_move_selection() {
        let (mut component, selected, _cancelled) = setup();
        component.handle_input("\x1b[B"); // down
        component.handle_input("\r");
        assert_eq!(*selected.lock().unwrap(), vec!["openai"]);
        component.handle_input("\x1b[A"); // up
        component.handle_input("\r");
        assert_eq!(*selected.lock().unwrap(), vec!["openai", "anthropic"]);
    }

    #[test]
    fn typing_filters_via_fuzzy_search() {
        let (mut component, selected, _cancelled) = setup();
        // Type into the search input; "open" fuzzy-matches OpenAI's name.
        for ch in "open".chars() {
            component.handle_input(&ch.to_string());
        }
        component.handle_input("\r");
        assert_eq!(*selected.lock().unwrap(), vec!["openai"]);
    }

    #[test]
    fn render_shows_badges_scroll_and_empty_messages() {
        let (component, _selected, _cancelled) = setup();
        let lines = component.render(60);
        let joined = lines.join("\n");
        assert!(joined.contains("Select provider to configure:"));
        assert!(joined.contains("[subscription]"));
        assert!(joined.contains("[API key]"));
        assert!(joined.contains("Anthropic"));
        assert!(joined.contains("OpenAI"));
        // Both rows fit, so no scroll indicator.
        assert!(!joined.contains("(1/2)"));

        // Empty provider list -> login message.
        let empty = OAuthSelectorComponent::new(
            theme(),
            AuthSelectorMode::Login,
            vec![],
            Box::new(|_| {}),
            Box::new(|| {}),
            None,
        );
        assert!(empty
            .render(60)
            .join("\n")
            .contains("No providers available"));

        // Logout mode with no providers.
        let empty_logout = OAuthSelectorComponent::new(
            theme(),
            AuthSelectorMode::Logout,
            vec![],
            Box::new(|_| {}),
            Box::new(|| {}),
            None,
        );
        assert!(empty_logout
            .render(60)
            .join("\n")
            .contains("No providers logged in. Use /login first."));

        // Scroll indicator appears once rows exceed the 8-row window.
        let many: Vec<AuthSelectorProvider> = (0..12)
            .map(|i| {
                provider(
                    &format!("p{i}"),
                    &format!("Provider {i}"),
                    AuthType::ApiKey,
                    None,
                )
            })
            .collect();
        let scrolled = OAuthSelectorComponent::new(
            theme(),
            AuthSelectorMode::Login,
            many,
            Box::new(|_| {}),
            Box::new(|| {}),
            None,
        );
        let scrolled_lines = scrolled.render(60).join("\n");
        assert!(scrolled_lines.contains("(1/12)"));
    }

    #[test]
    fn status_indicator_formats_sources() {
        let component = OAuthSelectorComponent::new(
            theme(),
            AuthSelectorMode::Login,
            vec![],
            Box::new(|_| {}),
            Box::new(|| {}),
            None,
        );
        let ok = |source: Option<&str>| AuthSelectorProvider {
            id: "x".into(),
            name: "X".into(),
            auth_type: AuthType::ApiKey,
            method_name: None,
            method_login: false,
            status: Some(AuthCheck {
                kind: AuthType::ApiKey,
                source: source.map(str::to_string),
            }),
        };
        assert!(component
            .format_status_indicator(&ok(None))
            .contains("✓ configured"));
        assert!(component
            .format_status_indicator(&ok(Some("stored credential")))
            .contains("✓ configured"));
        let env = component.format_status_indicator(&ok(Some("ANTHROPIC_API_KEY, FOO")));
        assert!(env.contains("✓ env: ANTHROPIC_API_KEY, FOO"));
        // Non-env source falls back to the raw label.
        let other = component.format_status_indicator(&ok(Some("custom path")));
        assert!(other.contains("✓ custom path"));
        // Mismatched kind -> warning label.
        let mismatched = AuthSelectorProvider {
            id: "y".into(),
            name: "Y".into(),
            auth_type: AuthType::Oauth,
            method_name: None,
            method_login: true,
            status: Some(AuthCheck {
                kind: AuthType::ApiKey,
                source: None,
            }),
        };
        assert!(component
            .format_status_indicator(&mismatched)
            .contains("API key configured"));
    }
}
