//! Port of `packages/tui/src/keybindings.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - `Keybinding` is a Rust enum mirroring the compile-time union
//!   `Keybinding = keyof Keybindings` (keybindings.ts:7-44). The upstream
//!   `Keybindings` interface itself is only a keyof source (every value is
//!   the literal `true`) and has no runtime shape, so no equivalent type
//!   exists; ids are the canonical `tui.*` tokens at runtime
//!   ([`Keybinding::as_str`]), byte-identical to the upstream config.
//! - `KeybindingsConfig` / `KeybindingDefinitions` are insertion-ordered
//!   (a custom map / `Vec`) mirrors of the upstream plain objects, so JSON
//!   object key order round-trips like a JS object. The TS `undefined`
//!   config value has no JSON representation — an absent key is the
//!   equivalent; an explicit empty array unbinds an action.
//! - The global singleton is a replaceable slot
//!   (`RwLock<Option<&'static RwLock<KeybindingsManager>>>`): every
//!   `set_keybindings` installs a fresh instance and the last install wins,
//!   matching upstream's unconditional assignment (`setKeybindings`,
//!   keybindings.ts:235-237). Superseded instances leak by design — the
//!   Rust counterpart of upstream dropping the old reference for GC — one
//!   small allocation per install, bounded by the install sites
//!   (`startup-ui.ts:81`, `interactive-mode.ts:469`, `session-picker.ts:23`).
//! - `KeyBindingValue::deserialize` maps a JSON `null` to an empty key list
//!   and rejects other non-string values; upstream stores `null` verbatim and
//!   crashes with a TypeError when the binding is matched.
//! - `get_definition` returns `Option` (the upstream return type is unsound —
//!   unknown ids yield `undefined` at runtime) and
//!   `KeybindingDefinition::description` is `Option<&'static str>` (upstream
//!   `description?`).
//!
//! Global keybinding registry for the TUI components: a fixed default table
//! of editor/input/select action ids, user overrides (JSON config), conflict
//! detection, and key matching via `keys::matches_key`. Key-id tokens are
//! byte-identical to upstream so `keybindings.json` configs interoperate.

use std::collections::{HashMap, HashSet};
use std::sync::{OnceLock, RwLock};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::keys::matches_key;

// =============================================================================
// Types
// =============================================================================

/// A keybinding value: single key or array of keys (upstream `KeyId | KeyId[]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyBindingValue {
    /// A single key id, e.g. `"enter"`.
    Single(String),
    /// A list of key ids, e.g. `["escape", "ctrl+c"]`.
    Multiple(Vec<String>),
}

impl KeyBindingValue {
    /// Convert to a flat list of key ids.
    pub fn to_vec(&self) -> Vec<String> {
        match self {
            KeyBindingValue::Single(key) => vec![key.clone()],
            KeyBindingValue::Multiple(keys) => keys.clone(),
        }
    }
}

/// A keybinding id (`Keybinding = keyof Keybindings`, keybindings.ts:7-44).
///
/// The upstream union type is compile-time only; the runtime ids are the
/// `tui.*` strings ([`Keybinding::as_str`]), byte-identical to the
/// `keybindings.json` config tokens. Variant order mirrors the `Keybindings`
/// interface declaration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Keybinding {
    // Editor navigation and editing (keybindings.ts:8-29)
    EditorCursorUp,
    EditorCursorDown,
    EditorCursorLeft,
    EditorCursorRight,
    EditorCursorWordLeft,
    EditorCursorWordRight,
    EditorCursorLineStart,
    EditorCursorLineEnd,
    EditorJumpForward,
    EditorJumpBackward,
    EditorPageUp,
    EditorPageDown,
    EditorDeleteCharBackward,
    EditorDeleteCharForward,
    EditorDeleteWordBackward,
    EditorDeleteWordForward,
    EditorDeleteToLineStart,
    EditorDeleteToLineEnd,
    EditorYank,
    EditorYankPop,
    EditorUndo,
    // Generic input actions (keybindings.ts:31-34)
    InputNewLine,
    InputSubmit,
    InputTab,
    InputCopy,
    // Generic selection actions (keybindings.ts:36-41)
    SelectUp,
    SelectDown,
    SelectPageUp,
    SelectPageDown,
    SelectConfirm,
    SelectCancel,
}

impl Keybinding {
    /// All 31 keybinding ids, in interface order (keybindings.ts:7-42).
    pub const ALL: [Keybinding; 31] = [
        Self::EditorCursorUp,
        Self::EditorCursorDown,
        Self::EditorCursorLeft,
        Self::EditorCursorRight,
        Self::EditorCursorWordLeft,
        Self::EditorCursorWordRight,
        Self::EditorCursorLineStart,
        Self::EditorCursorLineEnd,
        Self::EditorJumpForward,
        Self::EditorJumpBackward,
        Self::EditorPageUp,
        Self::EditorPageDown,
        Self::EditorDeleteCharBackward,
        Self::EditorDeleteCharForward,
        Self::EditorDeleteWordBackward,
        Self::EditorDeleteWordForward,
        Self::EditorDeleteToLineStart,
        Self::EditorDeleteToLineEnd,
        Self::EditorYank,
        Self::EditorYankPop,
        Self::EditorUndo,
        Self::InputNewLine,
        Self::InputSubmit,
        Self::InputTab,
        Self::InputCopy,
        Self::SelectUp,
        Self::SelectDown,
        Self::SelectPageUp,
        Self::SelectPageDown,
        Self::SelectConfirm,
        Self::SelectCancel,
    ];

    /// The canonical id string, byte-identical to the upstream config token
    /// (e.g. `tui.editor.cursorUp`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EditorCursorUp => "tui.editor.cursorUp",
            Self::EditorCursorDown => "tui.editor.cursorDown",
            Self::EditorCursorLeft => "tui.editor.cursorLeft",
            Self::EditorCursorRight => "tui.editor.cursorRight",
            Self::EditorCursorWordLeft => "tui.editor.cursorWordLeft",
            Self::EditorCursorWordRight => "tui.editor.cursorWordRight",
            Self::EditorCursorLineStart => "tui.editor.cursorLineStart",
            Self::EditorCursorLineEnd => "tui.editor.cursorLineEnd",
            Self::EditorJumpForward => "tui.editor.jumpForward",
            Self::EditorJumpBackward => "tui.editor.jumpBackward",
            Self::EditorPageUp => "tui.editor.pageUp",
            Self::EditorPageDown => "tui.editor.pageDown",
            Self::EditorDeleteCharBackward => "tui.editor.deleteCharBackward",
            Self::EditorDeleteCharForward => "tui.editor.deleteCharForward",
            Self::EditorDeleteWordBackward => "tui.editor.deleteWordBackward",
            Self::EditorDeleteWordForward => "tui.editor.deleteWordForward",
            Self::EditorDeleteToLineStart => "tui.editor.deleteToLineStart",
            Self::EditorDeleteToLineEnd => "tui.editor.deleteToLineEnd",
            Self::EditorYank => "tui.editor.yank",
            Self::EditorYankPop => "tui.editor.yankPop",
            Self::EditorUndo => "tui.editor.undo",
            Self::InputNewLine => "tui.input.newLine",
            Self::InputSubmit => "tui.input.submit",
            Self::InputTab => "tui.input.tab",
            Self::InputCopy => "tui.input.copy",
            Self::SelectUp => "tui.select.up",
            Self::SelectDown => "tui.select.down",
            Self::SelectPageUp => "tui.select.pageUp",
            Self::SelectPageDown => "tui.select.pageDown",
            Self::SelectConfirm => "tui.select.confirm",
            Self::SelectCancel => "tui.select.cancel",
        }
    }

    /// Parse a canonical id string. Returns `None` for ids outside the tui
    /// table (e.g. the `app.*` ids downstream packages inject via declaration
    /// merging — those are handled by the app-side manager, T09).
    pub fn try_from_str(id: &str) -> Option<Keybinding> {
        Some(match id {
            "tui.editor.cursorUp" => Self::EditorCursorUp,
            "tui.editor.cursorDown" => Self::EditorCursorDown,
            "tui.editor.cursorLeft" => Self::EditorCursorLeft,
            "tui.editor.cursorRight" => Self::EditorCursorRight,
            "tui.editor.cursorWordLeft" => Self::EditorCursorWordLeft,
            "tui.editor.cursorWordRight" => Self::EditorCursorWordRight,
            "tui.editor.cursorLineStart" => Self::EditorCursorLineStart,
            "tui.editor.cursorLineEnd" => Self::EditorCursorLineEnd,
            "tui.editor.jumpForward" => Self::EditorJumpForward,
            "tui.editor.jumpBackward" => Self::EditorJumpBackward,
            "tui.editor.pageUp" => Self::EditorPageUp,
            "tui.editor.pageDown" => Self::EditorPageDown,
            "tui.editor.deleteCharBackward" => Self::EditorDeleteCharBackward,
            "tui.editor.deleteCharForward" => Self::EditorDeleteCharForward,
            "tui.editor.deleteWordBackward" => Self::EditorDeleteWordBackward,
            "tui.editor.deleteWordForward" => Self::EditorDeleteWordForward,
            "tui.editor.deleteToLineStart" => Self::EditorDeleteToLineStart,
            "tui.editor.deleteToLineEnd" => Self::EditorDeleteToLineEnd,
            "tui.editor.yank" => Self::EditorYank,
            "tui.editor.yankPop" => Self::EditorYankPop,
            "tui.editor.undo" => Self::EditorUndo,
            "tui.input.newLine" => Self::InputNewLine,
            "tui.input.submit" => Self::InputSubmit,
            "tui.input.tab" => Self::InputTab,
            "tui.input.copy" => Self::InputCopy,
            "tui.select.up" => Self::SelectUp,
            "tui.select.down" => Self::SelectDown,
            "tui.select.pageUp" => Self::SelectPageUp,
            "tui.select.pageDown" => Self::SelectPageDown,
            "tui.select.confirm" => Self::SelectConfirm,
            "tui.select.cancel" => Self::SelectCancel,
            _ => return None,
        })
    }
}

/// A keybinding definition (`KeybindingDefinition`, keybindings.ts:46-49).
#[derive(Debug, Clone)]
pub struct KeybindingDefinition {
    /// Default key(s) for this action.
    pub default_keys: KeyBindingValue,
    /// Human-readable description (upstream `description?`).
    pub description: Option<&'static str>,
}

/// A detected conflict between user bindings: two or more user bindings claim
/// the same physical key (`KeybindingConflict`, keybindings.ts:136-139).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingConflict {
    /// The contested key id.
    pub key: String,
    /// Claimant keybinding ids, in user-config insertion order.
    pub keybindings: Vec<String>,
}

/// Keybinding definitions table (`KeybindingDefinitions`, keybindings.ts:51).
/// Insertion order is significant: it is the key order of
/// [`KeybindingsManager::get_resolved_bindings`], mirroring the upstream
/// object literal.
pub type KeybindingDefinitions = Vec<(String, KeybindingDefinition)>;

/// User keybinding config (`KeybindingsConfig`, keybindings.ts:52) — an
/// ordered map of keybinding ids to keys (`KeyId | KeyId[] | undefined`;
/// `undefined` has no JSON representation — an absent key is the
/// equivalent). Insertion order is preserved like a JS object, so JSON key
/// order round-trips; reassigning an existing id keeps its position.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeybindingsConfig {
    entries: Vec<(String, KeyBindingValue)>,
}

impl KeybindingsConfig {
    /// Create an empty config.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace a binding (JS object property assignment).
    pub fn insert(&mut self, id: String, value: KeyBindingValue) {
        if let Some((_, existing)) = self
            .entries
            .iter_mut()
            .find(|(existing_id, _)| *existing_id == id)
        {
            *existing = value;
            return;
        }
        self.entries.push((id, value));
    }

    /// Look up a binding by id.
    pub fn get(&self, id: &str) -> Option<&KeyBindingValue> {
        self.entries
            .iter()
            .find(|(existing_id, _)| existing_id == id)
            .map(|(_, value)| value)
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the config has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate over (id, value) pairs in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &KeyBindingValue)> {
        self.entries.iter().map(|(id, value)| (id, value))
    }
}

impl Serialize for KeybindingsConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.entries.len()))?;
        for (id, value) in &self.entries {
            map.serialize_entry(id, value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for KeybindingsConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct KeybindingsConfigVisitor;

        impl<'de> serde::de::Visitor<'de> for KeybindingsConfigVisitor {
            type Value = KeybindingsConfig;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a map of keybinding ids to keys")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut config = KeybindingsConfig::new();
                while let Some((id, value)) = map.next_entry::<String, KeyBindingValue>()? {
                    config.insert(id, value);
                }
                Ok(config)
            }
        }

        deserializer.deserialize_map(KeybindingsConfigVisitor)
    }
}

impl Serialize for KeyBindingValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            KeyBindingValue::Single(key) => serializer.serialize_str(key),
            KeyBindingValue::Multiple(keys) => keys.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for KeyBindingValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct KeyBindingValueVisitor;

        impl<'de> serde::de::Visitor<'de> for KeyBindingValueVisitor {
            type Value = KeyBindingValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a key id string or an array of key id strings")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(KeyBindingValue::Single(value.to_string()))
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut keys = Vec::new();
                while let Some(key) = seq.next_element::<String>()? {
                    keys.push(key);
                }
                Ok(KeyBindingValue::Multiple(keys))
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                // Upstream stores `null` as a key id and crashes in `matches`
                // (TypeError on `key.split`); treat it as "no keys" instead.
                Ok(KeyBindingValue::Multiple(Vec::new()))
            }
        }

        deserializer.deserialize_any(KeyBindingValueVisitor)
    }
}

// =============================================================================
// Default Keybinding Table
// =============================================================================

fn single(key: &'static str) -> KeyBindingValue {
    KeyBindingValue::Single(key.to_string())
}

fn multiple(keys: &[&'static str]) -> KeyBindingValue {
    KeyBindingValue::Multiple(keys.iter().map(|key| key.to_string()).collect())
}

fn entry(
    id: &'static str,
    default_keys: KeyBindingValue,
    description: &'static str,
) -> (String, KeybindingDefinition) {
    (
        id.to_string(),
        KeybindingDefinition {
            default_keys,
            description: Some(description),
        },
    )
}

static TUI_KEYBINDINGS: OnceLock<Vec<(String, KeybindingDefinition)>> = OnceLock::new();

/// The default keybinding table (`TUI_KEYBINDINGS`, keybindings.ts:54-134) —
/// all 31 `tui.*` keybinding ids with their default keys and descriptions,
/// in upstream definition order.
pub fn tui_keybindings() -> &'static [(String, KeybindingDefinition)] {
    TUI_KEYBINDINGS
        .get_or_init(build_tui_keybindings)
        .as_slice()
}

fn build_tui_keybindings() -> Vec<(String, KeybindingDefinition)> {
    vec![
        // ---- tui.editor.* (21) ----
        entry("tui.editor.cursorUp", single("up"), "Move cursor up"),
        entry("tui.editor.cursorDown", single("down"), "Move cursor down"),
        entry(
            "tui.editor.cursorLeft",
            multiple(&["left", "ctrl+b"]),
            "Move cursor left",
        ),
        entry(
            "tui.editor.cursorRight",
            multiple(&["right", "ctrl+f"]),
            "Move cursor right",
        ),
        entry(
            "tui.editor.cursorWordLeft",
            multiple(&["alt+left", "ctrl+left", "alt+b"]),
            "Move cursor word left",
        ),
        entry(
            "tui.editor.cursorWordRight",
            multiple(&["alt+right", "ctrl+right", "alt+f"]),
            "Move cursor word right",
        ),
        entry(
            "tui.editor.cursorLineStart",
            multiple(&["home", "ctrl+a"]),
            "Move to line start",
        ),
        entry(
            "tui.editor.cursorLineEnd",
            multiple(&["end", "ctrl+e"]),
            "Move to line end",
        ),
        entry(
            "tui.editor.jumpForward",
            single("ctrl+]"),
            "Jump forward to character",
        ),
        entry(
            "tui.editor.jumpBackward",
            single("ctrl+alt+]"),
            "Jump backward to character",
        ),
        entry("tui.editor.pageUp", single("pageUp"), "Page up"),
        entry("tui.editor.pageDown", single("pageDown"), "Page down"),
        entry(
            "tui.editor.deleteCharBackward",
            single("backspace"),
            "Delete character backward",
        ),
        entry(
            "tui.editor.deleteCharForward",
            multiple(&["delete", "ctrl+d"]),
            "Delete character forward",
        ),
        entry(
            "tui.editor.deleteWordBackward",
            multiple(&["ctrl+w", "alt+backspace"]),
            "Delete word backward",
        ),
        entry(
            "tui.editor.deleteWordForward",
            multiple(&["alt+d", "alt+delete"]),
            "Delete word forward",
        ),
        entry(
            "tui.editor.deleteToLineStart",
            single("ctrl+u"),
            "Delete to line start",
        ),
        entry(
            "tui.editor.deleteToLineEnd",
            single("ctrl+k"),
            "Delete to line end",
        ),
        entry("tui.editor.yank", single("ctrl+y"), "Yank"),
        entry("tui.editor.yankPop", single("alt+y"), "Yank pop"),
        entry("tui.editor.undo", single("ctrl+-"), "Undo"),
        // ---- tui.input.* (4) ----
        entry(
            "tui.input.newLine",
            multiple(&["shift+enter", "ctrl+j"]),
            "Insert newline",
        ),
        entry("tui.input.submit", single("enter"), "Submit input"),
        entry("tui.input.tab", single("tab"), "Tab / autocomplete"),
        entry("tui.input.copy", single("ctrl+c"), "Copy selection"),
        // ---- tui.select.* (6) ----
        entry("tui.select.up", single("up"), "Move selection up"),
        entry("tui.select.down", single("down"), "Move selection down"),
        entry("tui.select.pageUp", single("pageUp"), "Selection page up"),
        entry(
            "tui.select.pageDown",
            single("pageDown"),
            "Selection page down",
        ),
        entry("tui.select.confirm", single("enter"), "Confirm selection"),
        entry(
            "tui.select.cancel",
            multiple(&["escape", "ctrl+c"]),
            "Cancel selection",
        ),
    ]
}

// =============================================================================
// KeybindingsManager
// =============================================================================

/// Manages keybinding definitions, user overrides, resolved keys, and
/// conflicts (`KeybindingsManager`, keybindings.ts:155-231).
#[derive(Debug)]
pub struct KeybindingsManager {
    definitions: KeybindingDefinitions,
    user_bindings: KeybindingsConfig,
    keys_by_id: HashMap<String, Vec<String>>,
    conflicts: Vec<KeybindingConflict>,
}

impl KeybindingsManager {
    /// Create a manager with the given definitions and user bindings
    /// (`constructor`, keybindings.ts:161-165). Resolved keys and conflicts
    /// are rebuilt immediately.
    pub fn new(definitions: KeybindingDefinitions, user_bindings: KeybindingsConfig) -> Self {
        let mut manager = Self {
            definitions,
            user_bindings,
            keys_by_id: HashMap::new(),
            conflicts: Vec::new(),
        };
        manager.rebuild();
        manager
    }

    /// Create a manager with the default [`tui_keybindings`] table and no
    /// user overrides — the equivalent of `new KeybindingsManager(TUI_KEYBINDINGS)`
    /// (keybindings.ts:241).
    pub fn with_defaults() -> Self {
        Self::new(tui_keybindings().to_vec(), KeybindingsConfig::new())
    }

    /// Whether `data` matches any key bound to `keybinding`
    /// (`matches`, keybindings.ts:194-200).
    pub fn matches(&self, data: &str, keybinding: Keybinding) -> bool {
        let Some(keys) = self.keys_by_id.get(keybinding.as_str()) else {
            return false;
        };
        keys.iter().any(|key| matches_key(data, key))
    }

    /// Whether `data` matches any key bound to the keybinding id `id`
    /// (string-id form of [`Self::matches`]). App-side ids without a
    /// [`Keybinding`] variant (`app.*`, injected downstream via declaration
    /// merging in upstream) use this — the manager is created with the full
    /// app + tui definitions by the coding-agent startup.
    pub fn matches_id(&self, data: &str, id: &str) -> bool {
        let Some(keys) = self.keys_by_id.get(id) else {
            return false;
        };
        keys.iter().any(|key| matches_key(data, key))
    }

    /// Resolved keys for a keybinding (`getKeys`, keybindings.ts:202-204).
    pub fn get_keys(&self, keybinding: Keybinding) -> Vec<String> {
        self.keys_by_id
            .get(keybinding.as_str())
            .cloned()
            .unwrap_or_default()
    }

    /// Resolved keys for a keybinding id (string-id form of
    /// [`Self::get_keys`]; `app.*` ids use this).
    pub fn get_keys_by_id(&self, id: &str) -> Vec<String> {
        self.keys_by_id.get(id).cloned().unwrap_or_default()
    }

    /// Definition for a keybinding, or `None` when unknown
    /// (`getDefinition`, keybindings.ts:206-208).
    pub fn get_definition(&self, keybinding: Keybinding) -> Option<&KeybindingDefinition> {
        self.definitions
            .iter()
            .find(|(id, _)| id == keybinding.as_str())
            .map(|(_, definition)| definition)
    }

    /// Detected conflicts between user bindings (`getConflicts`,
    /// keybindings.ts:210-212).
    pub fn get_conflicts(&self) -> Vec<KeybindingConflict> {
        self.conflicts.clone()
    }

    /// Replace user bindings and rebuild (`setUserBindings`,
    /// keybindings.ts:214-217).
    pub fn set_user_bindings(&mut self, user_bindings: KeybindingsConfig) {
        self.user_bindings = user_bindings;
        self.rebuild();
    }

    /// Clone of the user bindings (`getUserBindings`, keybindings.ts:219-221).
    pub fn get_user_bindings(&self) -> KeybindingsConfig {
        self.user_bindings.clone()
    }

    /// All resolved bindings, in definition order (`getResolvedBindings`,
    /// keybindings.ts:223-230). Single key → [`KeyBindingValue::Single`];
    /// zero or 2+ keys → [`KeyBindingValue::Multiple`].
    pub fn get_resolved_bindings(&self) -> KeybindingsConfig {
        let mut resolved = KeybindingsConfig::new();
        for (id, _) in &self.definitions {
            let keys = self.keys_by_id.get(id).cloned().unwrap_or_default();
            let value = match keys.as_slice() {
                [single_key] => KeyBindingValue::Single(single_key.clone()),
                _ => KeyBindingValue::Multiple(keys),
            };
            resolved.insert(id.clone(), value);
        }
        resolved
    }

    /// Rebuild resolved keys and conflicts from definitions + user bindings
    /// (`rebuild`, keybindings.ts:167-192).
    fn rebuild(&mut self) {
        self.keys_by_id.clear();
        self.conflicts.clear();

        let known_ids: HashSet<&str> = self.definitions.iter().map(|(id, _)| id.as_str()).collect();

        // Collect user claims per physical key (for conflict detection).
        // Claimant order preserves user-config insertion order (JS Set
        // iteration order).
        let mut user_claims: Vec<(String, Vec<String>)> = Vec::new();
        for (keybinding, keys) in self.user_bindings.iter() {
            if !known_ids.contains(keybinding.as_str()) {
                continue;
            }
            for key in normalize_keys(keys) {
                if let Some((_, claimants)) = user_claims
                    .iter_mut()
                    .find(|(claimed_key, _)| *claimed_key == key)
                {
                    if !claimants.contains(keybinding) {
                        claimants.push(keybinding.clone());
                    }
                } else {
                    user_claims.push((key, vec![keybinding.clone()]));
                }
            }
        }

        // Detect conflicts: the same physical key claimed by 2+ user bindings.
        for (key, keybindings) in &user_claims {
            if keybindings.len() > 1 {
                self.conflicts.push(KeybindingConflict {
                    key: key.clone(),
                    keybindings: keybindings.clone(),
                });
            }
        }

        // Resolve keys per definition: user override if present, else default.
        for (id, definition) in &self.definitions {
            let keys = match self.user_bindings.get(id.as_str()) {
                Some(user_keys) => normalize_keys(user_keys),
                None => normalize_keys(&definition.default_keys),
            };
            self.keys_by_id.insert(id.clone(), keys);
        }
    }
}

impl Default for KeybindingsManager {
    fn default() -> Self {
        Self::with_defaults()
    }
}

/// Deduplicate a key list preserving first-occurrence order
/// (`normalizeKeys`, keybindings.ts:141-153). `undefined` (an absent config
/// key) is handled by the caller before this point.
fn normalize_keys(keys: &KeyBindingValue) -> Vec<String> {
    let key_list: Vec<&str> = match keys {
        KeyBindingValue::Single(key) => vec![key.as_str()],
        KeyBindingValue::Multiple(keys) => keys.iter().map(|key| key.as_str()).collect(),
    };
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for key in key_list {
        if seen.insert(key) {
            result.push(key.to_string());
        }
    }
    result
}

// =============================================================================
// Global Keybinding Registry
// =============================================================================

/// The current global keybinding manager, or `None` until installed. A
/// replaceable slot: the last install wins, mirroring upstream
/// `setKeybindings` (keybindings.ts:235-237). Each replaced instance is
/// leaked by design — the Rust counterpart of upstream dropping the old
/// reference for GC — one small allocation per install, bounded by the
/// install sites (startup / session switches).
static GLOBAL_KEYBINDINGS: RwLock<Option<&'static RwLock<KeybindingsManager>>> = RwLock::new(None);

/// Install the global keybinding manager (`setKeybindings`,
/// keybindings.ts:235-237).
///
/// Unconditional replacement like upstream: every call installs a fresh
/// instance and later installs supersede earlier ones. The superseded
/// instance leaks — the Rust counterpart of upstream dropping the old
/// reference for GC — one small allocation per install, bounded by the
/// install sites (startup / session switches).
pub fn set_keybindings(keybindings: KeybindingsManager) {
    let instance: &'static RwLock<KeybindingsManager> =
        Box::leak(Box::new(RwLock::new(keybindings)));
    *GLOBAL_KEYBINDINGS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(instance);
}

/// The global keybinding manager, lazily created from the default table when
/// not installed (`getKeybindings`, keybindings.ts:239-243).
///
/// Components use the shared registry like upstream
/// `getKeybindings().matches(...)`:
///
/// ```
/// let read = pir_tui::keybindings::get_keybindings()
///     .read()
///     .unwrap_or_else(|poisoned| poisoned.into_inner());
/// ```
pub fn get_keybindings() -> &'static RwLock<KeybindingsManager> {
    let read = GLOBAL_KEYBINDINGS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(instance) = *read {
        return instance;
    }
    drop(read);
    // Not installed yet: lazily create the default instance. Re-check under
    // the write lock in case a concurrent `set_keybindings` won the race.
    let mut write = GLOBAL_KEYBINDINGS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(instance) = *write {
        return instance;
    }
    let instance: &'static RwLock<KeybindingsManager> =
        Box::leak(Box::new(RwLock::new(KeybindingsManager::with_defaults())));
    *write = Some(instance);
    instance
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    /// Serializes tests that install the process-global keybinding registry
    /// (Rust test threads run in parallel).
    static GLOBALS_LOCK: Mutex<()> = Mutex::new(());

    fn lock_globals() -> MutexGuard<'static, ()> {
        GLOBALS_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Key id list helper: `keys(&["enter", "ctrl+x"])` → `Vec<String>`.
    fn keys(key_ids: &[&str]) -> Vec<String> {
        key_ids.iter().map(|key| key.to_string()).collect()
    }

    /// Build a manager over the default table with the given user config
    /// (mirrors `new KeybindingsManager(TUI_KEYBINDINGS, {...})`).
    fn manager_with_config(entries: &[(&str, KeyBindingValue)]) -> KeybindingsManager {
        let mut config = KeybindingsConfig::new();
        for (id, value) in entries {
            config.insert((*id).to_string(), value.clone());
        }
        KeybindingsManager::new(tui_keybindings().to_vec(), config)
    }

    #[test]
    fn matches_id_and_get_keys_by_id_support_arbitrary_ids() {
        // App-side ids have no `Keybinding` variant (declaration merging
        // upstream); the string-id forms resolve them the same way.
        let definitions = vec![
            (
                "app.interrupt".to_string(),
                KeybindingDefinition {
                    default_keys: KeyBindingValue::Single("escape".to_string()),
                    description: None,
                },
            ),
            (
                "app.clear".to_string(),
                KeybindingDefinition {
                    default_keys: KeyBindingValue::Multiple(keys(&["ctrl+c", "ctrl+q"])),
                    description: None,
                },
            ),
        ];
        let manager = KeybindingsManager::new(definitions, KeybindingsConfig::new());
        assert!(manager.matches_id("\u{1b}", "app.interrupt"));
        assert!(!manager.matches_id("\u{3}", "app.interrupt"));
        assert!(manager.matches_id("\u{3}", "app.clear"));
        assert!(manager.matches_id("\u{11}", "app.clear"));
        assert_eq!(
            manager.get_keys_by_id("app.clear"),
            keys(&["ctrl+c", "ctrl+q"])
        );
        // Unknown ids resolve to empty / no match.
        assert!(manager.get_keys_by_id("app.nonexistent").is_empty());
        assert!(!manager.matches_id("\u{1b}", "app.nonexistent"));
    }

    #[test]
    fn binds_ctrl_j_as_a_default_newline_alias() {
        let keybindings = KeybindingsManager::with_defaults();

        assert_eq!(
            keybindings.get_keys(Keybinding::InputNewLine),
            keys(&["shift+enter", "ctrl+j"])
        );
        assert!(keybindings.matches("\n", Keybinding::InputNewLine));
        assert!(keybindings.matches("\x1b[106;5u", Keybinding::InputNewLine));
    }

    #[test]
    fn does_not_evict_selector_confirm_when_input_submit_is_rebound() {
        let keybindings = manager_with_config(&[(
            "tui.input.submit",
            KeyBindingValue::Multiple(keys(&["enter", "ctrl+enter"])),
        )]);

        assert_eq!(
            keybindings.get_keys(Keybinding::InputSubmit),
            keys(&["enter", "ctrl+enter"])
        );
        assert_eq!(
            keybindings.get_keys(Keybinding::SelectConfirm),
            keys(&["enter"])
        );
    }

    #[test]
    fn does_not_evict_cursor_bindings_when_another_action_reuses_the_same_key() {
        let keybindings = manager_with_config(&[(
            "tui.select.up",
            KeyBindingValue::Multiple(keys(&["up", "ctrl+p"])),
        )]);

        assert_eq!(
            keybindings.get_keys(Keybinding::SelectUp),
            keys(&["up", "ctrl+p"])
        );
        assert_eq!(
            keybindings.get_keys(Keybinding::EditorCursorUp),
            keys(&["up"])
        );
    }

    #[test]
    fn still_reports_direct_user_binding_conflicts_without_evicting_defaults() {
        let keybindings = manager_with_config(&[
            (
                "tui.input.submit",
                KeyBindingValue::Single("ctrl+x".to_string()),
            ),
            (
                "tui.select.confirm",
                KeyBindingValue::Single("ctrl+x".to_string()),
            ),
        ]);

        assert_eq!(
            keybindings.get_conflicts(),
            vec![KeybindingConflict {
                key: "ctrl+x".to_string(),
                keybindings: keys(&["tui.input.submit", "tui.select.confirm"]),
            }]
        );
        assert_eq!(
            keybindings.get_keys(Keybinding::EditorCursorLeft),
            keys(&["left", "ctrl+b"])
        );
    }

    #[test]
    fn user_binding_deduplicates_keys_preserving_first_occurrence() {
        let keybindings = manager_with_config(&[(
            "tui.input.submit",
            KeyBindingValue::Multiple(keys(&["enter", "enter", "ctrl+x", "enter"])),
        )]);

        assert_eq!(
            keybindings.get_keys(Keybinding::InputSubmit),
            keys(&["enter", "ctrl+x"])
        );
    }

    #[test]
    fn empty_user_binding_unbinds_the_action() {
        let keybindings =
            manager_with_config(&[("tui.input.submit", KeyBindingValue::Multiple(Vec::new()))]);

        assert!(keybindings.get_keys(Keybinding::InputSubmit).is_empty());
        assert!(!keybindings.matches("\r", Keybinding::InputSubmit));
    }

    #[test]
    fn unknown_user_binding_ids_are_ignored() {
        let keybindings = manager_with_config(&[
            (
                "app.nonexistent",
                KeyBindingValue::Single("ctrl+x".to_string()),
            ),
            (
                "tui.input.submit",
                KeyBindingValue::Single("ctrl+x".to_string()),
            ),
        ]);

        assert_eq!(
            keybindings.get_keys(Keybinding::InputSubmit),
            keys(&["ctrl+x"])
        );
        assert!(keybindings.get_conflicts().is_empty());
    }

    #[test]
    fn user_override_replaces_defaults() {
        let keybindings = manager_with_config(&[(
            "tui.editor.cursorUp",
            KeyBindingValue::Single("ctrl+p".to_string()),
        )]);

        assert_eq!(
            keybindings.get_keys(Keybinding::EditorCursorUp),
            keys(&["ctrl+p"])
        );
    }

    #[test]
    fn matches_any_key_bound_to_the_action() {
        let keybindings = KeybindingsManager::with_defaults();

        assert!(keybindings.matches("\x02", Keybinding::EditorCursorLeft)); // ctrl+b
        assert!(keybindings.matches("\x1b[D", Keybinding::EditorCursorLeft)); // left
        assert!(!keybindings.matches("\x1b[C", Keybinding::EditorCursorLeft)); // right
    }

    #[test]
    fn get_resolved_bindings_uses_single_for_one_key_and_array_otherwise() {
        let keybindings = KeybindingsManager::with_defaults();
        let resolved = keybindings.get_resolved_bindings();

        assert_eq!(resolved.len(), 31);
        assert_eq!(
            resolved.get("tui.editor.cursorUp"),
            Some(&KeyBindingValue::Single("up".to_string()))
        );
        assert_eq!(
            resolved.get("tui.input.newLine"),
            Some(&KeyBindingValue::Multiple(keys(&["shift+enter", "ctrl+j"])))
        );
    }

    #[test]
    fn get_definition_returns_the_definition_with_description() {
        let keybindings = KeybindingsManager::with_defaults();

        let definition = keybindings
            .get_definition(Keybinding::EditorUndo)
            .expect("undo has a definition");
        assert_eq!(
            definition.default_keys,
            KeyBindingValue::Single("ctrl+-".to_string())
        );
        assert_eq!(definition.description, Some("Undo"));
    }

    #[test]
    fn get_user_bindings_round_trips_the_config() {
        let config = {
            let mut config = KeybindingsConfig::new();
            config.insert(
                "tui.input.submit".to_string(),
                KeyBindingValue::Single("ctrl+x".to_string()),
            );
            config.insert(
                "tui.select.up".to_string(),
                KeyBindingValue::Multiple(keys(&["up", "ctrl+p"])),
            );
            config
        };
        let mut keybindings = KeybindingsManager::with_defaults();
        keybindings.set_user_bindings(config.clone());

        assert_eq!(keybindings.get_user_bindings(), config);
    }

    #[test]
    fn keybinding_enum_covers_exactly_the_default_table() {
        let table_ids: HashSet<&str> = tui_keybindings()
            .iter()
            .map(|(id, _)| id.as_str())
            .collect();
        assert_eq!(table_ids.len(), Keybinding::ALL.len());

        for keybinding in Keybinding::ALL {
            assert!(table_ids.contains(keybinding.as_str()));
            assert_eq!(
                Keybinding::try_from_str(keybinding.as_str()),
                Some(keybinding)
            );
        }
        for id in table_ids {
            assert!(
                Keybinding::try_from_str(id).is_some(),
                "no enum variant for {id}"
            );
        }
        // Ids outside the tui table (e.g. app-side ids, T09) parse to None.
        assert_eq!(Keybinding::try_from_str("app.model.cycleForward"), None);
    }

    #[test]
    fn key_binding_value_serializes_as_string_or_array() {
        assert_eq!(
            serde_json::to_string(&KeyBindingValue::Single("enter".to_string())).unwrap(),
            "\"enter\""
        );
        assert_eq!(
            serde_json::to_string(&KeyBindingValue::Multiple(keys(&["escape", "ctrl+c"]))).unwrap(),
            "[\"escape\",\"ctrl+c\"]"
        );
    }

    #[test]
    fn key_binding_value_deserializes_null_to_empty_keys() {
        assert_eq!(
            serde_json::from_str::<KeyBindingValue>("null").unwrap(),
            KeyBindingValue::Multiple(Vec::new())
        );
        assert_eq!(
            serde_json::from_str::<KeyBindingValue>("\"enter\"").unwrap(),
            KeyBindingValue::Single("enter".to_string())
        );
        assert_eq!(
            serde_json::from_str::<KeyBindingValue>("[\"escape\",\"ctrl+c\"]").unwrap(),
            KeyBindingValue::Multiple(keys(&["escape", "ctrl+c"]))
        );
        assert!(serde_json::from_str::<KeyBindingValue>("42").is_err());
    }

    #[test]
    fn global_singleton_lazily_creates_defaults_and_accepts_install() {
        let _globals = lock_globals();
        set_keybindings(KeybindingsManager::with_defaults());

        let read = get_keybindings()
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(
            read.get_keys(Keybinding::InputNewLine),
            keys(&["shift+enter", "ctrl+j"])
        );
        assert_eq!(
            read.get_keys(Keybinding::SelectCancel),
            keys(&["escape", "ctrl+c"])
        );
        assert!(read.matches("\x1b[106;5u", Keybinding::InputNewLine));
    }

    #[test]
    fn global_singleton_last_install_wins() {
        let _globals = lock_globals();
        set_keybindings(manager_with_config(&[(
            "tui.editor.cursorUp",
            KeyBindingValue::Single("ctrl+p".to_string()),
        )]));
        set_keybindings(manager_with_config(&[(
            "tui.editor.cursorUp",
            KeyBindingValue::Single("ctrl+n".to_string()),
        )]));

        let read = get_keybindings()
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(read.get_keys(Keybinding::EditorCursorUp), keys(&["ctrl+n"]));
    }
}
