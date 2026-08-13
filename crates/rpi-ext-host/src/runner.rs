//! Extension runner core — registration registries, same-name conflict
//! rules, and serial event dispatch @ pi 0.82.1 (2efa728).
//!
//! Port of `packages/coding-agent/src/core/extensions/runner.ts`:
//! - reserved shortcut list + `buildBuiltinKeybindings` (:67-111)
//! - `getAllRegisteredTools` / `getToolDefinition` / `getFlags` (:446-488)
//! - `getShortcuts` with last-wins + diagnostics (:490-537)
//! - `resolveRegisteredCommands` with `:N` suffixes (:595-629)
//! - `invalidate` / `assertActive` / `onError` / `emitError` /
//!   `hasHandlers` / renderer lookups (:539-593)
//! - `emit` serial dispatch (:788-820) and the specialized emitters
//!   `emitMessageEnd` / `emitToolResult` / `emitToolCall` / `emitUserBash` /
//!   `emitContext` / `emitBeforeProviderRequest` /
//!   `emitBeforeProviderHeaders` / `emitBeforeAgentStart` /
//!   `emitResourcesDiscover` / `emitInput` (:822-1222)
//! - `emitProjectTrustEvent` (:201-231)
//! - tool/flag conflict detection (`detectExtensionConflicts`,
//!   resource-loader.ts:1003-1038)
//!
//! Dispatch core: handlers cross as camelCase JSON (see types.rs header).
//! Intentional differences:
//! - `emitBeforeProviderHeaders`: upstream handlers mutate the headers map
//!   in place and the return value is ignored (runner.ts:1046-1051). JSON
//!   handlers cannot mutate in place, so a non-null object result *replaces*
//!   the headers (a `null` value inside still deletes that header). The
//!   typed native wrapper restores in-place `&mut` semantics on top.
//! - `emitContext` does not `structuredClone` the input (runner.ts:973):
//!   handlers receive owned JSON values, so no shared mutation is possible.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use serde_json::{Map, Value};

use crate::api::{
    EventHandler, ExtensionContext, ExtensionRuntime, HostActions, InsertionMap, LoadedExtension,
    Unsubscribe,
};
use crate::types::{
    is_session_before_event, DiagnosticKind, ExtensionError, ExtensionFlag, ExtensionShortcut,
    HostDiagnostic, RegisteredCommand, RegisteredTool, ResolvedCommand, EVENT_BEFORE_AGENT_START,
    EVENT_CONTEXT, EVENT_INPUT, EVENT_MESSAGE_END, EVENT_TOOL_CALL, EVENT_TOOL_RESULT,
    EVENT_USER_BASH,
};

fn read<T>(m: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    m.read().unwrap_or_else(|e| e.into_inner())
}

fn write<T>(m: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    m.write().unwrap_or_else(|e| e.into_inner())
}

/// `RESERVED_KEYBINDINGS_FOR_EXTENSION_CONFLICTS` (runner.ts:69-88) —
/// verbatim; extension shortcuts colliding with these built-in keybinding
/// ids are skipped.
pub const RESERVED_KEYBINDINGS_FOR_EXTENSION_CONFLICTS: [&str; 18] = [
    "app.interrupt",
    "app.clear",
    "app.exit",
    "app.suspend",
    "app.thinking.cycle",
    "app.model.cycleForward",
    "app.model.cycleBackward",
    "app.model.select",
    "app.tools.expand",
    "app.thinking.toggle",
    "app.editor.external",
    "app.message.copy",
    "app.message.followUp",
    "tui.input.submit",
    "tui.select.confirm",
    "tui.select.cancel",
    "tui.input.copy",
    "tui.editor.deleteToLineEnd",
];

/// Error listener registered through [`ExtensionRunnerCore::on_error`]
/// (`ExtensionErrorListener`, runner.ts:159).
pub type ExtensionErrorListener = Arc<dyn Fn(ExtensionError) + Send + Sync>;

/// One built-in keybinding entry after normalization
/// (`BuiltInKeyBindings` value, runner.ts:90).
#[derive(Debug, Clone, Copy)]
struct BuiltinKeybinding {
    /// Keybinding action id (only used in diagnostics).
    keybinding_idx: usize,
    restrict_override: bool,
}

/// Registration registry + emit dispatch over a loaded extension set.
///
/// The upstream `ExtensionRunner` also owns context factories and bound
/// action closures; here those live in the shared [`ExtensionRuntime`] (W3
/// binds actions, W5 binds context actions).
pub struct ExtensionRunnerCore {
    extensions: Vec<Arc<LoadedExtension>>,
    runtime: ExtensionRuntime,
    cwd: String,
    error_listeners: Arc<RwLock<HashMap<u64, ExtensionErrorListener>>>,
    next_listener_id: AtomicU64,
    shortcut_diagnostics: RwLock<Vec<HostDiagnostic>>,
    command_diagnostics: RwLock<Vec<HostDiagnostic>>,
}

impl ExtensionRunnerCore {
    pub fn new(
        extensions: Vec<Arc<LoadedExtension>>,
        runtime: ExtensionRuntime,
        cwd: String,
    ) -> Self {
        ExtensionRunnerCore {
            extensions,
            runtime,
            cwd,
            error_listeners: Arc::new(RwLock::new(HashMap::new())),
            next_listener_id: AtomicU64::new(0),
            shortcut_diagnostics: RwLock::new(Vec::new()),
            command_diagnostics: RwLock::new(Vec::new()),
        }
    }

    pub fn runtime(&self) -> ExtensionRuntime {
        self.runtime.clone()
    }

    pub fn extensions(&self) -> &[Arc<LoadedExtension>] {
        &self.extensions
    }

    /// `getExtensionPaths` (runner.ts:442-444).
    pub fn get_extension_paths(&self) -> Vec<String> {
        self.extensions.iter().map(|ext| ext.path.clone()).collect()
    }

    /// `createContext` (runner.ts:665-738).
    pub fn create_context(&self) -> ExtensionContext {
        ExtensionContext::new(self.runtime.clone(), self.cwd.clone())
    }

    /// `createCommandContext` (runner.ts:740-777) — command handlers get
    /// the session-control surface; event handlers never see it (upstream
    /// warns they deadlock; here the type separation enforces it).
    pub fn create_command_context(&self) -> crate::api::ExtensionCommandContext {
        crate::api::ExtensionCommandContext::new(self.create_context())
    }

    /// Bind host actions (`bindCore`, runner.ts:311-408): queued provider
    /// registrations flush through the bound actions, failures are reported
    /// via `emitError` with event `"register_provider"` (:358-364).
    pub async fn bind_actions(&self, actions: Arc<dyn HostActions>) {
        for registration in self.runtime.take_pending_provider_registrations() {
            // The queue drains before the actions are visible to
            // `register_provider`, so call the action directly.
            if let Err(error) = actions
                .register_provider(&registration.name, registration.config.clone())
                .await
            {
                self.emit_error(ExtensionError::new(
                    &registration.extension_path,
                    "register_provider",
                    error,
                ));
            }
        }
        for registration in self.runtime.take_pending_native_provider_registrations() {
            if let Err(error) = actions
                .register_native_provider(registration.provider.clone())
                .await
            {
                self.emit_error(ExtensionError::new(
                    &registration.extension_path,
                    "register_provider",
                    error,
                ));
            }
        }
        self.runtime.bind_actions(actions);
    }

    // ========================================================================
    // Stale lifecycle (runner.ts:539-552)
    // ========================================================================

    /// `invalidate(message?)` — first message wins (runner.ts:543-546).
    pub fn invalidate(&self, message: Option<String>) {
        self.runtime.invalidate(message);
    }

    /// `assertActive` (runner.ts:548-552).
    pub fn assert_active(&self) -> Result<(), crate::error::ExtError> {
        self.runtime.assert_active()
    }

    // ========================================================================
    // Error listeners (runner.ts:554-563)
    // ========================================================================

    /// `onError(listener)` — returns an unsubscribe closure.
    pub fn on_error(&self, listener: ExtensionErrorListener) -> Unsubscribe {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        write(&self.error_listeners).insert(id, listener);
        let listeners = Arc::downgrade(&self.error_listeners);
        Box::new(move || {
            if let Some(listeners) = listeners.upgrade() {
                write(&listeners).remove(&id);
            }
        })
    }

    /// `emitError(error)` (runner.ts:559-563).
    pub fn emit_error(&self, error: ExtensionError) {
        let listeners: Vec<ExtensionErrorListener> =
            read(&self.error_listeners).values().cloned().collect();
        for listener in listeners {
            listener(error.clone());
        }
    }

    // ========================================================================
    // Handler lookup (runner.ts:565-573)
    // ========================================================================

    /// `hasHandlers(eventType)`.
    pub fn has_handlers(&self, event_type: &str) -> bool {
        self.extensions
            .iter()
            .any(|ext| ext.has_handlers(event_type))
    }

    /// Snapshot of `(extension_path, handler)` pairs in dispatch order:
    /// extensions in load order, handlers in registration order within each
    /// extension (runner.ts:792-796).
    fn handlers_for(&self, event_type: &str) -> Vec<(String, EventHandler)> {
        let mut out = Vec::new();
        for ext in &self.extensions {
            for handler in ext.handlers_for(event_type) {
                out.push((ext.path.clone(), handler));
            }
        }
        out
    }

    // ========================================================================
    // Registration queries + conflict rules (runner.ts:446-629)
    // ========================================================================

    /// `getAllRegisteredTools` (runner.ts:447-457): first registration per
    /// name wins across extensions; extensions iterate in load order, tools
    /// in registration order.
    pub fn get_all_registered_tools(&self) -> Vec<RegisteredTool> {
        let mut by_name: InsertionMap<RegisteredTool> = InsertionMap::new();
        for ext in &self.extensions {
            for tool in ext.tools().values() {
                if !by_name.contains(&tool.definition.name) {
                    by_name.set(tool.definition.name.clone(), tool.clone());
                }
            }
        }
        by_name.values().cloned().collect()
    }

    /// `getToolDefinition` (runner.ts:460-468).
    pub fn get_tool_definition(&self, tool_name: &str) -> Option<crate::types::ToolDefinition> {
        for ext in &self.extensions {
            if let Some(tool) = ext.tools().get(tool_name) {
                return Some(tool.definition.clone());
            }
        }
        None
    }

    /// `getFlags` (runner.ts:470-480): first registration per name wins.
    pub fn get_flags(&self) -> InsertionMap<ExtensionFlag> {
        let mut all = InsertionMap::new();
        for ext in &self.extensions {
            for (name, flag) in ext.flags().iter() {
                if !all.contains(name) {
                    all.set(name.clone(), flag.clone());
                }
            }
        }
        all
    }

    /// `getShortcuts` (runner.ts:490-533): last registration wins with
    /// diagnostics; reserved built-in keys skip the extension shortcut.
    ///
    /// `builtin_keybindings` is the resolved keybindings config as
    /// `(action_id, keys)` pairs (`KeybindingsConfig`, keys already split
    /// into lists); keys are normalized to lowercase like runner.ts:99.
    pub fn get_shortcuts(
        &self,
        builtin_keybindings: &[(String, Vec<String>)],
    ) -> InsertionMap<ExtensionShortcut> {
        *write(&self.shortcut_diagnostics) = Vec::new();
        let builtins = build_builtin_keybindings(builtin_keybindings);
        let mut extension_shortcuts: InsertionMap<ExtensionShortcut> = InsertionMap::new();

        let add_diagnostic = |message: String, extension_path: &str| {
            // Upstream also `console.warn`s when there is no UI
            // (runner.ts:497-499); tracing is the Rust equivalent.
            if !self.runtime.has_ui() {
                tracing::warn!("{message}");
            }
            write(&self.shortcut_diagnostics).push(HostDiagnostic {
                kind: DiagnosticKind::Warning,
                message,
                path: Some(extension_path.to_owned()),
            });
        };

        for ext in &self.extensions {
            for (_, shortcut) in ext.shortcuts().iter() {
                let normalized = shortcut.shortcut.to_lowercase();
                if let Some(builtin) = builtins.get(&normalized) {
                    if builtin.restrict_override {
                        add_diagnostic(
                            format!(
                                "Extension shortcut '{}' from {} conflicts with built-in shortcut. Skipping.",
                                shortcut.shortcut, shortcut.extension_path
                            ),
                            &shortcut.extension_path,
                        );
                        continue;
                    }
                    add_diagnostic(
                        format!(
                            "Extension shortcut conflict: '{}' is built-in shortcut for {} and {}. Using {}.",
                            shortcut.shortcut,
                            builtin_keybindings[builtin.keybinding_idx].0,
                            shortcut.extension_path,
                            shortcut.extension_path
                        ),
                        &shortcut.extension_path,
                    );
                }

                if let Some(existing) = extension_shortcuts.get(&normalized) {
                    add_diagnostic(
                        format!(
                            "Extension shortcut conflict: '{}' registered by both {} and {}. Using {}.",
                            shortcut.shortcut,
                            existing.extension_path,
                            shortcut.extension_path,
                            shortcut.extension_path
                        ),
                        &shortcut.extension_path,
                    );
                }
                extension_shortcuts.set(normalized, shortcut.clone());
            }
        }
        extension_shortcuts
    }

    /// `getShortcutDiagnostics` (runner.ts:535-537).
    pub fn get_shortcut_diagnostics(&self) -> Vec<HostDiagnostic> {
        read(&self.shortcut_diagnostics).clone()
    }

    /// `resolveRegisteredCommands` (runner.ts:595-629): all commands are
    /// kept; names registered more than once get `:N` invocation suffixes,
    /// bumped past any taken name.
    fn resolve_registered_commands(&self) -> Vec<ResolvedCommand> {
        let mut commands: Vec<RegisteredCommand> = Vec::new();
        let mut counts: HashMap<String, usize> = HashMap::new();
        for ext in &self.extensions {
            for command in ext.commands().values() {
                *counts.entry(command.name.clone()).or_insert(0) += 1;
                commands.push(command.clone());
            }
        }

        let mut seen: HashMap<String, usize> = HashMap::new();
        let mut taken: HashSet<String> = HashSet::new();

        commands
            .into_iter()
            .map(|command| {
                let occurrence = seen.get(&command.name).map_or(1, |n| n + 1);
                seen.insert(command.name.clone(), occurrence);

                let mut invocation_name = if counts.get(&command.name).copied().unwrap_or(0) > 1 {
                    format!("{}:{}", command.name, occurrence)
                } else {
                    command.name.clone()
                };

                if taken.contains(&invocation_name) {
                    let mut suffix = occurrence;
                    loop {
                        suffix += 1;
                        invocation_name = format!("{}:{}", command.name, suffix);
                        if !taken.contains(&invocation_name) {
                            break;
                        }
                    }
                }

                taken.insert(invocation_name.clone());
                ResolvedCommand {
                    name: command.name,
                    invocation_name,
                    source_info: command.source_info,
                    description: command.description,
                    get_argument_completions: command.get_argument_completions,
                    handler: command.handler,
                }
            })
            .collect()
    }

    /// `getRegisteredCommands` (runner.ts:635-638).
    pub fn get_registered_commands(&self) -> Vec<ResolvedCommand> {
        *write(&self.command_diagnostics) = Vec::new();
        self.resolve_registered_commands()
    }

    /// `getCommandDiagnostics` (runner.ts:640-642).
    pub fn get_command_diagnostics(&self) -> Vec<HostDiagnostic> {
        read(&self.command_diagnostics).clone()
    }

    /// `getCommand(name)` (runner.ts:644-646): lookup by invocation name.
    pub fn get_command(&self, name: &str) -> Option<ResolvedCommand> {
        self.resolve_registered_commands()
            .into_iter()
            .find(|command| command.invocation_name == name)
    }

    /// `getMessageRenderer` (runner.ts:575-583): first registration per
    /// custom type wins, silently.
    pub fn get_message_renderer(&self, custom_type: &str) -> Option<crate::types::MessageRenderFn> {
        self.extensions
            .iter()
            .find_map(|ext| ext.message_renderer(custom_type))
    }

    /// `getMarkdownTransformers` (runner.ts:589-591 @ 4181f66): one
    /// transformer per extension, flattened in load order; extensions
    /// without a registered transformer contribute nothing.
    pub fn get_markdown_transformers(&self) -> Vec<crate::types::MarkdownTransformerFn> {
        self.extensions
            .iter()
            .filter_map(|ext| ext.markdown_transformer())
            .collect()
    }

    /// `getEntryRenderer` (runner.ts:585-593): first registration wins.
    pub fn get_entry_renderer(&self, custom_type: &str) -> Option<crate::types::EntryRenderFn> {
        self.extensions
            .iter()
            .find_map(|ext| ext.entry_renderer(custom_type))
    }

    /// `detectExtensionConflicts` (resource-loader.ts:1003-1038): tool and
    /// flag name conflicts across extensions; all extensions stay loaded.
    pub fn detect_extension_conflicts(&self) -> Vec<HostDiagnostic> {
        let mut conflicts = Vec::new();
        let mut tool_owners: HashMap<String, String> = HashMap::new();
        let mut flag_owners: HashMap<String, String> = HashMap::new();

        for ext in &self.extensions {
            for tool in ext.tools().values() {
                let name = &tool.definition.name;
                match tool_owners.get(name) {
                    Some(owner) if *owner != ext.path => conflicts.push(HostDiagnostic {
                        kind: DiagnosticKind::Warning,
                        message: format!("Tool \"{name}\" conflicts with {owner}"),
                        path: Some(ext.path.clone()),
                    }),
                    _ => {
                        tool_owners.insert(name.clone(), ext.path.clone());
                    }
                }
            }
            for (name, _) in ext.flags().iter() {
                match flag_owners.get(name) {
                    Some(owner) if *owner != ext.path => conflicts.push(HostDiagnostic {
                        kind: DiagnosticKind::Warning,
                        message: format!("Flag \"--{name}\" conflicts with {owner}"),
                        path: Some(ext.path.clone()),
                    }),
                    _ => {
                        flag_owners.insert(name.clone(), ext.path.clone());
                    }
                }
            }
        }
        conflicts
    }

    // ========================================================================
    // Emit dispatch (runner.ts:788-1222)
    // ========================================================================

    /// Generic `emit` (runner.ts:788-820): serial dispatch; handler errors
    /// are collected via `emitError` and dispatch continues; for
    /// `session_before_*` events the last non-null result is returned and
    /// `cancel: true` short-circuits immediately.
    pub async fn emit(&self, event_type: &str, payload: Value) -> Option<Value> {
        let session_before = is_session_before_event(event_type);
        let mut payload = payload;
        // Every upstream event object carries its `type` tag; stamp it so
        // callers may pass a bare payload.
        crate::types::stamp_event_type(&mut payload, event_type);
        let mut result: Option<Value> = None;

        for (path, handler) in self.handlers_for(event_type) {
            let ctx = self.create_context();
            match handler(payload.clone(), ctx).await {
                Ok(handler_result) => {
                    if session_before && !handler_result.is_null() {
                        let cancel = handler_result
                            .get("cancel")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        result = Some(handler_result);
                        if cancel {
                            return result;
                        }
                    }
                }
                Err(error) => self.emit_error(ExtensionError::new(&path, event_type, error)),
            }
        }

        result
    }

    /// `emitMessageEnd` (runner.ts:822-862): chained message replacement;
    /// a replacement must keep the original role, otherwise an error is
    /// emitted and the replacement is skipped.
    pub async fn emit_message_end(&self, payload: Value) -> Option<Value> {
        let mut current_message = payload.get("message").cloned().unwrap_or(Value::Null);
        let mut modified = false;

        for (path, handler) in self.handlers_for(EVENT_MESSAGE_END) {
            let ctx = self.create_context();
            let mut event = payload.clone();
            if let Value::Object(map) = &mut event {
                map.insert("message".to_owned(), current_message.clone());
            }
            match handler(event, ctx).await {
                Ok(handler_result) => {
                    let replacement = handler_result.get("message").cloned();
                    let Some(replacement) = replacement.filter(|m| !m.is_null()) else {
                        continue;
                    };
                    // runner.ts:837-844 role check.
                    let current_role = current_message.get("role").and_then(Value::as_str);
                    let replacement_role = replacement.get("role").and_then(Value::as_str);
                    if replacement_role != current_role {
                        self.emit_error(ExtensionError::new(
                            &path,
                            EVENT_MESSAGE_END,
                            "message_end handlers must return a message with the same role"
                                .to_owned(),
                        ));
                        continue;
                    }
                    current_message = replacement;
                    modified = true;
                }
                Err(error) => {
                    self.emit_error(ExtensionError::new(&path, EVENT_MESSAGE_END, error));
                }
            }
        }

        modified.then_some(current_message)
    }

    /// `emitToolResult` (runner.ts:864-917): partial-patch chaining — each
    /// non-undefined result field replaces the corresponding event field and
    /// later handlers observe earlier patches.
    pub async fn emit_tool_result(&self, payload: Value) -> Option<Value> {
        let mut current_event = payload.clone();
        let mut modified = false;

        for (path, handler) in self.handlers_for(EVENT_TOOL_RESULT) {
            let ctx = self.create_context();
            match handler(current_event.clone(), ctx).await {
                Ok(handler_result) => {
                    if handler_result.is_null() {
                        continue;
                    }
                    // camelCase patch keys (types.ts:1079-1084).
                    for key in ["content", "details", "isError", "usage"] {
                        if let Some(value) = handler_result.get(key) {
                            if !value.is_null() {
                                if let Value::Object(map) = &mut current_event {
                                    map.insert(key.to_owned(), value.clone());
                                }
                                modified = true;
                            }
                        }
                    }
                }
                Err(error) => {
                    self.emit_error(ExtensionError::new(&path, EVENT_TOOL_RESULT, error));
                }
            }
        }

        if !modified {
            return None;
        }

        // The result carries exactly the four patchable fields
        // (runner.ts:911-916).
        let mut out = Map::new();
        for key in ["content", "details", "isError", "usage"] {
            if let Some(value) = current_event.get(key) {
                out.insert(key.to_owned(), value.clone());
            }
        }
        Some(Value::Object(out))
    }

    /// `emitToolCall` (runner.ts:919-940): last non-null result wins;
    /// `block: true` short-circuits. Intentional parity notes:
    /// - upstream has NO try/catch here — a throwing handler propagates to
    ///   the caller (the sdk fail-safe decides); mirrored by returning
    ///   `Err(ExtensionError)`.
    /// - upstream handlers mutate `event.input` in place and later handlers
    ///   observe the mutation (types.ts:892-896). In-place mutation cannot
    ///   cross the JSON handler boundary, so handlers thread the mutated
    ///   arguments back in an `"input"` field of the result object; the
    ///   dispatch feeds each handler the current input and exposes the final
    ///   input in the returned result's `"input"` key (rpi extension of the
    ///   result shape, needed by both L0 and L1).
    pub async fn emit_tool_call(&self, payload: Value) -> Result<Option<Value>, ExtensionError> {
        let mut current_input = payload.get("input").cloned().unwrap_or(Value::Null);
        let mut input_modified = false;
        let mut result: Option<Value> = None;

        for (path, handler) in self.handlers_for(EVENT_TOOL_CALL) {
            let ctx = self.create_context();
            let mut event = payload.clone();
            if let Value::Object(map) = &mut event {
                map.insert("input".to_owned(), current_input.clone());
            }
            let handler_result = handler(event, ctx)
                .await
                .map_err(|error| ExtensionError::new(&path, EVENT_TOOL_CALL, error))?;
            if !handler_result.is_null() {
                if let Some(input) = handler_result.get("input") {
                    if !input.is_null() {
                        current_input = input.clone();
                        input_modified = true;
                    }
                }
                let block = handler_result
                    .get("block")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                result = Some(handler_result);
                if block {
                    break;
                }
            }
        }

        if input_modified {
            let result = result.get_or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(map) = result {
                map.insert("input".to_owned(), current_input);
            }
        }
        Ok(result)
    }

    /// `emitUserBash` (runner.ts:942-969): first non-null result wins;
    /// errors are isolated per handler.
    pub async fn emit_user_bash(&self, payload: Value) -> Option<Value> {
        for (path, handler) in self.handlers_for(EVENT_USER_BASH) {
            let ctx = self.create_context();
            match handler(payload.clone(), ctx).await {
                Ok(handler_result) => {
                    if !handler_result.is_null() {
                        return Some(handler_result);
                    }
                }
                Err(error) => {
                    self.emit_error(ExtensionError::new(&path, EVENT_USER_BASH, error));
                }
            }
        }
        None
    }

    /// `emitContext` (runner.ts:971-1001): chained `messages` replacement.
    pub async fn emit_context(&self, messages: Value) -> Value {
        let mut current_messages = messages;

        for (path, handler) in self.handlers_for(EVENT_CONTEXT) {
            let ctx = self.create_context();
            let event = serde_json::json!({
                "type": EVENT_CONTEXT,
                "messages": current_messages,
            });
            match handler(event, ctx).await {
                Ok(handler_result) => {
                    if let Some(messages) = handler_result.get("messages") {
                        if !messages.is_null() {
                            current_messages = messages.clone();
                        }
                    }
                }
                Err(error) => {
                    self.emit_error(ExtensionError::new(&path, EVENT_CONTEXT, error));
                }
            }
        }

        current_messages
    }

    /// `emitBeforeProviderRequest` (runner.ts:1003-1035): chained payload
    /// replacement; `undefined` (JSON null) does not replace.
    pub async fn emit_before_provider_request(&self, payload: Value) -> Value {
        let mut current_payload = payload;
        const EVENT: &str = "before_provider_request";

        for (path, handler) in self.handlers_for(EVENT) {
            let ctx = self.create_context();
            let event = serde_json::json!({
                "type": EVENT,
                "payload": current_payload,
            });
            match handler(event, ctx).await {
                Ok(handler_result) => {
                    if !handler_result.is_null() {
                        current_payload = handler_result;
                    }
                }
                Err(error) => {
                    self.emit_error(ExtensionError::new(&path, EVENT, error));
                }
            }
        }

        current_payload
    }

    /// `emitBeforeProviderHeaders` (runner.ts:1037-1063). Deviation (see
    /// header): a non-null object result replaces the headers map; the typed
    /// native wrapper provides upstream's in-place `&mut` semantics.
    pub async fn emit_before_provider_headers(&self, headers: Value) -> Value {
        let mut current_headers = headers;
        const EVENT: &str = "before_provider_headers";

        for (path, handler) in self.handlers_for(EVENT) {
            let ctx = self.create_context();
            let event = serde_json::json!({
                "type": EVENT,
                "headers": current_headers,
            });
            match handler(event, ctx).await {
                Ok(handler_result) => {
                    if handler_result.is_object() {
                        current_headers = handler_result;
                    }
                }
                Err(error) => {
                    self.emit_error(ExtensionError::new(&path, EVENT, error));
                }
            }
        }

        current_headers
    }

    /// `emitBeforeAgentStart` (runner.ts:1068-1132): collects injected
    /// messages; `systemPrompt` replacements chain so each handler observes
    /// the previous handler's prompt.
    pub async fn emit_before_agent_start(&self, payload: Value) -> Option<Value> {
        // `ctx.getSystemPrompt()` returns the current (chained) prompt
        // during this emit (runner.ts:1075-1082).
        let current_prompt_cell = Arc::new(RwLock::new(
            payload
                .get("systemPrompt")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        ));
        let mut current_system_prompt = payload.get("systemPrompt").cloned().unwrap_or(Value::Null);
        let mut messages: Vec<Value> = Vec::new();
        let mut system_prompt_modified = false;

        for (path, handler) in self.handlers_for(EVENT_BEFORE_AGENT_START) {
            let ctx = ExtensionContext::with_system_prompt_override(
                self.runtime.clone(),
                self.cwd.clone(),
                current_prompt_cell.clone(),
            );
            let mut event = payload.clone();
            if let Value::Object(map) = &mut event {
                map.insert("systemPrompt".to_owned(), current_system_prompt.clone());
            }
            match handler(event, ctx).await {
                Ok(handler_result) => {
                    if handler_result.is_null() {
                        continue;
                    }
                    if let Some(message) = handler_result.get("message") {
                        if !message.is_null() {
                            messages.push(message.clone());
                        }
                    }
                    if let Some(system_prompt) = handler_result.get("systemPrompt") {
                        if !system_prompt.is_null() {
                            current_system_prompt = system_prompt.clone();
                            if let Some(text) = system_prompt.as_str() {
                                *write(&current_prompt_cell) = text.to_owned();
                            }
                            system_prompt_modified = true;
                        }
                    }
                }
                Err(error) => {
                    self.emit_error(ExtensionError::new(&path, EVENT_BEFORE_AGENT_START, error));
                }
            }
        }

        if messages.is_empty() && !system_prompt_modified {
            return None;
        }

        let mut out = Map::new();
        if !messages.is_empty() {
            out.insert("messages".to_owned(), Value::Array(messages));
        }
        if system_prompt_modified {
            out.insert("systemPrompt".to_owned(), current_system_prompt);
        }
        Some(Value::Object(out))
    }

    /// `emitResourcesDiscover` (runner.ts:1134-1180): path lists tagged with
    /// the contributing extension path.
    pub async fn emit_resources_discover(&self, payload: Value) -> Value {
        const EVENT: &str = "resources_discover";
        let mut skill_paths: Vec<Value> = Vec::new();
        let mut prompt_paths: Vec<Value> = Vec::new();
        let mut theme_paths: Vec<Value> = Vec::new();

        for (path, handler) in self.handlers_for(EVENT) {
            let ctx = self.create_context();
            match handler(payload.clone(), ctx).await {
                Ok(handler_result) => {
                    for (key, target) in [
                        ("skillPaths", &mut skill_paths),
                        ("promptPaths", &mut prompt_paths),
                        ("themePaths", &mut theme_paths),
                    ] {
                        if let Some(Value::Array(paths)) = handler_result.get(key) {
                            for p in paths {
                                target.push(serde_json::json!({
                                    "path": p,
                                    "extensionPath": path,
                                }));
                            }
                        }
                    }
                }
                Err(error) => {
                    self.emit_error(ExtensionError::new(&path, EVENT, error));
                }
            }
        }

        serde_json::json!({
            "skillPaths": skill_paths,
            "promptPaths": prompt_paths,
            "themePaths": theme_paths,
        })
    }

    /// `emitInput` (runner.ts:1182-1222): transforms chain (each handler
    /// observes the previous transform), the first `"handled"` result
    /// short-circuits; unchanged input reports `"continue"`.
    pub async fn emit_input(&self, payload: Value) -> Value {
        let original_text = payload.get("text").cloned().unwrap_or(Value::Null);
        let original_images = payload.get("images").cloned();
        let mut current_text = original_text.clone();
        let mut current_images = original_images.clone();

        for (path, handler) in self.handlers_for(EVENT_INPUT) {
            let ctx = self.create_context();
            let mut event = payload.clone();
            if let Value::Object(map) = &mut event {
                map.insert("text".to_owned(), current_text.clone());
                match &current_images {
                    Some(images) => {
                        map.insert("images".to_owned(), images.clone());
                    }
                    None => {
                        map.remove("images");
                    }
                }
            }
            match handler(event, ctx).await {
                Ok(handler_result) => {
                    match handler_result.get("action").and_then(Value::as_str) {
                        Some("handled") => return handler_result,
                        Some("transform") => {
                            if let Some(text) = handler_result.get("text") {
                                current_text = text.clone();
                            }
                            // `result.images ?? currentImages` (runner.ts:1207).
                            if let Some(images) = handler_result.get("images") {
                                if !images.is_null() {
                                    current_images = Some(images.clone());
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Err(error) => {
                    self.emit_error(ExtensionError::new(&path, EVENT_INPUT, error));
                }
            }
        }

        if current_text != original_text || current_images != original_images {
            let mut out = Map::new();
            out.insert("action".to_owned(), Value::String("transform".to_owned()));
            out.insert("text".to_owned(), current_text);
            if let Some(images) = current_images {
                out.insert("images".to_owned(), images);
            }
            Value::Object(out)
        } else {
            serde_json::json!({ "action": "continue" })
        }
    }

    /// `emitProjectTrustEvent` (runner.ts:201-231): handlers run in order,
    /// `"undecided"` falls through, the first yes/no wins. Errors are
    /// collected and returned (upstream does NOT route them through
    /// `emitError` here).
    pub async fn emit_project_trust(&self, payload: Value) -> (Option<Value>, Vec<ExtensionError>) {
        const EVENT: &str = "project_trust";
        let mut errors = Vec::new();

        for (path, handler) in self.handlers_for(EVENT) {
            let ctx = self.create_context();
            match handler(payload.clone(), ctx).await {
                Ok(handler_result) => {
                    if handler_result.is_null() {
                        continue;
                    }
                    match handler_result.get("trusted").and_then(Value::as_str) {
                        Some("undecided") | None => continue,
                        _ => return (Some(handler_result), errors),
                    }
                }
                Err(error) => errors.push(ExtensionError::new(&path, EVENT, error)),
            }
        }

        (None, errors)
    }
}

/// `buildBuiltinKeybindings` (runner.ts:92-111): normalized key → entry;
/// when several actions bind the same key, the reserved action wins.
fn build_builtin_keybindings(
    resolved_keybindings: &[(String, Vec<String>)],
) -> HashMap<String, BuiltinKeybinding> {
    let mut builtins: HashMap<String, BuiltinKeybinding> = HashMap::new();
    for (idx, (keybinding, keys)) in resolved_keybindings.iter().enumerate() {
        let restrict_override =
            RESERVED_KEYBINDINGS_FOR_EXTENSION_CONFLICTS.contains(&keybinding.as_str());
        for key in keys {
            let normalized = key.to_lowercase();
            // runner.ts:102-106: a reserved binding already recorded beats a
            // non-reserved one for the same key.
            if let Some(existing) = builtins.get(&normalized) {
                if existing.restrict_override && !restrict_override {
                    continue;
                }
            }
            builtins.insert(
                normalized,
                BuiltinKeybinding {
                    keybinding_idx: idx,
                    restrict_override,
                },
            );
        }
    }
    builtins
}
