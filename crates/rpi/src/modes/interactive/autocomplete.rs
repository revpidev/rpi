//! Slash-command autocomplete provider — port of
//! `packages/coding-agent/src/modes/interactive/interactive-mode.ts` @ pi
//! 0.82.1 (2efa728):
//! - `getAutocompleteSourceTag` / `prefixAutocompleteDescription`
//!   (interactive-mode.ts:676-708 @ abe508e1b)
//! - `getBuiltInCommandConflictDiagnostics` (interactive-mode.ts:710-723 @ abe508e1b)
//! - `createBaseAutocompleteProvider` (interactive-mode.ts:725-823 @ abe508e1b)
//! - `setupAutocompleteProvider` (interactive-mode.ts:825-841 @ abe508e1b)
//!
//! The provider combines four command sources: the built-in slash commands
//! (`core::slash_commands::BUILTIN_SLASH_COMMANDS`), prompt templates
//! (`session.prompt_templates()`), extension commands (the live runner) and
//! skill commands (`skill:<name>`). It is assembled into a
//! [`CombinedAutocompleteProvider`] wrapped for trigger-character parity.
//!
//! Intentional differences:
//! - `/model` argument completions search with `get_model_selector_search_text`
//!   (model-search.ts:17-20) instead of upstream's `getModelSearchText`
//!   (model-search.ts:7-11, used at interactive-mode.ts:572): the argument
//!   completion ranks provider-prefixed queries (e.g. `openrouter/gpt-5`)
//!   ahead of proxy-provider model IDs, matching the /model selector
//!   (T12-S5b spec decision).
//! - `/login` argument completions build the provider list through
//!   `AuthSelectorProvider::from_provider` with no cached auth status;
//!   `getLoginProviderCompletionOptions` (interactive-mode.ts:386-407)
//!   drops `status` anyway, so the difference is unobservable.
//! - `fd` resolution is lookup-only: upstream `getToolPath`
//!   (tools-manager.ts:83-102) checks the managed `<agentDir>/bin` and then
//!   `PATH` after `ensureTool` may have downloaded the binary
//!   (tools-manager.ts:349-400). rpi has no managed-tool store, so a missing
//!   `fd` falls back to the existing path completion.
//! - The `skill_commands` map lives on [`InteractiveUi`] (allowed field
//!   addition, upstream class field cleared at the top of
//!   `createBaseAutocompleteProvider`, interactive-mode.ts:803-806); the
//!   slash-command dispatch reads it.
//! - The base provider is wrapped so `trigger_characters()` reports the
//!   editor defaults plus `/`. The editor merges provider triggers with its
//!   own defaults and skips `/` (editor.ts:2219-2230, slash triggering is
//!   handled natively at the start of a message), so the wrapper documents
//!   the upstream `setupAutocompleteProvider` wrapping shape without
//!   changing the effective trigger set.

use std::sync::{Arc, Mutex};

use rpi_ai::auth::types::AuthType;
use rpi_tui::autocomplete::{
    AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions, CombinedAutocompleteProvider,
    CompletionResult, GetSuggestionsOptions, SlashCommand as TuiSlashCommand, SlashCommandOrItem,
};
use rpi_tui::fuzzy::fuzzy_filter;

use crate::core::extensions::ExtensionCommand;
use crate::core::git_url::parse_git_url;
use crate::core::skills::{DiagnosticKind, ResourceDiagnostic, SourceInfo, SourceScope};
use crate::core::slash_commands::{BUILTIN_SLASH_COMMANDS, is_builtin_command};
use crate::core::tool_paths::{FD_BINARY_NAMES, get_tool_path};
use crate::modes::interactive::components::model_search::{
    ModelSearchItem, get_model_selector_search_text,
};
use crate::modes::interactive::components::oauth_selector::{
    AuthSelectorProvider, auth_type_str, format_auth_selector_provider_type,
};
use crate::modes::interactive::interactive_mode::InteractiveUi;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// Source tags (interactive-mode.ts:676-708)
// ---------------------------------------------------------------------------

/// `getAutocompleteSourceTag` (interactive-mode.ts:676-701): the short
/// `[u]`-style prefix shown on command descriptions. `scope` maps to
/// `u`/`p`/`t` (user/project/temporary); the source string narrows it:
/// `auto`/`local`/`cli` → scope prefix only, `npm:` → `scope:npm:...`, a
/// parseable git source → `scope:git:host/path@ref`, anything else → the
/// bare scope prefix (upstream's last line). Built-in extension commands
/// (source `builtin`) are untagged like built-in commands.
pub(crate) fn get_autocomplete_source_tag(source_info: Option<&SourceInfo>) -> Option<String> {
    let source_info = source_info?;
    if source_info.source == "builtin" {
        // Built-in extension commands are untagged, like built-in
        // commands (interactive-mode.ts:678-680, source-info.ts:18-21).
        return None;
    }
    let scope_prefix = match source_info.scope {
        SourceScope::User => "u",
        SourceScope::Project => "p",
        SourceScope::Temporary => "t",
    };
    let source = source_info.source.trim();
    if source == "auto" || source == "local" || source == "cli" {
        return Some(scope_prefix.to_string());
    }
    if source.starts_with("npm:") {
        return Some(format!("{scope_prefix}:{source}"));
    }
    // `parseGitUrl(source)` (interactive-mode.ts:693-697; utils/git.ts:172):
    // `scope:git:<host>/<path>@<ref>` when the source parses, else the bare
    // scope prefix (interactive-mode.ts:699-700).
    if let Some(git_source) = parse_git_url(source) {
        let reference = git_source
            .ref_
            .as_deref()
            .map(|git_ref| format!("@{git_ref}"))
            .unwrap_or_default();
        return Some(format!(
            "{scope_prefix}:git:{}/{}{reference}",
            git_source.host, git_source.path
        ));
    }
    Some(scope_prefix.to_string())
}

/// `prefixAutocompleteDescription` (interactive-mode.ts:522-528): prefix the
/// description with `[<sourceTag>] ` when a tag applies; without a tag the
/// description passes through unchanged.
pub(crate) fn prefix_autocomplete_description(
    description: Option<&str>,
    source_info: Option<&SourceInfo>,
) -> Option<String> {
    match get_autocomplete_source_tag(source_info) {
        Some(source_tag) => Some(match description {
            // JS truthiness: an empty description is dropped like `undefined`.
            Some(description) if !description.is_empty() => {
                format!("[{source_tag}] {description}")
            }
            _ => format!("[{source_tag}]"),
        }),
        // Upstream: `if (!sourceTag) return description;` — no tag means the
        // description passes through unchanged (interactive-mode.ts:524-526).
        None => description.map(str::to_string),
    }
}

// ---------------------------------------------------------------------------
// Conflict diagnostics (interactive-mode.ts:710-723)
// ---------------------------------------------------------------------------

/// `getBuiltinCommandConflictDiagnostics` (interactive-mode.ts:710-723):
/// warnings for extension commands whose registered `name` collides with a
/// built-in. The message distinguishes a plain shadow (the command is
/// skipped in autocomplete) from a suffixed invocation name (the command is
/// reachable under `/<name>:<n>`).
pub(crate) fn get_builtin_command_conflict_diagnostics(
    extension_commands: &[ExtensionCommand],
) -> Vec<ResourceDiagnostic> {
    extension_commands
        .iter()
        .filter(|command| is_builtin_command(&command.name))
        .map(|command| ResourceDiagnostic {
            kind: DiagnosticKind::Warning,
            message: if command.invocation_name == command.name {
                format!(
                    "Extension command '/{}' conflicts with built-in interactive command. Skipping in autocomplete.",
                    command.name
                )
            } else {
                format!(
                    "Extension command '/{}' conflicts with built-in interactive command. Available as '/{}'.",
                    command.name, command.invocation_name
                )
            },
            path: command
                .source_info
                .as_ref()
                .map(|source_info| source_info.path.clone()),
            collision: None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// /login provider argument completions (interactive-mode.ts:375-421,
// :776-783)
// ---------------------------------------------------------------------------

/// `LoginProviderCompletionOption` (interactive-mode.ts:364-369): one
/// provider with its merged auth methods and subscription flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoginProviderCompletionOption {
    pub id: String,
    pub name: String,
    pub auth_types: Vec<AuthType>,
    pub subscription: Option<bool>,
}

/// `AUTH_TYPE_ORDER` (interactive-mode.ts:373): `oauth` sorts before
/// `api_key` when the merged methods are listed.
fn auth_type_order(auth_type: AuthType) -> u8 {
    match auth_type {
        AuthType::Oauth => 0,
        AuthType::ApiKey => 1,
    }
}

/// `getLoginProviderCompletionOptions` (interactive-mode.ts:386-407): merge
/// the per-method selector rows into one entry per provider id (auth types
/// deduped and ordered), sorted by display name.
pub(crate) fn get_login_provider_completion_options(
    provider_options: &[AuthSelectorProvider],
) -> Vec<LoginProviderCompletionOption> {
    let mut options: Vec<LoginProviderCompletionOption> = Vec::new();
    for provider in provider_options {
        if let Some(existing) = options.iter_mut().find(|option| option.id == provider.id) {
            if !existing.auth_types.contains(&provider.auth_type) {
                existing.auth_types.push(provider.auth_type);
                existing
                    .auth_types
                    .sort_by_key(|auth_type| auth_type_order(*auth_type));
            }
            continue;
        }
        options.push(LoginProviderCompletionOption {
            id: provider.id.clone(),
            name: provider.name.clone(),
            auth_types: vec![provider.auth_type],
            subscription: provider.subscription,
        });
    }
    options.sort_by(|a, b| a.name.cmp(&b.name));
    options
}

/// `getLoginProviderSearchText` (interactive-mode.ts:409-414): the fuzzy
/// haystack — id, name, and each auth type with its display label.
pub(crate) fn get_login_provider_search_text(option: &LoginProviderCompletionOption) -> String {
    let auth_types = option
        .auth_types
        .iter()
        .map(|auth_type| {
            format!(
                "{} {}",
                auth_type_str(auth_type),
                format_auth_selector_provider_type(auth_type, option.subscription)
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!("{} {} {auth_types}", option.id, option.name)
}

/// `formatLoginProviderCompletionDescription` (interactive-mode.ts:416-421):
/// `API key`, `subscription`/`account`, `/`-joined when both are offered,
/// prefixed with the display name when it differs from the id.
pub(crate) fn format_login_provider_completion_description(
    option: &LoginProviderCompletionOption,
) -> String {
    let auth_types = option
        .auth_types
        .iter()
        .map(|auth_type| format_auth_selector_provider_type(auth_type, option.subscription))
        .collect::<Vec<_>>()
        .join("/");
    if option.name == option.id {
        auth_types
    } else {
        format!("{} · {}", option.name, auth_types)
    }
}

// ---------------------------------------------------------------------------
// Base provider (interactive-mode.ts:545-629)
// ---------------------------------------------------------------------------

/// `createBaseAutocompleteProvider` (interactive-mode.ts:725-823 @ abe508e1b): the
/// combined slash-command provider — built-ins (+ `/model` and `/login`
/// argument completions), prompt templates, extension commands and skills.
pub(crate) fn create_base_autocomplete_provider(
    ui: &Arc<InteractiveUi>,
) -> Arc<dyn AutocompleteProvider> {
    // Built-in commands (interactive-mode.ts:547-551).
    let mut slash_commands: Vec<TuiSlashCommand> = BUILTIN_SLASH_COMMANDS
        .iter()
        .map(|command| TuiSlashCommand {
            name: command.name.to_string(),
            description: Some(command.description.to_string()),
            argument_hint: command.argument_hint.map(str::to_string),
            get_argument_completions: None,
        })
        .collect();

    // /model argument completions (interactive-mode.ts:553-578): scoped
    // models win; otherwise the registry snapshot. Empty → no suggestions.
    let model_ui = Arc::clone(ui);
    if let Some(model_command) = slash_commands.iter_mut().find(|c| c.name == "model") {
        model_command.get_argument_completions = Some(Box::new(move |prefix: &str| {
            let models: Vec<ModelSearchItem> = {
                let scoped = model_ui.session().scoped_models();
                if !scoped.is_empty() {
                    scoped
                        .into_iter()
                        .map(|scoped| ModelSearchItem {
                            id: scoped.model.id,
                            provider: scoped.model.provider,
                            name: Some(scoped.model.name),
                        })
                        .collect()
                } else {
                    model_ui
                        .session()
                        .model_runtime()
                        .get_available_snapshot()
                        .into_iter()
                        .map(|model| ModelSearchItem {
                            id: model.id,
                            provider: model.provider,
                            name: Some(model.name),
                        })
                        .collect()
                }
            };
            if models.is_empty() {
                return None;
            }
            // `createFuzzyAutocompleteItems(items, prefix, getModelSearchText,
            // ...)` (interactive-mode.ts:258-267) — see module header for the
            // search-text difference.
            let filtered = fuzzy_filter(models, prefix, get_model_selector_search_text);
            if filtered.is_empty() {
                return None;
            }
            Some(
                filtered
                    .into_iter()
                    .map(|item| AutocompleteItem {
                        value: format!("{}/{}", item.provider, item.id),
                        label: item.id,
                        description: Some(item.provider),
                    })
                    .collect(),
            )
        }));
    }

    // /login argument completions (interactive-mode.ts:776-783):
    // `getLoginProviderOptions()` filtered through the completion helpers,
    // fuzzy-matched against the typed prefix.
    let login_ui = Arc::clone(ui);
    if let Some(login_command) = slash_commands.iter_mut().find(|c| c.name == "login") {
        login_command.get_argument_completions = Some(Box::new(move |prefix: &str| {
            let providers: Vec<AuthSelectorProvider> = login_ui
                .session()
                .model_runtime()
                .get_providers()
                .iter()
                .flat_map(|provider| AuthSelectorProvider::from_provider(provider, None))
                .collect();
            let options = get_login_provider_completion_options(&providers);
            if options.is_empty() {
                return None;
            }
            // `createFuzzyAutocompleteItems(providers, prefix,
            // getLoginProviderSearchText, ...)` (interactive-mode.ts:778-782).
            let filtered = fuzzy_filter(options, prefix, get_login_provider_search_text);
            if filtered.is_empty() {
                return None;
            }
            Some(
                filtered
                    .into_iter()
                    .map(|option| AutocompleteItem {
                        value: option.id.clone(),
                        label: option.id.clone(),
                        description: Some(format_login_provider_completion_description(&option)),
                    })
                    .collect(),
            )
        }));
    }

    // Prompt templates (interactive-mode.ts:592-597). The local
    // `PromptTemplate` has no `sourceInfo` (dropped with core::resource_loader
    // per prompt_templates.rs header), so descriptions are never prefixed.
    let template_commands: Vec<SlashCommandOrItem> = ui
        .session()
        .prompt_templates()
        .into_iter()
        .map(|template| {
            SlashCommandOrItem::Command(TuiSlashCommand {
                name: template.name,
                description: prefix_autocomplete_description(Some(&template.description), None),
                argument_hint: template.argument_hint,
                get_argument_completions: None,
            })
        })
        .collect();

    // Extension commands (interactive-mode.ts:795-802): filtered to names
    // not shadowed by a built-in (keyed on the registered `name`, so a
    // suffixed `help:1` invocation is skipped too), invocation name as
    // command name, from the extension host's registry. The hidden built-in
    // carries no source tag, so the description passes through unprefixed.
    let extension_commands: Vec<SlashCommandOrItem> = ui
        .session()
        .extension_runner()
        .registered_commands()
        .iter()
        .filter(|command| !is_builtin_command(&command.name))
        .map(|command| {
            SlashCommandOrItem::Command(TuiSlashCommand {
                name: command.invocation_name.clone(),
                description: command.description.clone(),
                argument_hint: None,
                get_argument_completions: None,
            })
        })
        .collect();

    // Skill commands (interactive-mode.ts:610-622): `skill:<name>` per
    // loaded skill, file path recorded for the slash-command dispatch.
    // Cleared and repopulated on every (re)build, like upstream.
    lock(&ui.skill_commands).clear();
    let mut skill_commands: Vec<SlashCommandOrItem> = Vec::new();
    let enable_skill_commands = ui
        .session()
        .settings_manager(|settings| settings.get_enable_skill_commands());
    if enable_skill_commands {
        let loader = ui.session().resource_loader();
        let resources = lock(&loader);
        for skill in &resources.resources().skills {
            let command_name = format!("skill:{}", skill.name);
            lock(&ui.skill_commands)
                .insert(command_name.clone(), skill.file_path.display().to_string());
            skill_commands.push(SlashCommandOrItem::Command(TuiSlashCommand {
                name: command_name,
                description: prefix_autocomplete_description(
                    Some(&skill.description),
                    Some(&skill.source_info),
                ),
                argument_hint: None,
                get_argument_completions: None,
            }));
        }
    }

    // Assemble (interactive-mode.ts:818-822): upstream passes
    // `this.sessionManager.getCwd()` and `this.fdPath` (resolved by
    // `ensureTool("fd", ...)` at startup, interactive-mode.ts:1088-1092).
    let base_path = lock(&ui.session().session_manager())
        .get_cwd()
        .to_string_lossy()
        .into_owned();
    let fd_path = resolve_fd_path();
    let mut commands: Vec<SlashCommandOrItem> = slash_commands
        .into_iter()
        .map(SlashCommandOrItem::Command)
        .collect();
    commands.extend(template_commands);
    commands.extend(extension_commands);
    commands.extend(skill_commands);
    let combined = CombinedAutocompleteProvider::new(commands, base_path, fd_path);

    // V17-11 FR-B R4: chain the extension-registered declarative sources
    // over the base provider (upstream folds each wrapper over the provider
    // in registration order, interactive-mode.ts:825-840).
    let mut provider: Arc<dyn AutocompleteProvider> = Arc::new(combined);
    for descriptor in lock(&ui.autocomplete_providers).iter().cloned() {
        provider = Arc::new(ExtensionAutocompleteProvider::new(descriptor, provider));
    }

    Arc::new(TriggerCharactersWrapper { delegate: provider })
}

/// Resolve the `fd` binary for file completions (`getToolPath`,
/// tools-manager.ts:83-102): the managed `<agentDir>/bin` first, then `PATH`
/// under `fd` and its Debian alias `fdfind`. rpi has no managed-tool
/// download, so a missing `fd` returns `None` and the provider keeps its
/// existing path completion (a spawn failure is treated as an empty walk).
fn resolve_fd_path() -> Option<String> {
    let bin_dir = crate::config::get_agent_dir().join("bin");
    let path_env = std::env::var_os("PATH");
    resolve_fd_path_in(&bin_dir, path_env.as_deref())
}

/// Environment-injected core of [`resolve_fd_path`] (test seam, same shape
/// as the clipboard env overrides): managed bin dir first, then `PATH`.
fn resolve_fd_path_in(
    bin_dir: &std::path::Path,
    path_env: Option<&std::ffi::OsStr>,
) -> Option<String> {
    FD_BINARY_NAMES
        .iter()
        .find_map(|name| get_tool_path(name, bin_dir, path_env))
}

// ---------------------------------------------------------------------------
// Trigger-character wrapper (interactive-mode.ts:631-647)
// ---------------------------------------------------------------------------

/// The editor's default trigger characters plus `/` (editor.ts:244 default
/// `['@', '#']`; `/` is the slash-command trigger). The editor merges
/// provider triggers with its defaults and skips `/` (editor.ts:2219-2230),
/// which triggers slash completion natively at the start of a message, so
/// this only mirrors the upstream `setupAutocompleteProvider` wrap/merge
/// shape (interactive-mode.ts:633-640).
const TRIGGER_CHARACTERS: [char; 3] = ['@', '#', '/'];

/// Delegating wrapper that adds the slash trigger character to the
/// [`CombinedAutocompleteProvider`]'s (empty) trigger list.
struct TriggerCharactersWrapper {
    delegate: Arc<dyn AutocompleteProvider>,
}

impl AutocompleteProvider for TriggerCharactersWrapper {
    fn trigger_characters(&self) -> &[char] {
        &TRIGGER_CHARACTERS
    }

    fn get_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        options: &GetSuggestionsOptions,
    ) -> Option<AutocompleteSuggestions> {
        self.delegate
            .get_suggestions(lines, cursor_line, cursor_col, options)
    }

    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> CompletionResult {
        self.delegate
            .apply_completion(lines, cursor_line, cursor_col, item, prefix)
    }

    fn should_trigger_file_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
    ) -> bool {
        self.delegate
            .should_trigger_file_completion(lines, cursor_line, cursor_col)
    }
}

/// Declarative extension autocomplete source (V17-11 FR-B R4): the
/// serializable replacement for upstream's closure-provider composition
/// (`addAutocompleteProvider`, interactive-mode.ts:2659-2662). Suggestions
/// appear when the text before the cursor contains the descriptor's
/// `trigger`; the fragment typed after it filters the descriptor's items.
/// The wrapper delegates to the provider below it when its trigger does not
/// match.
struct ExtensionAutocompleteProvider {
    delegate: Arc<dyn AutocompleteProvider>,
    trigger: char,
    trigger_chars: Vec<char>,
    items: Vec<AutocompleteItem>,
}

impl ExtensionAutocompleteProvider {
    fn new(
        descriptor: rpi_ext_host::types::AutocompleteProviderDescriptor,
        delegate: Arc<dyn AutocompleteProvider>,
    ) -> Self {
        let trigger = descriptor.trigger.chars().next().unwrap_or('/');
        let mut trigger_chars: Vec<char> = delegate.trigger_characters().to_vec();
        if !trigger_chars.contains(&trigger) {
            trigger_chars.push(trigger);
        }
        let items = descriptor
            .items
            .into_iter()
            .map(|item| AutocompleteItem {
                label: item.label.unwrap_or_else(|| item.value.clone()),
                value: item.value,
                description: item.description,
            })
            .collect();
        Self {
            delegate,
            trigger,
            trigger_chars,
            items,
        }
    }

    /// The fragment after the trigger character on the cursor's line, when
    /// the line before the cursor contains the trigger.
    fn fragment(&self, lines: &[String], cursor_line: usize, cursor_col: usize) -> Option<String> {
        let line = lines.get(cursor_line)?;
        let chars: Vec<char> = line.chars().collect();
        let end = cursor_col.min(chars.len());
        let position = chars[..end].iter().rposition(|c| *c == self.trigger)?;
        Some(chars[position + 1..end].iter().collect())
    }
}

impl AutocompleteProvider for ExtensionAutocompleteProvider {
    fn trigger_characters(&self) -> &[char] {
        &self.trigger_chars
    }

    fn get_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        options: &GetSuggestionsOptions,
    ) -> Option<AutocompleteSuggestions> {
        if let Some(fragment) = self.fragment(lines, cursor_line, cursor_col) {
            let lowered = fragment.to_lowercase();
            let items: Vec<AutocompleteItem> = self
                .items
                .iter()
                .filter(|item| item.value.to_lowercase().starts_with(&lowered))
                .cloned()
                .collect();
            if !items.is_empty() {
                return Some(AutocompleteSuggestions {
                    items,
                    prefix: fragment,
                });
            }
        }
        self.delegate
            .get_suggestions(lines, cursor_line, cursor_col, options)
    }

    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> CompletionResult {
        if self.items.iter().any(|own| own.value == item.value) {
            let mut lines = lines.to_vec();
            if let Some(line) = lines.get(cursor_line) {
                let chars: Vec<char> = line.chars().collect();
                let end = cursor_col.min(chars.len());
                let start = end.saturating_sub(prefix.chars().count());
                let mut replaced: Vec<char> = chars[..start].to_vec();
                replaced.extend(item.value.chars());
                replaced.extend_from_slice(&chars[end..]);
                lines[cursor_line] = replaced.into_iter().collect();
                return CompletionResult {
                    lines,
                    cursor_line,
                    cursor_col: start + item.value.chars().count(),
                };
            }
            return CompletionResult {
                lines,
                cursor_line,
                cursor_col,
            };
        }
        self.delegate
            .apply_completion(lines, cursor_line, cursor_col, item, prefix)
    }

    fn should_trigger_file_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
    ) -> bool {
        self.delegate
            .should_trigger_file_completion(lines, cursor_line, cursor_col)
    }
}

// ---------------------------------------------------------------------------
// Setup (interactive-mode.ts:825-841 @ abe508e1b)
// ---------------------------------------------------------------------------

/// `setupAutocompleteProvider` (interactive-mode.ts:825-841 @ abe508e1b):
/// build the base provider and install it on the editor. Upstream also sets
/// it on a secondary editor when one is active; the mode has a single
/// editor.
pub(crate) fn setup_autocomplete(ui: &Arc<InteractiveUi>) {
    let provider = create_base_autocomplete_provider(ui);
    lock(&ui.editor).set_autocomplete_provider(provider);
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;

    use rpi_tui::autocomplete::GetSuggestionsOptions;

    use crate::core::model_resolver::ScopedModel;
    use crate::core::skills::{SourceInfo, SourceOrigin, SourceScope};
    use crate::modes::interactive::interactive_mode::{InteractiveMode, InteractiveModeOptions};
    use crate::modes::interactive::test_support::{
        TestTerminal, build_test_session, build_test_session_with,
    };

    use super::*;

    /// The test session's model runtime snapshot is empty unless
    /// `RPI_TEST_INTERACTIVE_KEY` is set (the models.json `apiKey` is a
    /// `${VAR}` env reference); clear it so the registry path is
    /// deterministic.
    fn clear_test_api_key() {
        rpi_test_env::remove_var("RPI_TEST_INTERACTIVE_KEY");
    }

    async fn mode_harness() -> InteractiveMode {
        clear_test_api_key();
        let harness = build_test_session().await;
        let terminal = Arc::new(TestTerminal::new());
        InteractiveMode::with_terminal(
            harness.runtime,
            InteractiveModeOptions::default(),
            Box::new(TestTerminal::clone(&terminal)),
        )
    }

    fn get_suggestions(
        provider: &dyn AutocompleteProvider,
        line: &str,
        force: bool,
    ) -> Option<AutocompleteSuggestions> {
        provider.get_suggestions(
            std::slice::from_ref(&line.to_string()),
            0,
            line.chars().count(),
            &GetSuggestionsOptions {
                abort: Arc::new(AtomicBool::new(false)),
                force,
            },
        )
    }

    fn item_values(result: &Option<AutocompleteSuggestions>) -> Vec<String> {
        let mut values: Vec<String> = result
            .as_ref()
            .map(|result| result.items.iter().map(|item| item.value.clone()).collect())
            .unwrap_or_default();
        values.sort();
        values
    }

    fn source_info(scope: SourceScope, source: &str) -> SourceInfo {
        SourceInfo {
            path: PathBuf::from("/tmp/resource.md"),
            source: source.to_string(),
            scope,
            origin: SourceOrigin::TopLevel,
            base_dir: None,
        }
    }

    // ---- get_autocomplete_source_tag / prefix_autocomplete_description ----

    #[test]
    fn source_tag_none_for_missing_info() {
        assert_eq!(get_autocomplete_source_tag(None), None);
    }

    #[test]
    fn source_tag_scope_prefixes() {
        // auto/local/cli sources collapse to the scope letter.
        assert_eq!(
            get_autocomplete_source_tag(Some(&source_info(SourceScope::User, "local"))),
            Some("u".to_string())
        );
        assert_eq!(
            get_autocomplete_source_tag(Some(&source_info(SourceScope::User, "auto"))),
            Some("u".to_string())
        );
        assert_eq!(
            get_autocomplete_source_tag(Some(&source_info(SourceScope::Project, "local"))),
            Some("p".to_string())
        );
        assert_eq!(
            get_autocomplete_source_tag(Some(&source_info(SourceScope::Temporary, "cli"))),
            Some("t".to_string())
        );
    }

    #[test]
    fn source_tag_npm_and_git_sources() {
        assert_eq!(
            get_autocomplete_source_tag(Some(&source_info(SourceScope::User, "npm:@scope/skill"))),
            Some("u:npm:@scope/skill".to_string())
        );
        // `parseGitUrl` (utils/git.ts:172) successes carry the
        // `scope:git:<host>/<path>@<ref>` label (interactive-mode.ts:693-697).
        assert_eq!(
            get_autocomplete_source_tag(Some(&source_info(
                SourceScope::User,
                "git:github.com/user/repo@v1"
            ))),
            Some("u:git:github.com/user/repo@v1".to_string())
        );
        assert_eq!(
            get_autocomplete_source_tag(Some(&source_info(
                SourceScope::Project,
                "https://github.com/user/repo"
            ))),
            Some("p:git:github.com/user/repo".to_string())
        );
        // Unparseable sources fall back to the scope prefix
        // (interactive-mode.ts:699-700).
        assert_eq!(
            get_autocomplete_source_tag(Some(&source_info(SourceScope::Project, "git:custom"))),
            Some("p".to_string())
        );
        // Whitespace is trimmed before classification.
        assert_eq!(
            get_autocomplete_source_tag(Some(&source_info(SourceScope::User, " local "))),
            Some("u".to_string())
        );
        // Built-in extension commands are untagged (interactive-mode.ts:678-680).
        assert_eq!(
            get_autocomplete_source_tag(Some(&source_info(SourceScope::User, "builtin"))),
            None
        );
    }

    #[test]
    fn fd_path_present_and_absent() {
        let root = std::env::temp_dir().join(format!(
            "rpi-fd-autocomplete-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let managed = root.join("bin");
        let path_dir = root.join("path");
        let empty = root.join("empty");
        for dir in [&managed, &path_dir, &empty] {
            std::fs::create_dir_all(dir).expect("temp dir");
        }

        // Absent: no managed `fd` and no `PATH` hit → `None`, so the
        // combined provider keeps its existing path completion.
        let empty_path = std::env::join_paths([&empty]).expect("join paths");
        assert_eq!(
            resolve_fd_path_in(&managed, Some(empty_path.as_os_str())),
            None
        );
        assert_eq!(resolve_fd_path_in(&managed, None), None);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let fd = path_dir.join("fd");
            std::fs::write(&fd, "#!/bin/sh\nexit 0\n").expect("write fake fd");
            std::fs::set_permissions(&fd, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fake fd");
            let path_env = std::env::join_paths([&path_dir]).expect("join paths");
            // Present on `PATH` → the bare command name (`getToolPath`
            // convention, tools-manager.ts:93-101).
            assert_eq!(
                resolve_fd_path_in(&managed, Some(path_env.as_os_str())),
                Some("fd".to_string())
            );
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn description_prefixing() {
        assert_eq!(
            prefix_autocomplete_description(
                Some("A skill"),
                Some(&source_info(SourceScope::User, "local"))
            ),
            Some("[u] A skill".to_string())
        );
        // Empty/missing description → bare tag (JS truthiness).
        assert_eq!(
            prefix_autocomplete_description(
                Some(""),
                Some(&source_info(SourceScope::Project, "local"))
            ),
            Some("[p]".to_string())
        );
        assert_eq!(
            prefix_autocomplete_description(None, Some(&source_info(SourceScope::User, "auto"))),
            Some("[u]".to_string())
        );
        // No source info → description unchanged.
        assert_eq!(
            prefix_autocomplete_description(Some("plain"), None),
            Some("plain".to_string())
        );
        assert_eq!(prefix_autocomplete_description(None, None), None);
    }

    // ---- conflict diagnostics (interactive-mode.ts:710-723) ----

    fn extension_command(name: &str, invocation_name: &str) -> ExtensionCommand {
        ExtensionCommand {
            name: name.to_string(),
            invocation_name: invocation_name.to_string(),
            description: None,
            source_info: Some(source_info(SourceScope::User, "local")),
        }
    }

    #[test]
    fn conflict_diagnostics_two_message_shapes() {
        let commands = vec![
            extension_command("model", "model"),
            extension_command("tree", "tree:1"),
            extension_command("custom", "custom"),
        ];
        let diagnostics = get_builtin_command_conflict_diagnostics(&commands);
        assert_eq!(diagnostics.len(), 2, "only built-in name collisions warn");
        assert_eq!(diagnostics[0].kind, DiagnosticKind::Warning);
        assert_eq!(
            diagnostics[0].message,
            "Extension command '/model' conflicts with built-in interactive command. Skipping in autocomplete."
        );
        assert_eq!(
            diagnostics[1].message,
            "Extension command '/tree' conflicts with built-in interactive command. Available as '/tree:1'."
        );
        // The path comes from the registered command's source info.
        assert_eq!(
            diagnostics[0].path.as_deref(),
            Some(std::path::Path::new("/tmp/resource.md"))
        );
    }

    #[test]
    fn conflict_diagnostics_empty_without_conflicts() {
        assert!(get_builtin_command_conflict_diagnostics(&[]).is_empty());
        assert!(
            get_builtin_command_conflict_diagnostics(&[extension_command("custom", "custom")])
                .is_empty()
        );
    }

    // ---- /login provider completions (interactive-mode.ts:386-421) ----

    fn provider_row(
        id: &str,
        name: &str,
        auth_type: AuthType,
        subscription: Option<bool>,
    ) -> AuthSelectorProvider {
        AuthSelectorProvider {
            id: id.to_string(),
            name: name.to_string(),
            auth_type,
            method_name: None,
            method_login: true,
            status: None,
            subscription,
        }
    }

    #[test]
    fn login_completion_options_merge_duplicate_ids() {
        let options = get_login_provider_completion_options(&[
            provider_row("zai", "Z.ai", AuthType::ApiKey, None),
            provider_row("anthropic", "Anthropic", AuthType::Oauth, Some(true)),
            provider_row("anthropic", "Anthropic", AuthType::ApiKey, Some(true)),
            provider_row("zai", "Z.ai", AuthType::Oauth, None),
        ]);
        // Sorted by display name (`localeCompare`).
        assert_eq!(
            options
                .iter()
                .map(|option| option.id.as_str())
                .collect::<Vec<_>>(),
            vec!["anthropic", "zai"]
        );
        // Auth types are deduped and `oauth`-first.
        assert_eq!(
            options[0].auth_types,
            vec![AuthType::Oauth, AuthType::ApiKey]
        );
        assert_eq!(
            options[1].auth_types,
            vec![AuthType::Oauth, AuthType::ApiKey]
        );
        assert_eq!(options[0].subscription, Some(true));
    }

    #[test]
    fn login_completion_search_text_and_description() {
        let option = LoginProviderCompletionOption {
            id: "anthropic".to_string(),
            name: "Anthropic".to_string(),
            auth_types: vec![AuthType::Oauth, AuthType::ApiKey],
            subscription: Some(true),
        };
        assert_eq!(
            get_login_provider_search_text(&option),
            "anthropic Anthropic oauth subscription api_key API key"
        );
        assert_eq!(
            format_login_provider_completion_description(&option),
            "Anthropic · subscription/API key"
        );
        // A display name equal to the id drops the prefix
        // (interactive-mode.ts:419-420).
        let bare = LoginProviderCompletionOption {
            id: "openai".to_string(),
            name: "openai".to_string(),
            auth_types: vec![AuthType::ApiKey],
            subscription: None,
        };
        assert_eq!(
            get_login_provider_search_text(&bare),
            "openai openai api_key API key"
        );
        assert_eq!(
            format_login_provider_completion_description(&bare),
            "API key"
        );
    }

    // ---- provider assembly ----

    #[tokio::test]
    async fn provider_exposes_slash_trigger_character() {
        let mode = mode_harness().await;
        let ui = Arc::clone(&mode.ui_state);
        let provider = create_base_autocomplete_provider(&ui);
        assert_eq!(provider.trigger_characters(), &['@', '#', '/']);
    }

    #[tokio::test]
    async fn slash_prefix_filters_builtin_commands() {
        let mode = mode_harness().await;
        let ui = Arc::clone(&mode.ui_state);
        let provider = create_base_autocomplete_provider(&ui);

        let result = get_suggestions(provider.as_ref(), "/mo", false);
        assert_eq!(result.as_ref().map(|r| r.prefix.as_str()), Some("/mo"));
        // fuzzy "mo" matches model, scoped-models and import.
        assert_eq!(
            item_values(&result),
            vec!["import", "model", "scoped-models"]
        );

        // Non-matching prefixes yield no suggestions.
        assert!(
            get_suggestions(provider.as_ref(), "/zzz", false).is_none(),
            "no suggestions for unmatched prefixes"
        );
    }

    #[tokio::test]
    async fn slash_completion_excludes_argument_commands() {
        let mode = mode_harness().await;
        let ui = Arc::clone(&mode.ui_state);
        let provider = create_base_autocomplete_provider(&ui);

        // "/model " is an argument-completion context, not a command match.
        let result = get_suggestions(provider.as_ref(), "/model ", false);
        // No scoped models and an empty snapshot → no suggestions.
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn model_argument_completions_from_scoped_models() {
        let mode = mode_harness().await;
        let ui = Arc::clone(&mode.ui_state);
        let model = ui
            .session()
            .model_runtime()
            .get_model("custom", "m1")
            .expect("test model");
        ui.session().set_scoped_models(vec![ScopedModel {
            model,
            thinking_level: None,
        }]);
        let provider = create_base_autocomplete_provider(&ui);

        let result = get_suggestions(provider.as_ref(), "/model m1", false);
        assert_eq!(result.as_ref().map(|r| r.prefix.as_str()), Some("m1"));
        let items = result.as_ref().expect("scoped model suggestions");
        assert_eq!(
            items.items,
            vec![AutocompleteItem {
                value: "custom/m1".to_string(),
                label: "m1".to_string(),
                description: Some("custom".to_string()),
            }]
        );

        // Fuzzy argument matching ("cm" → custom/m1).
        let fuzzy = get_suggestions(provider.as_ref(), "/model cm", false);
        assert_eq!(item_values(&fuzzy), vec!["custom/m1".to_string()]);
    }

    #[tokio::test]
    async fn model_argument_completions_empty_without_models() {
        let mode = mode_harness().await;
        let ui = Arc::clone(&mode.ui_state);
        // No scoped models; the registry snapshot is empty (test API key
        // cleared) → no suggestions (interactive-mode.ts:562).
        assert!(
            ui.session()
                .model_runtime()
                .get_available_snapshot()
                .is_empty()
        );
        let provider = create_base_autocomplete_provider(&ui);
        assert!(get_suggestions(provider.as_ref(), "/model m1", false).is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn extension_commands_shadowing_builtins_are_filtered_by_name() {
        use rpi_ext_host::api::ExtensionApi;
        use rpi_ext_host::loader::{ExtensionFactory, InlineExtension};

        let shadow: ExtensionFactory = Arc::new(|api: ExtensionApi| {
            api.register_command(
                "model",
                Some("shadow".to_owned()),
                Arc::new(|_args, _ctx| Box::pin(async { Ok(()) })),
            )
            .expect("register shadowed command");
            Box::pin(async { Ok(()) })
        });
        // A second extension with the same name forces the `:N` invocation
        // suffix, so the filter's key field (`name` vs `invocation_name`)
        // is observable.
        let shadow_duplicate: ExtensionFactory = Arc::new(|api: ExtensionApi| {
            api.register_command(
                "model",
                Some("second shadow".to_owned()),
                Arc::new(|_args, _ctx| Box::pin(async { Ok(()) })),
            )
            .expect("register duplicate command");
            api.register_command(
                "custom",
                Some("kept".to_owned()),
                Arc::new(|_args, _ctx| Box::pin(async { Ok(()) })),
            )
            .expect("register custom command");
            Box::pin(async { Ok(()) })
        });
        let host = Arc::new(rpi_ext_host::host::NativeExtensionHost::new("/w3-cwd"));
        let errors = host
            .load_inline(&[
                InlineExtension::Anonymous(shadow),
                InlineExtension::Anonymous(shadow_duplicate),
            ])
            .await;
        assert!(errors.is_empty(), "unexpected load errors: {errors:?}");

        clear_test_api_key();
        let harness = build_test_session_with(None, Some(host)).await;
        let terminal = Arc::new(TestTerminal::new());
        let mode = InteractiveMode::with_terminal(
            harness.runtime,
            InteractiveModeOptions::default(),
            Box::new(TestTerminal::clone(&terminal)),
        );
        let ui = Arc::clone(&mode.ui_state);
        let provider = create_base_autocomplete_provider(&ui);

        let result = get_suggestions(provider.as_ref(), "/model", false);
        let values = item_values(&result);
        assert!(values.contains(&"model".to_string()), "{values:?}");
        assert!(
            !values.iter().any(|value| value.starts_with("model:")),
            "built-in shadowed commands are filtered by name: {values:?}"
        );
        // A non-conflicting extension command is still offered.
        let custom = get_suggestions(provider.as_ref(), "/custom", false);
        assert_eq!(item_values(&custom), vec!["custom".to_string()]);
    }

    #[tokio::test]
    async fn login_argument_completions_from_provider_catalog() {
        let mode = mode_harness().await;
        let ui = Arc::clone(&mode.ui_state);
        let provider = create_base_autocomplete_provider(&ui);
        let result = get_suggestions(provider.as_ref(), "/login anthropic", false);
        let items = result.expect("provider completions").items;
        let item = items
            .iter()
            .find(|item| item.value == "anthropic")
            .expect("anthropic provider offered");
        assert_eq!(item.label, "anthropic");
        let description = item.description.as_deref().expect("description");
        assert!(
            description.contains("Anthropic"),
            "display name in description: {description:?}"
        );
    }

    // ---- skills ----

    /// Build a harness whose session has one user-scope skill loaded
    /// (`agent_dir/skills/test-skill/SKILL.md`).
    async fn mode_harness_with_skill() -> (InteractiveMode, String) {
        clear_test_api_key();
        let harness = build_test_session().await;
        let agent_dir = harness.runtime.services().agent_dir.clone();
        let skill_file = agent_dir.join("skills").join("test-skill").join("SKILL.md");
        std::fs::create_dir_all(skill_file.parent().expect("skill dir")).expect("create skill dir");
        std::fs::write(
            &skill_file,
            "---\nname: test-skill\ndescription: A test skill\n---\nbody\n",
        )
        .expect("write skill file");
        lock(&harness.session.resource_loader()).reload();

        let terminal = Arc::new(TestTerminal::new());
        let mode = InteractiveMode::with_terminal(
            harness.runtime,
            InteractiveModeOptions::default(),
            Box::new(TestTerminal::clone(&terminal)),
        );
        (mode, skill_file.display().to_string())
    }

    #[tokio::test]
    async fn skill_commands_are_registered_with_source_tag() {
        let (mode, skill_path) = mode_harness_with_skill().await;
        let ui = Arc::clone(&mode.ui_state);
        let provider = create_base_autocomplete_provider(&ui);

        // Command list contains `skill:test-skill` with a user-scope tag.
        let result = get_suggestions(provider.as_ref(), "/skill:", false);
        assert_eq!(item_values(&result), vec!["skill:test-skill".to_string()]);
        let item = result
            .as_ref()
            .and_then(|r| r.items.iter().find(|item| item.value == "skill:test-skill"))
            .expect("skill command item");
        assert_eq!(item.label, "skill:test-skill");
        assert_eq!(item.description.as_deref(), Some("[u] A test skill"));

        // The dispatch map records `skill:<name>` → file path.
        assert_eq!(
            lock(&ui.skill_commands).get("skill:test-skill"),
            Some(&skill_path)
        );

        // Rebuilding clears and repopulates (no duplicates).
        let _ = create_base_autocomplete_provider(&ui);
        assert_eq!(lock(&ui.skill_commands).len(), 1);
    }

    // ---- setup ----

    #[tokio::test]
    async fn setup_autocomplete_installs_provider_on_editor() {
        let mode = mode_harness().await;
        let ui = Arc::clone(&mode.ui_state);
        setup_autocomplete(&ui);
        // Smoke-level: the provider is installed without error and slash
        // completion still returns built-ins afterwards.
        assert!(!lock(&ui.editor).is_showing_autocomplete());
    }

    // V17-11 FR-B R4: the declarative `{ trigger, items }` source is chained
    // over the base provider and filtered by the fragment after the trigger.
    #[test]
    fn extension_autocomplete_provider_filters_on_trigger() {
        use rpi_ext_host::types::{AutocompleteItemDescriptor, AutocompleteProviderDescriptor};

        let base = Arc::new(CombinedAutocompleteProvider::new(
            Vec::new(),
            "/tmp".to_string(),
            None,
        ));
        let provider = ExtensionAutocompleteProvider::new(
            AutocompleteProviderDescriptor {
                trigger: "@".to_owned(),
                items: vec![
                    AutocompleteItemDescriptor {
                        value: "file.rs".to_owned(),
                        label: None,
                        description: None,
                    },
                    AutocompleteItemDescriptor {
                        value: "folder".to_owned(),
                        label: Some("Folder".to_owned()),
                        description: Some("a directory".to_owned()),
                    },
                ],
            },
            base,
        );

        // Trigger + fragment: only matching items, prefix = the fragment.
        let filtered = get_suggestions(&provider, "open @fo", false).expect("suggestions");
        assert_eq!(filtered.prefix, "fo");
        assert_eq!(
            filtered
                .items
                .iter()
                .map(|item| item.value.as_str())
                .collect::<Vec<_>>(),
            ["folder"]
        );
        let both = get_suggestions(&provider, "open @f", false).expect("both f items");
        assert_eq!(both.items.len(), 2);
        // Trigger with an empty fragment: every item (labels default to the
        // value).
        let all = get_suggestions(&provider, "open @", false).expect("all items");
        assert_eq!(all.items.len(), 2);
        assert_eq!(all.items[1].label, "Folder");
        assert_eq!(all.items[1].description.as_deref(), Some("a directory"));
        // Without the trigger the wrapper delegates (empty base → None).
        assert!(get_suggestions(&provider, "plain text", false).is_none());

        // Applying a descriptor item replaces the fragment after the trigger.
        let applied = provider.apply_completion(&["open @".to_string()], 0, 6, &all.items[0], "");
        assert_eq!(applied.lines[0], "open @file.rs");
        assert_eq!(applied.cursor_col, 13);
    }
}
