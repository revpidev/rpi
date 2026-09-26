//! Asset-content pins for the bundled skill documentation: the docs ship
//! inside the `.rpix` and must not contradict the plugin's own behavior.
//! TE39 review finding: the bundled prompting/management docs still taught
//! the removed `fallbackModels` surface after the v0.70 rebase made it a
//! BREAKING removal — this scan turns any reintroduction red.

use std::path::PathBuf;

fn skill_assets() -> Vec<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/skills/rpi-subagents");
    let mut files = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

#[test]
fn skill_docs_do_not_teach_the_removed_fallback_models_surface() {
    let files = skill_assets();
    assert!(!files.is_empty(), "skill assets must be found");
    for path in files {
        let text = std::fs::read_to_string(&path).expect("read skill doc");
        assert!(
            !text.contains("fallbackModels"),
            "{} still teaches `fallbackModels` (removed by the pi-subagents \
             v0.70 rebase — BREAKING, see changes/v0.1.5.md)",
            path.display()
        );
    }
}
