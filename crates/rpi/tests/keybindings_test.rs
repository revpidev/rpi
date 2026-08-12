//! Integration tests for `rpi::core::keybindings`.
//!
//! Port of keybinding loading / migration scenarios from
//! `packages/coding-agent/src/core/keybindings.ts` and
//! `packages/coding-agent/test/keybindings-migration.test.ts`.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use rpi::core::keybindings::*;

/// Self-managing temp directory.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rpi-keybindings-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_config(dir: &std::path::Path, json: &str) -> PathBuf {
    let path = dir.join("keybindings.json");
    fs::write(&path, json).unwrap();
    path
}

// --- Basic loading --------------------------------------------------------

#[test]
fn test_load_empty_config() {
    let tmp = TempDir::new();
    let path = write_config(tmp.path(), "{}");
    let mgr = KeybindingsManager::create_from_path(&path);

    // With no overrides, all defaults should be active
    assert_eq!(mgr.get_keys("tui.editor.cursorUp"), vec!["up"]);
    assert_eq!(mgr.get_keys("app.interrupt"), vec!["escape"]);
}

#[test]
fn test_load_missing_file() {
    let tmp = TempDir::new();
    let path = tmp.path().join("nonexistent.json");
    let mgr = KeybindingsManager::create_from_path(&path);

    // Missing file → all defaults
    assert_eq!(mgr.get_keys("tui.editor.cursorUp"), vec!["up"]);
}

#[test]
fn test_load_string_value() {
    let tmp = TempDir::new();
    let path = write_config(tmp.path(), r#"{"tui.editor.cursorUp": "ctrl+p"}"#);
    let mgr = KeybindingsManager::create_from_path(&path);

    assert_eq!(mgr.get_keys("tui.editor.cursorUp"), vec!["ctrl+p"]);
    // Other bindings still default
    assert_eq!(mgr.get_keys("tui.editor.cursorDown"), vec!["down"]);
}

#[test]
fn test_load_array_value() {
    let tmp = TempDir::new();
    let path = write_config(
        tmp.path(),
        r#"{"tui.editor.cursorUp": ["ctrl+p", "ctrl+n"]}"#,
    );
    let mgr = KeybindingsManager::create_from_path(&path);

    assert_eq!(
        mgr.get_keys("tui.editor.cursorUp"),
        vec!["ctrl+p", "ctrl+n"]
    );
}

#[test]
fn test_load_invalid_json_uses_defaults() {
    let tmp = TempDir::new();
    let path = write_config(tmp.path(), "not valid json");
    let mgr = KeybindingsManager::create_from_path(&path);

    // Invalid JSON → all defaults
    assert_eq!(mgr.get_keys("tui.editor.cursorUp"), vec!["up"]);
}

// --- Legacy migration via file loading ------------------------------------

#[test]
fn test_file_load_migrates_legacy_names() {
    let tmp = TempDir::new();
    let path = write_config(
        tmp.path(),
        r#"{"cursorUp": "ctrl+p", "interrupt": "ctrl+x"}"#,
    );
    let mgr = KeybindingsManager::create_from_path(&path);

    // Legacy names are migrated
    assert_eq!(mgr.get_keys("tui.editor.cursorUp"), vec!["ctrl+p"]);
    assert_eq!(mgr.get_keys("app.interrupt"), vec!["ctrl+x"]);
}

#[test]
fn test_file_load_mixed_legacy_and_modern() {
    let tmp = TempDir::new();
    let path = write_config(
        tmp.path(),
        r#"{
            "cursorUp": "ctrl+p",
            "tui.editor.cursorUp": "ctrl+n",
            "submit": "ctrl+enter"
        }"#,
    );
    let mgr = KeybindingsManager::create_from_path(&path);

    // Modern name wins over legacy when both present
    assert_eq!(mgr.get_keys("tui.editor.cursorUp"), vec!["ctrl+n"]);
    // Non-conflicting legacy name still migrates
    assert_eq!(mgr.get_keys("tui.input.submit"), vec!["ctrl+enter"]);
}

// --- /reload --------------------------------------------------------------

#[test]
fn test_reload_picks_up_changes() {
    let tmp = TempDir::new();
    let path = write_config(tmp.path(), r#"{"tui.editor.cursorUp": "ctrl+p"}"#);
    let mut mgr = KeybindingsManager::create_from_path(&path);
    assert_eq!(mgr.get_keys("tui.editor.cursorUp"), vec!["ctrl+p"]);

    // Modify the file
    fs::write(&path, r#"{"tui.editor.cursorUp": "ctrl+n"}"#).unwrap();

    // Reload
    mgr.reload();
    assert_eq!(mgr.get_keys("tui.editor.cursorUp"), vec!["ctrl+n"]);
}

#[test]
fn test_reload_no_path_is_noop() {
    let mut mgr = KeybindingsManager::new();
    mgr.reload(); // should not crash
    assert_eq!(mgr.get_keys("tui.editor.cursorUp"), vec!["up"]);
}

// --- Full migration table spot-checks -------------------------------------

#[test]
fn test_migration_all_tui_editor() {
    let mut raw = serde_json::Map::new();
    raw.insert("cursorUp".into(), serde_json::json!("a"));
    raw.insert("cursorDown".into(), serde_json::json!("b"));
    raw.insert("cursorLeft".into(), serde_json::json!("c"));
    raw.insert("cursorRight".into(), serde_json::json!("d"));
    raw.insert("yank".into(), serde_json::json!("e"));
    raw.insert("undo".into(), serde_json::json!("f"));

    let (migrated, _) = migrate_keybindings_config(&raw);
    assert_eq!(
        migrated.get("tui.editor.cursorUp"),
        Some(&serde_json::json!("a"))
    );
    assert_eq!(
        migrated.get("tui.editor.cursorDown"),
        Some(&serde_json::json!("b"))
    );
    assert_eq!(
        migrated.get("tui.editor.cursorLeft"),
        Some(&serde_json::json!("c"))
    );
    assert_eq!(
        migrated.get("tui.editor.cursorRight"),
        Some(&serde_json::json!("d"))
    );
    assert_eq!(
        migrated.get("tui.editor.yank"),
        Some(&serde_json::json!("e"))
    );
    assert_eq!(
        migrated.get("tui.editor.undo"),
        Some(&serde_json::json!("f"))
    );
}

#[test]
fn test_migration_all_tui_input_select() {
    let mut raw = serde_json::Map::new();
    raw.insert("newLine".into(), serde_json::json!("a"));
    raw.insert("submit".into(), serde_json::json!("b"));
    raw.insert("tab".into(), serde_json::json!("c"));
    raw.insert("copy".into(), serde_json::json!("d"));
    raw.insert("selectUp".into(), serde_json::json!("e"));
    raw.insert("selectCancel".into(), serde_json::json!("f"));

    let (migrated, _) = migrate_keybindings_config(&raw);
    assert_eq!(
        migrated.get("tui.input.newLine"),
        Some(&serde_json::json!("a"))
    );
    assert_eq!(
        migrated.get("tui.input.submit"),
        Some(&serde_json::json!("b"))
    );
    assert_eq!(migrated.get("tui.input.tab"), Some(&serde_json::json!("c")));
    assert_eq!(
        migrated.get("tui.input.copy"),
        Some(&serde_json::json!("d"))
    );
    assert_eq!(migrated.get("tui.select.up"), Some(&serde_json::json!("e")));
    assert_eq!(
        migrated.get("tui.select.cancel"),
        Some(&serde_json::json!("f"))
    );
}

#[test]
fn test_migration_all_app() {
    let mut raw = serde_json::Map::new();
    raw.insert("interrupt".into(), serde_json::json!("a"));
    raw.insert("clear".into(), serde_json::json!("b"));
    raw.insert("exit".into(), serde_json::json!("c"));
    raw.insert("suspend".into(), serde_json::json!("d"));
    raw.insert("pasteImage".into(), serde_json::json!("e"));
    raw.insert("deleteSessionNoninvasive".into(), serde_json::json!("f"));

    let (migrated, _) = migrate_keybindings_config(&raw);
    assert_eq!(migrated.get("app.interrupt"), Some(&serde_json::json!("a")));
    assert_eq!(migrated.get("app.clear"), Some(&serde_json::json!("b")));
    assert_eq!(migrated.get("app.exit"), Some(&serde_json::json!("c")));
    assert_eq!(migrated.get("app.suspend"), Some(&serde_json::json!("d")));
    assert_eq!(
        migrated.get("app.clipboard.pasteImage"),
        Some(&serde_json::json!("e"))
    );
    assert_eq!(
        migrated.get("app.session.deleteNoninvasive"),
        Some(&serde_json::json!("f"))
    );
}

#[test]
fn test_no_migration_for_new_namespaces() {
    // These IDs have no legacy equivalents
    let mut raw = serde_json::Map::new();
    raw.insert("app.message.copy".into(), serde_json::json!("ctrl+x"));
    raw.insert("app.models.save".into(), serde_json::json!("ctrl+s"));
    raw.insert("app.tree.filter.all".into(), serde_json::json!("ctrl+a"));

    let (migrated, was_migrated) = migrate_keybindings_config(&raw);
    assert!(!was_migrated);
    assert_eq!(
        migrated.get("app.message.copy"),
        Some(&serde_json::json!("ctrl+x"))
    );
}

// --- Config ordering ------------------------------------------------------

#[test]
fn test_config_ordering_known_first() {
    let mut raw = serde_json::Map::new();
    // Insert in reverse order
    raw.insert("app.exit".into(), serde_json::json!("ctrl+d"));
    raw.insert("tui.editor.cursorUp".into(), serde_json::json!("up"));

    let (migrated, _) = migrate_keybindings_config(&raw);
    let keys: Vec<&String> = migrated.keys().collect();
    // tui.editor.cursorUp should come before app.exit (definition order)
    let cu_idx = keys
        .iter()
        .position(|k| k.as_str() == "tui.editor.cursorUp")
        .unwrap();
    let exit_idx = keys.iter().position(|k| k.as_str() == "app.exit").unwrap();
    assert!(cu_idx < exit_idx);
}

#[test]
fn test_config_ordering_extras_alphabetical() {
    let mut raw = serde_json::Map::new();
    raw.insert("zzz_unknown".into(), serde_json::json!("a"));
    raw.insert("aaa_unknown".into(), serde_json::json!("b"));
    raw.insert("tui.editor.cursorUp".into(), serde_json::json!("up"));

    let (migrated, _) = migrate_keybindings_config(&raw);
    let keys: Vec<&String> = migrated.keys().collect();
    // Known key first, then extras alphabetically
    assert_eq!(keys[0].as_str(), "tui.editor.cursorUp");
    assert_eq!(keys[1].as_str(), "aaa_unknown");
    assert_eq!(keys[2].as_str(), "zzz_unknown");
}

// --- All 75 default keys spot-checks (docs/keybindings.md parity) ---------

#[test]
fn test_all_tui_editor_defaults() {
    let mgr = KeybindingsManager::new();
    // Check all 23 tui.editor.* defaults
    assert_eq!(mgr.get_keys("tui.editor.cursorUp"), vec!["up"]);
    assert_eq!(mgr.get_keys("tui.editor.cursorDown"), vec!["down"]);
    // Dedicated prompt-history actions are unbound by default (16ad96ae8).
    assert!(mgr.get_keys("tui.editor.historyPrevious").is_empty());
    assert!(mgr.get_keys("tui.editor.historyNext").is_empty());
    assert_eq!(
        mgr.get_keys("tui.editor.cursorLeft"),
        vec!["left", "ctrl+b"]
    );
    assert_eq!(
        mgr.get_keys("tui.editor.cursorRight"),
        vec!["right", "ctrl+f"]
    );
    assert_eq!(
        mgr.get_keys("tui.editor.cursorWordLeft"),
        vec!["alt+left", "ctrl+left", "alt+b"]
    );
    assert_eq!(
        mgr.get_keys("tui.editor.cursorWordRight"),
        vec!["alt+right", "ctrl+right", "alt+f"]
    );
    assert_eq!(
        mgr.get_keys("tui.editor.cursorLineStart"),
        vec!["home", "ctrl+home", "ctrl+a"]
    );
    assert_eq!(
        mgr.get_keys("tui.editor.cursorLineEnd"),
        vec!["end", "ctrl+end", "ctrl+e"]
    );
    assert_eq!(mgr.get_keys("tui.editor.jumpForward"), vec!["ctrl+]"]);
    assert_eq!(mgr.get_keys("tui.editor.jumpBackward"), vec!["ctrl+alt+]"]);
    assert_eq!(
        mgr.get_keys("tui.editor.pageUp"),
        vec!["pageUp", "ctrl+pageUp"]
    );
    assert_eq!(
        mgr.get_keys("tui.editor.pageDown"),
        vec!["pageDown", "ctrl+pageDown"]
    );
    assert_eq!(
        mgr.get_keys("tui.editor.deleteCharBackward"),
        vec!["backspace"]
    );
    assert_eq!(
        mgr.get_keys("tui.editor.deleteCharForward"),
        vec!["delete", "ctrl+d"]
    );
    assert_eq!(
        mgr.get_keys("tui.editor.deleteWordBackward"),
        vec!["ctrl+w", "alt+backspace"]
    );
    assert_eq!(
        mgr.get_keys("tui.editor.deleteWordForward"),
        vec!["alt+d", "alt+delete"]
    );
    assert_eq!(mgr.get_keys("tui.editor.deleteToLineStart"), vec!["ctrl+u"]);
    assert_eq!(mgr.get_keys("tui.editor.deleteToLineEnd"), vec!["ctrl+k"]);
    assert_eq!(mgr.get_keys("tui.editor.yank"), vec!["ctrl+y"]);
    assert_eq!(mgr.get_keys("tui.editor.yankPop"), vec!["alt+y"]);
    assert_eq!(mgr.get_keys("tui.editor.undo"), vec!["ctrl+-"]);
}

#[test]
fn test_all_tui_input_select_defaults() {
    let mgr = KeybindingsManager::new();
    assert_eq!(
        mgr.get_keys("tui.input.newLine"),
        vec!["shift+enter", "ctrl+j"]
    );
    assert_eq!(mgr.get_keys("tui.input.submit"), vec!["enter"]);
    assert_eq!(mgr.get_keys("tui.input.tab"), vec!["tab"]);
    assert_eq!(mgr.get_keys("tui.input.copy"), vec!["ctrl+c"]);
    assert_eq!(mgr.get_keys("tui.select.up"), vec!["up"]);
    assert_eq!(mgr.get_keys("tui.select.down"), vec!["down"]);
    assert_eq!(mgr.get_keys("tui.select.pageUp"), vec!["pageUp"]);
    assert_eq!(mgr.get_keys("tui.select.pageDown"), vec!["pageDown"]);
    assert_eq!(mgr.get_keys("tui.select.confirm"), vec!["enter"]);
    assert_eq!(mgr.get_keys("tui.select.cancel"), vec!["escape", "ctrl+c"]);
}

#[test]
fn test_all_app_defaults() {
    let mgr = KeybindingsManager::new();
    assert_eq!(mgr.get_keys("app.interrupt"), vec!["escape"]);
    assert_eq!(mgr.get_keys("app.clear"), vec!["ctrl+c"]);
    assert_eq!(mgr.get_keys("app.exit"), vec!["ctrl+d"]);
    assert_eq!(mgr.get_keys("app.thinking.cycle"), vec!["shift+tab"]);
    assert_eq!(mgr.get_keys("app.model.cycleForward"), vec!["ctrl+p"]);
    assert_eq!(
        mgr.get_keys("app.model.cycleBackward"),
        vec!["shift+ctrl+p"]
    );
    assert_eq!(mgr.get_keys("app.model.select"), vec!["ctrl+l"]);
    assert_eq!(mgr.get_keys("app.tools.expand"), vec!["ctrl+o"]);
    assert_eq!(mgr.get_keys("app.thinking.toggle"), vec!["ctrl+t"]);
    assert_eq!(
        mgr.get_keys("app.session.toggleNamedFilter"),
        vec!["ctrl+n"]
    );
    assert_eq!(mgr.get_keys("app.editor.external"), vec!["ctrl+g"]);
    assert_eq!(mgr.get_keys("app.message.copy"), vec!["ctrl+x"]);
    assert_eq!(mgr.get_keys("app.message.followUp"), vec!["alt+enter"]);
    assert_eq!(mgr.get_keys("app.message.dequeue"), vec!["alt+up"]);
    assert_eq!(mgr.get_keys("app.session.new"), Vec::<String>::new());
    assert_eq!(mgr.get_keys("app.session.tree"), Vec::<String>::new());
    assert_eq!(mgr.get_keys("app.session.fork"), Vec::<String>::new());
    assert_eq!(mgr.get_keys("app.session.resume"), Vec::<String>::new());
    assert_eq!(mgr.get_keys("app.tree.editLabel"), vec!["shift+l"]);
    assert_eq!(
        mgr.get_keys("app.tree.toggleLabelTimestamp"),
        vec!["shift+t"]
    );
    assert_eq!(mgr.get_keys("app.session.togglePath"), vec!["ctrl+p"]);
    assert_eq!(mgr.get_keys("app.session.toggleSort"), vec!["ctrl+s"]);
    assert_eq!(mgr.get_keys("app.session.rename"), vec!["ctrl+r"]);
    assert_eq!(mgr.get_keys("app.session.delete"), vec!["ctrl+d"]);
    assert_eq!(
        mgr.get_keys("app.session.deleteNoninvasive"),
        vec!["ctrl+backspace"]
    );
    assert_eq!(mgr.get_keys("app.models.save"), vec!["ctrl+s"]);
    assert_eq!(mgr.get_keys("app.models.enableAll"), vec!["ctrl+a"]);
    assert_eq!(mgr.get_keys("app.models.clearAll"), vec!["ctrl+x"]);
    assert_eq!(mgr.get_keys("app.models.toggleProvider"), vec!["ctrl+p"]);
    assert_eq!(mgr.get_keys("app.models.reorderUp"), vec!["alt+up"]);
    assert_eq!(mgr.get_keys("app.models.reorderDown"), vec!["alt+down"]);
    assert_eq!(mgr.get_keys("app.tree.filter.default"), vec!["ctrl+d"]);
    assert_eq!(mgr.get_keys("app.tree.filter.noTools"), vec!["ctrl+t"]);
    assert_eq!(mgr.get_keys("app.tree.filter.userOnly"), vec!["ctrl+u"]);
    assert_eq!(mgr.get_keys("app.tree.filter.labeledOnly"), vec!["ctrl+l"]);
    assert_eq!(mgr.get_keys("app.tree.filter.all"), vec!["ctrl+a"]);
    assert_eq!(mgr.get_keys("app.tree.filter.cycleForward"), vec!["ctrl+o"]);
    assert_eq!(
        mgr.get_keys("app.tree.filter.cycleBackward"),
        vec!["shift+ctrl+o"]
    );
}

// --- set_user_bindings + reload interface --------------------------------

#[test]
fn test_set_user_bindings() {
    let mut mgr = KeybindingsManager::new();
    let mut bindings = HashMap::new();
    bindings.insert("app.interrupt".to_string(), vec!["ctrl+x".to_string()]);
    mgr.set_user_bindings(bindings);
    assert_eq!(mgr.get_keys("app.interrupt"), vec!["ctrl+x"]);
}

#[test]
fn test_get_definition() {
    let mgr = KeybindingsManager::new();
    let def = mgr.get_definition("tui.editor.cursorUp").unwrap();
    assert_eq!(def.description, "Move cursor up");
}

#[test]
fn test_definition_for_unknown_id() {
    let mgr = KeybindingsManager::new();
    assert!(mgr.get_definition("nonexistent.action").is_none());
}

// --- Conflict detection with file-loaded config --------------------------

#[test]
fn test_conflict_from_file() {
    let tmp = TempDir::new();
    let path = write_config(
        tmp.path(),
        r#"{
            "app.interrupt": "ctrl+x",
            "app.clear": "ctrl+x"
        }"#,
    );
    let mgr = KeybindingsManager::create_from_path(&path);
    let conflicts = mgr.get_conflicts();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].key, "ctrl+x");
}
