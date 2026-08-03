//! Port of the keybindings system from
//! `packages/coding-agent/src/core/keybindings.ts` @ pi 0.82.1 (2efa728)
//! and `packages/tui/src/keybindings.ts`.
//!
//! Provides the full keybinding definitions table (73 namespace ids), the
//! legacy-name migration table (59 entries), config-file loading with
//! migration, conflict detection, and the [`KeybindingsManager`] that merges
//! defaults with user overrides.
//!
//! Intentional differences:
//! - `matchesKey` (Kitty keyboard protocol parser from `tui/src/keys.ts`) is
//!   not ported — it depends on terminal I/O and lands in T12. The
//!   [`KeybindingsManager::matches`] method is therefore not implemented;
//!   callers compare resolved key lists directly until T12 wires the parser.
//! - Platform-dependent defaults use `cfg!(target_os = …)` (compile-time).
//!   Upstream checks `process.platform` at runtime; the observable behaviour
//!   is identical for a given target.
//! - The global singleton (`globalKeybindings` / `setKeybindings` /
//!   `getKeybindings`) is not ported — the TUI runtime (T12) owns lifecycle.
//! - On-disk migration (`migrateKeybindingsConfigFile` in `migrations.ts`)
//!   is not ported — the resource loader (T17+) performs disk writes.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::config;
use crate::error::PirError;

// ===========================================================================
// Types
// ===========================================================================

/// A keybinding value: single key or array of keys.
///
/// JSON serialises as `string` or `string[]` (matching upstream
/// `KeyId | KeyId[]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyBindingValue {
    Single(String),
    Multiple(Vec<String>),
}

impl KeyBindingValue {
    /// Convert to a `Vec<String>`.
    pub fn to_vec(&self) -> Vec<String> {
        match self {
            KeyBindingValue::Single(k) => vec![k.clone()],
            KeyBindingValue::Multiple(ks) => ks.clone(),
        }
    }

    /// Whether this binding has zero keys (empty array).
    pub fn is_empty(&self) -> bool {
        match self {
            KeyBindingValue::Single(_) => false,
            KeyBindingValue::Multiple(ks) => ks.is_empty(),
        }
    }
}

impl serde::Serialize for KeyBindingValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            KeyBindingValue::Single(k) => serializer.serialize_str(k),
            KeyBindingValue::Multiple(ks) => ks.serialize(serializer),
        }
    }
}

/// A keybinding definition (upstream `KeybindingDefinition`).
#[derive(Debug, Clone)]
pub struct KeybindingDefinition {
    pub default_keys: KeyBindingValue,
    pub description: &'static str,
}

/// A detected conflict between user bindings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingConflict {
    pub key: String,
    pub keybindings: Vec<String>,
}

// ===========================================================================
// Legacy-name migration table (59 entries, keybindings.ts:209-269)
// ===========================================================================

/// Full legacy keybinding name → namespaced id mapping.
pub const KEYBINDING_NAME_MIGRATIONS: &[(&str, &str)] = &[
    // tui.editor.* (21)
    ("cursorUp", "tui.editor.cursorUp"),
    ("cursorDown", "tui.editor.cursorDown"),
    ("cursorLeft", "tui.editor.cursorLeft"),
    ("cursorRight", "tui.editor.cursorRight"),
    ("cursorWordLeft", "tui.editor.cursorWordLeft"),
    ("cursorWordRight", "tui.editor.cursorWordRight"),
    ("cursorLineStart", "tui.editor.cursorLineStart"),
    ("cursorLineEnd", "tui.editor.cursorLineEnd"),
    ("jumpForward", "tui.editor.jumpForward"),
    ("jumpBackward", "tui.editor.jumpBackward"),
    ("pageUp", "tui.editor.pageUp"),
    ("pageDown", "tui.editor.pageDown"),
    ("deleteCharBackward", "tui.editor.deleteCharBackward"),
    ("deleteCharForward", "tui.editor.deleteCharForward"),
    ("deleteWordBackward", "tui.editor.deleteWordBackward"),
    ("deleteWordForward", "tui.editor.deleteWordForward"),
    ("deleteToLineStart", "tui.editor.deleteToLineStart"),
    ("deleteToLineEnd", "tui.editor.deleteToLineEnd"),
    ("yank", "tui.editor.yank"),
    ("yankPop", "tui.editor.yankPop"),
    ("undo", "tui.editor.undo"),
    // tui.input.* (4)
    ("newLine", "tui.input.newLine"),
    ("submit", "tui.input.submit"),
    ("tab", "tui.input.tab"),
    ("copy", "tui.input.copy"),
    // tui.select.* (6)
    ("selectUp", "tui.select.up"),
    ("selectDown", "tui.select.down"),
    ("selectPageUp", "tui.select.pageUp"),
    ("selectPageDown", "tui.select.pageDown"),
    ("selectConfirm", "tui.select.confirm"),
    ("selectCancel", "tui.select.cancel"),
    // app.* (28)
    ("interrupt", "app.interrupt"),
    ("clear", "app.clear"),
    ("exit", "app.exit"),
    ("suspend", "app.suspend"),
    ("cycleThinkingLevel", "app.thinking.cycle"),
    ("cycleModelForward", "app.model.cycleForward"),
    ("cycleModelBackward", "app.model.cycleBackward"),
    ("selectModel", "app.model.select"),
    ("expandTools", "app.tools.expand"),
    ("toggleThinking", "app.thinking.toggle"),
    ("toggleSessionNamedFilter", "app.session.toggleNamedFilter"),
    ("externalEditor", "app.editor.external"),
    ("followUp", "app.message.followUp"),
    ("dequeue", "app.message.dequeue"),
    ("pasteImage", "app.clipboard.pasteImage"),
    ("newSession", "app.session.new"),
    ("tree", "app.session.tree"),
    ("fork", "app.session.fork"),
    ("resume", "app.session.resume"),
    ("treeFoldOrUp", "app.tree.foldOrUp"),
    ("treeUnfoldOrDown", "app.tree.unfoldOrDown"),
    ("treeEditLabel", "app.tree.editLabel"),
    ("treeToggleLabelTimestamp", "app.tree.toggleLabelTimestamp"),
    ("toggleSessionPath", "app.session.togglePath"),
    ("toggleSessionSort", "app.session.toggleSort"),
    ("renameSession", "app.session.rename"),
    ("deleteSession", "app.session.delete"),
    ("deleteSessionNoninvasive", "app.session.deleteNoninvasive"),
];

/// Check whether `key` is a legacy keybinding name.
pub fn is_legacy_keybinding_name(key: &str) -> bool {
    KEYBINDING_NAME_MIGRATIONS
        .iter()
        .any(|(old, _)| *old == key)
}

/// Migrate a single key name. Returns the new name if legacy, else the
/// original.
pub fn migrate_key_name(key: &str) -> &str {
    for (old, new) in KEYBINDING_NAME_MIGRATIONS {
        if *old == key {
            return new;
        }
    }
    key
}

// ===========================================================================
// Keybinding Definitions (73 = 31 tui.* + 42 app.*)
// ===========================================================================

static DEFINITIONS: OnceLock<Vec<(String, KeybindingDefinition)>> = OnceLock::new();

/// Single key shortcut helper.
fn s(key: &'static str) -> KeyBindingValue {
    KeyBindingValue::Single(key.to_string())
}

/// Multiple keys shortcut helper.
fn m(keys: &[&'static str]) -> KeyBindingValue {
    KeyBindingValue::Multiple(keys.iter().map(|k| k.to_string()).collect())
}

/// Build the full definitions table (73 entries, platform-specific defaults).
///
/// Order matches upstream `KEYBINDINGS` definition order (tui/src/keybindings.ts:54-134
/// + coding-agent/src/core/keybindings.ts:64-207).
fn build_definitions() -> Vec<(String, KeybindingDefinition)> {
    vec![
        // ---- tui.editor.* (21) ----
        ("tui.editor.cursorUp", s("up"), "Move cursor up"),
        ("tui.editor.cursorDown", s("down"), "Move cursor down"),
        (
            "tui.editor.cursorLeft",
            m(&["left", "ctrl+b"]),
            "Move cursor left",
        ),
        (
            "tui.editor.cursorRight",
            m(&["right", "ctrl+f"]),
            "Move cursor right",
        ),
        (
            "tui.editor.cursorWordLeft",
            m(&["alt+left", "ctrl+left", "alt+b"]),
            "Move cursor word left",
        ),
        (
            "tui.editor.cursorWordRight",
            m(&["alt+right", "ctrl+right", "alt+f"]),
            "Move cursor word right",
        ),
        (
            "tui.editor.cursorLineStart",
            m(&["home", "ctrl+a"]),
            "Move to line start",
        ),
        (
            "tui.editor.cursorLineEnd",
            m(&["end", "ctrl+e"]),
            "Move to line end",
        ),
        (
            "tui.editor.jumpForward",
            s("ctrl+]"),
            "Jump forward to character",
        ),
        (
            "tui.editor.jumpBackward",
            s("ctrl+alt+]"),
            "Jump backward to character",
        ),
        ("tui.editor.pageUp", s("pageUp"), "Page up"),
        ("tui.editor.pageDown", s("pageDown"), "Page down"),
        (
            "tui.editor.deleteCharBackward",
            s("backspace"),
            "Delete character backward",
        ),
        (
            "tui.editor.deleteCharForward",
            m(&["delete", "ctrl+d"]),
            "Delete character forward",
        ),
        (
            "tui.editor.deleteWordBackward",
            m(&["ctrl+w", "alt+backspace"]),
            "Delete word backward",
        ),
        (
            "tui.editor.deleteWordForward",
            m(&["alt+d", "alt+delete"]),
            "Delete word forward",
        ),
        (
            "tui.editor.deleteToLineStart",
            s("ctrl+u"),
            "Delete to line start",
        ),
        (
            "tui.editor.deleteToLineEnd",
            s("ctrl+k"),
            "Delete to line end",
        ),
        ("tui.editor.yank", s("ctrl+y"), "Yank"),
        ("tui.editor.yankPop", s("alt+y"), "Yank pop"),
        ("tui.editor.undo", s("ctrl+-"), "Undo"),
        // ---- tui.input.* (4) ----
        (
            "tui.input.newLine",
            m(&["shift+enter", "ctrl+j"]),
            "Insert new line",
        ),
        ("tui.input.submit", s("enter"), "Submit input"),
        ("tui.input.tab", s("tab"), "Tab / autocomplete"),
        ("tui.input.copy", s("ctrl+c"), "Copy selection"),
        // ---- tui.select.* (6) ----
        ("tui.select.up", s("up"), "Move selection up"),
        ("tui.select.down", s("down"), "Move selection down"),
        ("tui.select.pageUp", s("pageUp"), "Selection page up"),
        ("tui.select.pageDown", s("pageDown"), "Selection page down"),
        ("tui.select.confirm", s("enter"), "Confirm selection"),
        (
            "tui.select.cancel",
            m(&["escape", "ctrl+c"]),
            "Cancel selection",
        ),
        // ---- app.* (42) ----
        ("app.interrupt", s("escape"), "Cancel or abort"),
        ("app.clear", s("ctrl+c"), "Clear editor"),
        ("app.exit", s("ctrl+d"), "Exit when editor is empty"),
        (
            "app.suspend",
            app_suspend_default(),
            "Suspend to background",
        ),
        ("app.thinking.cycle", s("shift+tab"), "Cycle thinking level"),
        ("app.model.cycleForward", s("ctrl+p"), "Cycle to next model"),
        (
            "app.model.cycleBackward",
            s("shift+ctrl+p"),
            "Cycle to previous model",
        ),
        ("app.model.select", s("ctrl+l"), "Open model selector"),
        ("app.tools.expand", s("ctrl+o"), "Toggle tool output"),
        ("app.thinking.toggle", s("ctrl+t"), "Toggle thinking blocks"),
        (
            "app.session.toggleNamedFilter",
            s("ctrl+n"),
            "Toggle named session filter",
        ),
        ("app.editor.external", s("ctrl+g"), "Open external editor"),
        ("app.message.copy", s("ctrl+x"), "Copy message to clipboard"),
        (
            "app.message.followUp",
            s("alt+enter"),
            "Queue follow-up message",
        ),
        (
            "app.message.dequeue",
            s("alt+up"),
            "Restore queued messages",
        ),
        (
            "app.clipboard.pasteImage",
            paste_image_default(),
            "Paste image from clipboard (text fallback)",
        ),
        ("app.session.new", m(&[]), "Start a new session"),
        ("app.session.tree", m(&[]), "Open session tree"),
        ("app.session.fork", m(&[]), "Fork current session"),
        ("app.session.resume", m(&[]), "Resume a session"),
        (
            "app.tree.foldOrUp",
            tree_fold_or_up_default(),
            "Fold tree branch or move up",
        ),
        (
            "app.tree.unfoldOrDown",
            tree_unfold_or_down_default(),
            "Unfold tree branch or move down",
        ),
        ("app.tree.editLabel", s("shift+l"), "Edit tree label"),
        (
            "app.tree.toggleLabelTimestamp",
            s("shift+t"),
            "Toggle tree label timestamps",
        ),
        (
            "app.session.togglePath",
            s("ctrl+p"),
            "Toggle session path display",
        ),
        (
            "app.session.toggleSort",
            s("ctrl+s"),
            "Toggle session sort mode",
        ),
        ("app.session.rename", s("ctrl+r"), "Rename session"),
        ("app.session.delete", s("ctrl+d"), "Delete session"),
        (
            "app.session.deleteNoninvasive",
            s("ctrl+backspace"),
            "Delete session when query is empty",
        ),
        ("app.models.save", s("ctrl+s"), "Save model selection"),
        ("app.models.enableAll", s("ctrl+a"), "Enable all models"),
        ("app.models.clearAll", s("ctrl+x"), "Clear all models"),
        (
            "app.models.toggleProvider",
            s("ctrl+p"),
            "Toggle all models for provider",
        ),
        (
            "app.models.reorderUp",
            s("alt+up"),
            "Move model up in order",
        ),
        (
            "app.models.reorderDown",
            s("alt+down"),
            "Move model down in order",
        ),
        (
            "app.tree.filter.default",
            s("ctrl+d"),
            "Tree filter: default view",
        ),
        (
            "app.tree.filter.noTools",
            s("ctrl+t"),
            "Tree filter: hide tool results",
        ),
        (
            "app.tree.filter.userOnly",
            s("ctrl+u"),
            "Tree filter: user messages only",
        ),
        (
            "app.tree.filter.labeledOnly",
            s("ctrl+l"),
            "Tree filter: labeled entries only",
        ),
        (
            "app.tree.filter.all",
            s("ctrl+a"),
            "Tree filter: show all entries",
        ),
        (
            "app.tree.filter.cycleForward",
            s("ctrl+o"),
            "Tree filter: cycle forward",
        ),
        (
            "app.tree.filter.cycleBackward",
            s("shift+ctrl+o"),
            "Tree filter: cycle backward",
        ),
    ]
    .into_iter()
    .map(|(id, keys, desc)| {
        (
            id.to_string(),
            KeybindingDefinition {
                default_keys: keys,
                description: desc,
            },
        )
    })
    .collect()
}

/// Get the cached definitions table.
pub fn keybinding_definitions() -> &'static [(String, KeybindingDefinition)] {
    DEFINITIONS.get_or_init(build_definitions)
}

// Platform-specific default values (keybindings.ts:69-72, 111-113, 119-125)

fn app_suspend_default() -> KeyBindingValue {
    if cfg!(target_os = "windows") {
        m(&[])
    } else {
        s("ctrl+z")
    }
}

fn paste_image_default() -> KeyBindingValue {
    if cfg!(target_os = "windows") {
        s("alt+v")
    } else {
        s("ctrl+v")
    }
}

fn tree_fold_or_up_default() -> KeyBindingValue {
    if cfg!(target_os = "macos") {
        m(&["alt+left", "ctrl+left"])
    } else {
        m(&["ctrl+left", "alt+left"])
    }
}

fn tree_unfold_or_down_default() -> KeyBindingValue {
    if cfg!(target_os = "macos") {
        m(&["alt+right", "ctrl+right"])
    } else {
        m(&["ctrl+right", "alt+right"])
    }
}

// ===========================================================================
// Config migration (keybindings.ts:275-327)
// ===========================================================================

/// Convert a raw JSON config map to typed keybinding values, discarding
/// entries that are not string or string[] (keybindings.ts:275-287).
pub fn to_keybindings_config(
    raw: &serde_json::Map<String, serde_json::Value>,
) -> HashMap<String, Vec<String>> {
    let mut config = HashMap::new();
    for (key, binding) in raw {
        match binding {
            serde_json::Value::String(s) => {
                config.insert(key.clone(), vec![s.clone()]);
            }
            serde_json::Value::Array(arr) => {
                let keys: Vec<String> = arr
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect();
                if arr.len() == keys.len() {
                    config.insert(key.clone(), keys);
                }
                // Mixed-type arrays are silently discarded
            }
            _ => {}
        }
    }
    config
}

/// Migrate legacy key names in a raw config (keybindings.ts:289-309).
///
/// Returns `(migrated_config, was_migrated)`. When both an old key and its
/// new name exist in the input, the old key's value is discarded and the new
/// key's value wins.
pub fn migrate_keybindings_config(
    raw_config: &serde_json::Map<String, serde_json::Value>,
) -> (serde_json::Map<String, serde_json::Value>, bool) {
    let mut config = serde_json::Map::new();
    let mut migrated = false;

    for (key, value) in raw_config {
        let next_key = migrate_key_name(key);
        if next_key != key.as_str() {
            migrated = true;
        }
        // Conflict: both old and new key present → discard old key's value
        if key.as_str() != next_key && raw_config.contains_key(next_key) {
            migrated = true;
            continue;
        }
        config.insert(next_key.to_string(), value.clone());
    }

    let ordered = order_keybindings_config(&config);
    (ordered, migrated)
}

/// Reorder config: known keys in definition order, then extras alphabetically
/// (keybindings.ts:311-327).
fn order_keybindings_config(
    config: &serde_json::Map<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut ordered = serde_json::Map::new();

    // Known keys in definition order
    for (id, _) in keybinding_definitions() {
        if let Some(value) = config.get(id) {
            ordered.insert(id.clone(), value.clone());
        }
    }

    // Extras alphabetically
    let mut extras: Vec<&String> = config
        .keys()
        .filter(|k| !ordered.contains_key(k.as_str()))
        .collect();
    extras.sort();
    for key in extras {
        ordered.insert(key.clone(), config[key].clone());
    }

    ordered
}

// ===========================================================================
// Config loading (keybindings.ts:329-367)
// ===========================================================================

/// Read and parse the raw JSON config from disk (keybindings.ts:329-338).
///
/// Returns `None` if the file does not exist or contains invalid JSON / a
/// non-object.
fn load_raw_config(path: &Path) -> Option<serde_json::Map<String, serde_json::Value>> {
    if !path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&content).ok()?;
    parsed.as_object().cloned()
}

/// Load, migrate, and type-filter a keybindings config file
/// (keybindings.ts:363-367).
pub fn load_keybindings_from_file(path: &Path) -> Result<HashMap<String, Vec<String>>, PirError> {
    let raw = load_raw_config(path);
    match raw {
        None => Ok(HashMap::new()),
        Some(raw_map) => {
            let (migrated, _) = migrate_keybindings_config(&raw_map);
            Ok(to_keybindings_config(&migrated))
        }
    }
}

// ===========================================================================
// KeybindingsManager (tui/src/keybindings.ts:155-231 + coding-agent subclass)
// ===========================================================================

/// Manages keybinding definitions, user overrides, resolved keys, and
/// conflicts.
#[derive(Debug, Clone)]
pub struct KeybindingsManager {
    definitions: &'static [(String, KeybindingDefinition)],
    user_bindings: HashMap<String, Vec<String>>,
    keys_by_id: HashMap<String, Vec<String>>,
    conflicts: Vec<KeybindingConflict>,
    config_path: Option<PathBuf>,
}

impl KeybindingsManager {
    /// Create a manager with defaults only (no user overrides).
    pub fn new() -> Self {
        let mut mgr = Self {
            definitions: keybinding_definitions(),
            user_bindings: HashMap::new(),
            keys_by_id: HashMap::new(),
            conflicts: Vec::new(),
            config_path: None,
        };
        mgr.rebuild();
        mgr
    }

    /// Create a manager with user overrides (no config path — no reload).
    pub fn with_user_bindings(user_bindings: HashMap<String, Vec<String>>) -> Self {
        let mut mgr = Self {
            definitions: keybinding_definitions(),
            user_bindings,
            keys_by_id: HashMap::new(),
            conflicts: Vec::new(),
            config_path: None,
        };
        mgr.rebuild();
        mgr
    }

    /// Load from the global keybindings path (`config::get_keybindings_path()`).
    pub fn create() -> Self {
        let config_path = config::get_keybindings_path();
        Self::create_from_path(&config_path)
    }

    /// Load from an explicit config file path (used by tests and custom
    /// agent dirs).
    pub fn create_from_path(config_path: &Path) -> Self {
        let user_bindings = load_keybindings_from_file(config_path).unwrap_or_default();
        let mut mgr = Self {
            definitions: keybinding_definitions(),
            user_bindings,
            keys_by_id: HashMap::new(),
            conflicts: Vec::new(),
            config_path: Some(config_path.to_path_buf()),
        };
        mgr.rebuild();
        mgr
    }

    /// Re-read the config file and rebuild (keybindings.ts:354-357).
    pub fn reload(&mut self) {
        if let Some(path) = &self.config_path {
            if let Ok(user_bindings) = load_keybindings_from_file(path) {
                self.user_bindings = user_bindings;
                self.rebuild();
            }
        }
    }

    /// Replace user bindings and rebuild (keybindings.ts:214-217).
    pub fn set_user_bindings(&mut self, user_bindings: HashMap<String, Vec<String>>) {
        self.user_bindings = user_bindings;
        self.rebuild();
    }

    /// Clone of the raw user bindings (keybindings.ts:219-221).
    pub fn get_user_bindings(&self) -> HashMap<String, Vec<String>> {
        self.user_bindings.clone()
    }

    /// Resolved keys for a keybinding id (keybindings.ts:202-204).
    pub fn get_keys(&self, keybinding: &str) -> Vec<String> {
        self.keys_by_id.get(keybinding).cloned().unwrap_or_default()
    }

    /// Definition for a keybinding id (keybindings.ts:206-208).
    pub fn get_definition(&self, keybinding: &str) -> Option<&KeybindingDefinition> {
        self.definitions
            .iter()
            .find(|(id, _)| id == keybinding)
            .map(|(_, def)| def)
    }

    /// Detected conflicts (keybindings.ts:210-212).
    pub fn get_conflicts(&self) -> Vec<KeybindingConflict> {
        self.conflicts.clone()
    }

    /// All resolved bindings (keybindings.ts:223-230).
    ///
    /// Single key → [`KeyBindingValue::Single`]; zero or 2+ keys →
    /// [`KeyBindingValue::Multiple`].
    pub fn get_resolved_bindings(&self) -> HashMap<String, KeyBindingValue> {
        let mut resolved = HashMap::new();
        for (id, _) in self.definitions {
            let keys = self.keys_by_id.get(id).cloned().unwrap_or_default();
            let value = match keys.as_slice() {
                [single] => KeyBindingValue::Single(single.clone()),
                _ => KeyBindingValue::Multiple(keys),
            };
            resolved.insert(id.clone(), value);
        }
        resolved
    }

    /// Alias for [`Self::get_resolved_bindings`] (keybindings.ts:359-361).
    pub fn get_effective_config(&self) -> HashMap<String, KeyBindingValue> {
        self.get_resolved_bindings()
    }

    /// Total number of definitions.
    pub fn len(&self) -> usize {
        self.definitions.len()
    }

    /// Whether there are no definitions.
    pub fn is_empty(&self) -> bool {
        self.definitions.is_empty()
    }

    /// Rebuild resolved keys and conflicts from definitions + user bindings
    /// (keybindings.ts:167-192).
    fn rebuild(&mut self) {
        self.keys_by_id.clear();
        self.conflicts.clear();

        let known_ids: HashSet<&str> = self.definitions.iter().map(|(id, _)| id.as_str()).collect();

        // Collect user claims per physical key (for conflict detection)
        let mut user_claims: HashMap<String, HashSet<String>> = HashMap::new();
        for (id, keys) in &self.user_bindings {
            if !known_ids.contains(id.as_str()) {
                continue;
            }
            for key in keys {
                user_claims
                    .entry(key.clone())
                    .or_default()
                    .insert(id.clone());
            }
        }

        // Detect conflicts (same physical key claimed by 2+ user bindings)
        for (key, claimants) in &user_claims {
            if claimants.len() > 1 {
                let mut kb: Vec<String> = claimants.iter().cloned().collect();
                kb.sort();
                self.conflicts.push(KeybindingConflict {
                    key: key.clone(),
                    keybindings: kb,
                });
            }
        }

        // Resolve keys per definition: user override if present, else default
        for (id, def) in self.definitions {
            let keys = if let Some(user_keys) = self.user_bindings.get(id) {
                normalize_keys(user_keys)
            } else {
                normalize_value(&def.default_keys)
            };
            self.keys_by_id.insert(id.clone(), keys);
        }
    }
}

impl Default for KeybindingsManager {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// Key normalisation helpers
// ===========================================================================

/// Deduplicate a list of keys, preserving first-occurrence order
/// (tui/src/keybindings.ts:141-153).
fn normalize_keys(keys: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for k in keys {
        if seen.insert(k.clone()) {
            result.push(k.clone());
        }
    }
    result
}

/// Normalise a [`KeyBindingValue`] to a deduplicated `Vec<String>`.
fn normalize_value(value: &KeyBindingValue) -> Vec<String> {
    match value {
        KeyBindingValue::Single(k) => vec![k.clone()],
        KeyBindingValue::Multiple(ks) => normalize_keys(ks),
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- Definitions count ------------------------------------------------

    #[test]
    fn test_definitions_count() {
        let defs = keybinding_definitions();
        assert_eq!(defs.len(), 73, "expected 73 keybinding definitions");
    }

    #[test]
    fn test_migration_table_count() {
        assert_eq!(
            KEYBINDING_NAME_MIGRATIONS.len(),
            59,
            "expected 59 migration entries"
        );
    }

    // --- Specific definition defaults (tui.editor.*) ---------------------

    #[test]
    fn test_tui_editor_cursor_up() {
        let mgr = KeybindingsManager::new();
        let keys = mgr.get_keys("tui.editor.cursorUp");
        assert_eq!(keys, vec!["up"]);
    }

    #[test]
    fn test_tui_editor_cursor_left() {
        let mgr = KeybindingsManager::new();
        let keys = mgr.get_keys("tui.editor.cursorLeft");
        assert_eq!(keys, vec!["left", "ctrl+b"]);
    }

    #[test]
    fn test_tui_editor_cursor_word_left() {
        let mgr = KeybindingsManager::new();
        let keys = mgr.get_keys("tui.editor.cursorWordLeft");
        assert_eq!(keys, vec!["alt+left", "ctrl+left", "alt+b"]);
    }

    #[test]
    fn test_tui_editor_delete_char_forward() {
        let mgr = KeybindingsManager::new();
        let keys = mgr.get_keys("tui.editor.deleteCharForward");
        assert_eq!(keys, vec!["delete", "ctrl+d"]);
    }

    #[test]
    fn test_tui_editor_undo() {
        let mgr = KeybindingsManager::new();
        let keys = mgr.get_keys("tui.editor.undo");
        assert_eq!(keys, vec!["ctrl+-"]);
    }

    // --- Specific definition defaults (tui.input.*, tui.select.*) ---------

    #[test]
    fn test_tui_input_new_line() {
        let mgr = KeybindingsManager::new();
        let keys = mgr.get_keys("tui.input.newLine");
        assert_eq!(keys, vec!["shift+enter", "ctrl+j"]);
    }

    #[test]
    fn test_tui_select_cancel() {
        let mgr = KeybindingsManager::new();
        let keys = mgr.get_keys("tui.select.cancel");
        assert_eq!(keys, vec!["escape", "ctrl+c"]);
    }

    // --- Specific definition defaults (app.*) -----------------------------

    #[test]
    fn test_app_interrupt() {
        let mgr = KeybindingsManager::new();
        assert_eq!(mgr.get_keys("app.interrupt"), vec!["escape"]);
    }

    #[test]
    fn test_app_exit() {
        let mgr = KeybindingsManager::new();
        assert_eq!(mgr.get_keys("app.exit"), vec!["ctrl+d"]);
    }

    #[test]
    fn test_app_thinking_cycle() {
        let mgr = KeybindingsManager::new();
        assert_eq!(mgr.get_keys("app.thinking.cycle"), vec!["shift+tab"]);
    }

    #[test]
    fn test_app_session_new_empty() {
        let mgr = KeybindingsManager::new();
        assert!(mgr.get_keys("app.session.new").is_empty());
    }

    #[test]
    fn test_app_tree_filter_cycle_backward() {
        let mgr = KeybindingsManager::new();
        assert_eq!(
            mgr.get_keys("app.tree.filter.cycleBackward"),
            vec!["shift+ctrl+o"]
        );
    }

    // --- Platform-specific defaults ---------------------------------------

    #[test]
    fn test_app_suspend_default() {
        let mgr = KeybindingsManager::new();
        let keys = mgr.get_keys("app.suspend");
        if cfg!(target_os = "windows") {
            assert!(keys.is_empty(), "Windows should have no suspend default");
        } else {
            assert_eq!(keys, vec!["ctrl+z"]);
        }
    }

    #[test]
    fn test_paste_image_default() {
        let mgr = KeybindingsManager::new();
        let keys = mgr.get_keys("app.clipboard.pasteImage");
        if cfg!(target_os = "windows") {
            assert_eq!(keys, vec!["alt+v"]);
        } else {
            assert_eq!(keys, vec!["ctrl+v"]);
        }
    }

    #[test]
    fn test_tree_fold_or_up_default() {
        let mgr = KeybindingsManager::new();
        let keys = mgr.get_keys("app.tree.foldOrUp");
        if cfg!(target_os = "macos") {
            assert_eq!(keys, vec!["alt+left", "ctrl+left"]);
        } else {
            assert_eq!(keys, vec!["ctrl+left", "alt+left"]);
        }
    }

    // --- Migration --------------------------------------------------------

    #[test]
    fn test_is_legacy_name() {
        assert!(is_legacy_keybinding_name("cursorUp"));
        assert!(is_legacy_keybinding_name("pasteImage"));
        assert!(is_legacy_keybinding_name("deleteSessionNoninvasive"));
        assert!(!is_legacy_keybinding_name("tui.editor.cursorUp"));
        assert!(!is_legacy_keybinding_name("app.message.copy")); // no migration
        assert!(!is_legacy_keybinding_name("unknownAction"));
    }

    #[test]
    fn test_migrate_key_name() {
        assert_eq!(migrate_key_name("cursorUp"), "tui.editor.cursorUp");
        assert_eq!(migrate_key_name("interrupt"), "app.interrupt");
        assert_eq!(
            migrate_key_name("tui.editor.cursorUp"),
            "tui.editor.cursorUp"
        );
        assert_eq!(migrate_key_name("unknown"), "unknown");
    }

    #[test]
    fn test_migrate_config_simple() {
        let mut raw = serde_json::Map::new();
        raw.insert("cursorUp".to_string(), serde_json::json!("ctrl+p"));
        let (migrated, was_migrated) = migrate_keybindings_config(&raw);
        assert!(was_migrated);
        assert_eq!(
            migrated.get("tui.editor.cursorUp"),
            Some(&serde_json::json!("ctrl+p"))
        );
        assert!(!migrated.contains_key("cursorUp"));
    }

    #[test]
    fn test_migrate_config_conflict() {
        // Both old and new key present → old value discarded
        let mut raw = serde_json::Map::new();
        raw.insert("cursorUp".to_string(), serde_json::json!("ctrl+p"));
        raw.insert(
            "tui.editor.cursorUp".to_string(),
            serde_json::json!("ctrl+n"),
        );
        let (migrated, was_migrated) = migrate_keybindings_config(&raw);
        assert!(was_migrated);
        assert_eq!(
            migrated.get("tui.editor.cursorUp"),
            Some(&serde_json::json!("ctrl+n"))
        );
        assert!(!migrated.contains_key("cursorUp"));
    }

    #[test]
    fn test_migrate_config_no_change() {
        let mut raw = serde_json::Map::new();
        raw.insert(
            "tui.editor.cursorUp".to_string(),
            serde_json::json!("ctrl+p"),
        );
        let (migrated, was_migrated) = migrate_keybindings_config(&raw);
        assert!(!was_migrated);
        assert_eq!(
            migrated.get("tui.editor.cursorUp"),
            Some(&serde_json::json!("ctrl+p"))
        );
    }

    // --- Config type filtering --------------------------------------------

    #[test]
    fn test_to_keybindings_config_string() {
        let mut raw = serde_json::Map::new();
        raw.insert("a".to_string(), serde_json::json!("ctrl+c"));
        let config = to_keybindings_config(&raw);
        assert_eq!(config.get("a"), Some(&vec!["ctrl+c".to_string()]));
    }

    #[test]
    fn test_to_keybindings_config_string_array() {
        let mut raw = serde_json::Map::new();
        raw.insert("a".to_string(), serde_json::json!(["ctrl+c", "ctrl+x"]));
        let config = to_keybindings_config(&raw);
        assert_eq!(
            config.get("a"),
            Some(&vec!["ctrl+c".to_string(), "ctrl+x".to_string()])
        );
    }

    #[test]
    fn test_to_keybindings_config_mixed_array_discarded() {
        let mut raw = serde_json::Map::new();
        raw.insert("a".to_string(), serde_json::json!(["ctrl+c", 42]));
        let config = to_keybindings_config(&raw);
        assert!(!config.contains_key("a"));
    }

    #[test]
    fn test_to_keybindings_config_number_discarded() {
        let mut raw = serde_json::Map::new();
        raw.insert("a".to_string(), serde_json::json!(42));
        let config = to_keybindings_config(&raw);
        assert!(!config.contains_key("a"));
    }

    // --- User overrides ---------------------------------------------------

    #[test]
    fn test_user_override_single() {
        let mut user = HashMap::new();
        user.insert(
            "tui.editor.cursorUp".to_string(),
            vec!["ctrl+p".to_string()],
        );
        let mgr = KeybindingsManager::with_user_bindings(user);
        assert_eq!(mgr.get_keys("tui.editor.cursorUp"), vec!["ctrl+p"]);
    }

    #[test]
    fn test_user_override_array() {
        let mut user = HashMap::new();
        user.insert(
            "tui.editor.cursorUp".to_string(),
            vec!["ctrl+p".to_string(), "ctrl+n".to_string()],
        );
        let mgr = KeybindingsManager::with_user_bindings(user);
        assert_eq!(
            mgr.get_keys("tui.editor.cursorUp"),
            vec!["ctrl+p", "ctrl+n"]
        );
    }

    #[test]
    fn test_user_override_unknown_id_ignored() {
        let mut user = HashMap::new();
        user.insert("nonexistent.action".to_string(), vec!["ctrl+x".to_string()]);
        let mgr = KeybindingsManager::with_user_bindings(user);
        // Should not crash, unknown id is simply ignored
        assert!(mgr.get_keys("nonexistent.action").is_empty());
    }

    #[test]
    fn test_user_override_empty_array() {
        let mut user = HashMap::new();
        user.insert("tui.editor.cursorUp".to_string(), vec![]);
        let mgr = KeybindingsManager::with_user_bindings(user);
        assert!(mgr.get_keys("tui.editor.cursorUp").is_empty());
    }

    // --- Conflict detection -----------------------------------------------

    #[test]
    fn test_conflict_detected() {
        let mut user = HashMap::new();
        user.insert(
            "tui.editor.cursorUp".to_string(),
            vec!["ctrl+x".to_string()],
        );
        user.insert(
            "tui.editor.cursorDown".to_string(),
            vec!["ctrl+x".to_string()],
        );
        let mgr = KeybindingsManager::with_user_bindings(user);
        let conflicts = mgr.get_conflicts();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].key, "ctrl+x");
        assert!(conflicts[0]
            .keybindings
            .contains(&"tui.editor.cursorUp".to_string()));
        assert!(conflicts[0]
            .keybindings
            .contains(&"tui.editor.cursorDown".to_string()));
    }

    #[test]
    fn test_no_conflict_for_default_overlap() {
        // Defaults can overlap (e.g. up is used by cursorUp, select.up)
        // but that's not a conflict — only user bindings conflict
        let mgr = KeybindingsManager::new();
        assert!(mgr.get_conflicts().is_empty());
    }

    // --- Resolved bindings ------------------------------------------------

    #[test]
    fn test_get_resolved_bindings_single() {
        let mgr = KeybindingsManager::new();
        let resolved = mgr.get_resolved_bindings();
        let cursor_up = &resolved["tui.editor.cursorUp"];
        assert_eq!(cursor_up, &KeyBindingValue::Single("up".to_string()));
    }

    #[test]
    fn test_get_resolved_bindings_multiple() {
        let mgr = KeybindingsManager::new();
        let resolved = mgr.get_resolved_bindings();
        let cursor_left = &resolved["tui.editor.cursorLeft"];
        assert_eq!(
            cursor_left,
            &KeyBindingValue::Multiple(vec!["left".to_string(), "ctrl+b".to_string()])
        );
    }

    #[test]
    fn test_get_resolved_bindings_empty() {
        let mgr = KeybindingsManager::new();
        let resolved = mgr.get_resolved_bindings();
        let session_new = &resolved["app.session.new"];
        assert_eq!(session_new, &KeyBindingValue::Multiple(vec![]));
    }

    #[test]
    fn test_get_effective_config_equals_resolved() {
        let mgr = KeybindingsManager::new();
        let effective = mgr.get_effective_config();
        let resolved = mgr.get_resolved_bindings();
        assert_eq!(effective.len(), resolved.len());
    }

    // --- Deduplication ----------------------------------------------------

    #[test]
    fn test_normalize_keys_dedup() {
        let input = vec![
            "a".to_string(),
            "b".to_string(),
            "a".to_string(),
            "c".to_string(),
        ];
        let result = normalize_keys(&input);
        assert_eq!(result, vec!["a", "b", "c"]);
    }
}
