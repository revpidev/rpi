//! Slash-command autocomplete provider — port of
//! `packages/coding-agent/src/modes/interactive/interactive-mode.ts` @ pi
//! 0.82.1 (2efa728):
//! - `getAutocompleteSourceTag` / `prefixAutocompleteDescription`
//!   (interactive-mode.ts:497-528)
//! - `getBuiltinCommandConflictDiagnostics` (interactive-mode.ts:530-543)
//! - `createBaseAutocompleteProvider` (interactive-mode.ts:545-629)
//! - `setupAutocompleteProvider` (interactive-mode.ts:631-647)
//!
//! The provider combines four command sources: the built-in slash commands
//! (`core::slash_commands::BUILTIN_SLASH_COMMANDS`), prompt templates
//! (`session.prompt_templates()`), extension commands (T15 hook — empty) and
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
//! - `/login` argument completions are a T13 hook (`get_login_provider_options`
//!   returns `None` + TODO); upstream wires `getLoginProviderOptions`
//!   (interactive-mode.ts:4845) through `getLoginProviderCompletionOptions`
//!   (:269-289) and `getLoginProviderSearchText` (:291-296).
//! - Extension commands and the `fdPath` are T15 hooks (empty / `None`);
//!   `getBuiltinCommandConflictDiagnostics` therefore reports no conflicts
//!   yet. Upstream returns `ResourceDiagnostic[]`; the port returns plain
//!   messages (`Vec<String>`) until the extension runner lands.
//! - Git sources in [`get_autocomplete_source_tag`] fall back to the plain
//!   scope prefix (upstream's last line): `parseGitUrl` (utils/git.ts:172,
//!   hosted-git-info) is not ported. Local `SourceInfo`s only carry
//!   `"local"`/`"auto"`/`"package"` sources today, so the fallback is
//!   unobservable in this slice.
//! - The `skill_commands` map lives on [`InteractiveUi`] (allowed field
//!   addition, upstream class field cleared at the top of
//!   `createBaseAutocompleteProvider`, interactive-mode.ts:611-616); the
//!   slash-command dispatch reads it.
//! - The base provider is wrapped so `trigger_characters()` reports the
//!   editor defaults plus `/`. The editor merges provider triggers with its
//!   own defaults and skips `/` (editor.ts:2219-2230, slash triggering is
//!   handled natively at the start of a message), so the wrapper documents
//!   the upstream `setupAutocompleteProvider` wrapping shape without
//!   changing the effective trigger set.

use std::sync::{Arc, Mutex};

use pir_tui::autocomplete::{
    AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions, CombinedAutocompleteProvider,
    CompletionResult, GetSuggestionsOptions, SlashCommand as TuiSlashCommand, SlashCommandOrItem,
};
use pir_tui::fuzzy::fuzzy_filter;

use crate::core::skills::{SourceInfo, SourceScope};
use crate::core::slash_commands::BUILTIN_SLASH_COMMANDS;
use crate::modes::interactive::components::model_search::{
    get_model_selector_search_text, ModelSearchItem,
};
use crate::modes::interactive::interactive_mode::InteractiveUi;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// Source tags (interactive-mode.ts:497-528)
// ---------------------------------------------------------------------------

/// `getAutocompleteSourceTag` (interactive-mode.ts:497-520): the short
/// `[u]`-style prefix shown on command descriptions. `scope` maps to
/// `u`/`p`/`t` (user/project/temporary); the source string narrows it:
/// `auto`/`local`/`cli` → scope prefix only, `npm:` → `scope:npm:...`, a git
/// source → `scope:git:host/path@ref` (not ported, see module header).
pub(crate) fn get_autocomplete_source_tag(source_info: Option<&SourceInfo>) -> Option<String> {
    let source_info = source_info?;
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
    // TODO(T14): git sources (`parseGitUrl`, utils/git.ts:172) — fall back
    // to the scope prefix, upstream's last line (interactive-mode.ts:519).
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
// Conflict diagnostics (interactive-mode.ts:530-543)
// ---------------------------------------------------------------------------

/// `getBuiltinCommandConflictDiagnostics` (interactive-mode.ts:530-543):
/// warnings for extension commands whose name collides with a built-in. The
/// extension command list is a T15 hook, so there is nothing to diagnose yet.
///
/// Upstream returns `ResourceDiagnostic[]` (`{ type: "warning", message,
/// path }`); the port returns the message strings until the extension runner
/// lands (T15 will switch to the full diagnostic shape).
pub(crate) fn get_builtin_command_conflict_diagnostics(_ui: &InteractiveUi) -> Vec<String> {
    // TODO(T15): for each registered extension command whose name is a
    // built-in (core::slash_commands::is_builtin_command), emit
    //   invocationName === name
    //     ? `Extension command '/{name}' conflicts with built-in interactive
    //        command. Skipping in autocomplete.`
    //     : `Extension command '/{name}' conflicts with built-in interactive
    //        command. Available as '/{invocationName}'.`
    // (interactive-mode.ts:535-541).
    Vec::new()
}

// ---------------------------------------------------------------------------
// Base provider (interactive-mode.ts:545-629)
// ---------------------------------------------------------------------------

/// `createBaseAutocompleteProvider` (interactive-mode.ts:545-629): the
/// combined slash-command provider — built-ins (+ `/model` and `/login`
/// argument completions), prompt templates, extension commands (T15) and
/// skills.
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

    // /login argument completions (interactive-mode.ts:580-590) — T13 hook.
    if let Some(login_command) = slash_commands.iter_mut().find(|c| c.name == "login") {
        login_command.get_argument_completions = Some(Box::new(|_prefix: &str| {
            // TODO(T13): getLoginProviderOptions (interactive-mode.ts:4845)
            // → getLoginProviderCompletionOptions (:269-289) → fuzzy items
            // with getLoginProviderSearchText (:291-296) and
            // formatLoginProviderCompletionDescription (:298-301).
            None
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

    // Extension commands (interactive-mode.ts:599-608) — T15 hook: filtered
    // to names not shadowed by a built-in, invocation name as command name,
    // description prefixed with the extension's source tag.
    let extension_commands: Vec<SlashCommandOrItem> = Vec::new(); // TODO(T15)

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

    // Assemble (interactive-mode.ts:624-628): upstream passes
    // `this.sessionManager.getCwd()` and `this.fdPath` (T15 hook here).
    let base_path = lock(&ui.session().session_manager())
        .get_cwd()
        .to_string_lossy()
        .into_owned();
    let fd_path: Option<String> = None; // TODO(T15)
    let mut commands: Vec<SlashCommandOrItem> = slash_commands
        .into_iter()
        .map(SlashCommandOrItem::Command)
        .collect();
    commands.extend(template_commands);
    commands.extend(extension_commands);
    commands.extend(skill_commands);
    let combined = CombinedAutocompleteProvider::new(commands, base_path, fd_path);

    Arc::new(TriggerCharactersWrapper {
        delegate: Arc::new(combined),
    })
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

// ---------------------------------------------------------------------------
// Setup (interactive-mode.ts:631-647)
// ---------------------------------------------------------------------------

/// `setupAutocompleteProvider` (interactive-mode.ts:631-647): build the base
/// provider and install it on the editor. Upstream also sets it on a
/// secondary editor when one is active; the mode has a single editor.
pub(crate) fn setup_autocomplete(ui: &Arc<InteractiveUi>) {
    let provider = create_base_autocomplete_provider(ui);
    lock(&ui.editor).set_autocomplete_provider(provider);
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;

    use pir_tui::autocomplete::GetSuggestionsOptions;

    use crate::core::model_resolver::ScopedModel;
    use crate::core::skills::{SourceInfo, SourceOrigin, SourceScope};
    use crate::modes::interactive::interactive_mode::{InteractiveMode, InteractiveModeOptions};
    use crate::modes::interactive::test_support::{build_test_session, TestTerminal};

    use super::*;

    /// The test session's model runtime snapshot is empty unless
    /// `PIR_TEST_INTERACTIVE_KEY` is set (the models.json `apiKey` is an
    /// env-var name); clear it so the registry path is deterministic.
    fn clear_test_api_key() {
        std::env::remove_var("PIR_TEST_INTERACTIVE_KEY");
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
    fn source_tag_npm_and_unknown_sources() {
        assert_eq!(
            get_autocomplete_source_tag(Some(&source_info(SourceScope::User, "npm:@scope/skill"))),
            Some("u:npm:@scope/skill".to_string())
        );
        // Unparseable sources (git in this slice, TODO(T14)) fall back to
        // the scope prefix (interactive-mode.ts:519).
        assert_eq!(
            get_autocomplete_source_tag(Some(&source_info(SourceScope::Project, "git:custom"))),
            Some("p".to_string())
        );
        // Whitespace is trimmed before classification.
        assert_eq!(
            get_autocomplete_source_tag(Some(&source_info(SourceScope::User, " local "))),
            Some("u".to_string())
        );
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

    // ---- conflict diagnostics (T15 hook) ----

    #[tokio::test]
    async fn conflict_diagnostics_empty_without_extensions() {
        let mode = mode_harness().await;
        let ui = Arc::clone(&mode.ui_state);
        assert!(get_builtin_command_conflict_diagnostics(&ui).is_empty());
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
        assert!(ui
            .session()
            .model_runtime()
            .get_available_snapshot()
            .is_empty());
        let provider = create_base_autocomplete_provider(&ui);
        assert!(get_suggestions(provider.as_ref(), "/model m1", false).is_none());
    }

    #[tokio::test]
    async fn login_argument_completions_are_t13_hook() {
        let mode = mode_harness().await;
        let ui = Arc::clone(&mode.ui_state);
        let provider = create_base_autocomplete_provider(&ui);
        assert!(get_suggestions(provider.as_ref(), "/login openai", false).is_none());
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
}
