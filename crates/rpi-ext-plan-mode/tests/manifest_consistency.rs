//! TE43 G12: the shipped `rpi-extension.json`, the registry skeleton
//! (`registry-entry.json`, mirrored by TE47 into rpi-pages
//! `registry/rpi-plan-mode.json`) and the cdylib ship inventory are pinned
//! statically.
//!
//! [RPI-OWN] for the registry/ship legs. The `.rpix` packs exactly one
//! artifact — the cdylib — so the two-way guarantee is: every production
//! module under `src/` is reachable from `src/lib.rs` through file-backed
//! `mod` declarations, and the manifest `native` filename matches the
//! crate's cdylib artifact (the `stale` leg).
//!
//! The lockstep `version`/`minHostVersion` pair is injected at CI pack
//! time (build.yml rewrites the packed copy with the workspace version),
//! so the source-tree manifest only pins the capability/ABI shape.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

fn read_crate_file(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

#[test]
fn manifest_pins_capabilities_abi_and_native_only_carrier() {
    let manifest: Value =
        serde_json::from_str(&read_crate_file("rpi-extension.json")).expect("manifest json");

    // R-PM-1..7: exactly the five consumed capabilities — `tools`
    // (write_plan), `commands` (/plan), `ui` (select/input/notify/
    // setWidget/editExternal), `session` (ctx.sessionFile / getMode /
    // setMode), `events` (the five subscriptions). `exec` must NOT be
    // declared: no subprocess surface anywhere.
    assert_eq!(
        manifest["capabilities"],
        serde_json::json!(["commands", "tools", "ui", "session", "events"]),
        "capabilities must be exactly commands/tools/ui/session/events"
    );
    assert!(
        manifest["capabilities"]
            .as_array()
            .is_some_and(|caps| !caps.iter().any(|cap| cap == "exec")),
        "exec capability must not be declared"
    );

    // ADR-0013: rpiAbi stays at 1 (every consumed host-call is additive;
    // setToolExposures/clearToolExposures landed in V16-14 as additive).
    assert_eq!(manifest["rpiAbi"], serde_json::json!(1), "rpiAbi: 1");

    // Native-only carrier: the manifest must NOT carry a `wasm` field.
    assert!(
        manifest.get("wasm").is_none(),
        "manifest must not declare a wasm carrier (native-only)"
    );
    assert_eq!(
        manifest["native"], "librpi_ext_plan_mode.so",
        "native carrier filename (renamed to this manifest name at pack time)"
    );
    assert_eq!(manifest["name"], "rpi-plan-mode");
    assert_eq!(
        manifest["version"], "0.1.0",
        "pack-time injected placeholder"
    );
}

#[test]
fn registry_entry_matches_official_lockstep_shape() {
    // TE43/TE47: the skeleton is mirrored into rpi-pages
    // `registry/rpi-plan-mode.json` (official first-party entry, shared
    // revpidev/rpi Release; index key `rpi-plan-mode` per ADR-0033).
    let entry: Value =
        serde_json::from_str(&read_crate_file("registry-entry.json")).expect("registry entry json");
    let manifest: Value =
        serde_json::from_str(&read_crate_file("rpi-extension.json")).expect("manifest json");

    assert_eq!(
        entry["name"], manifest["name"],
        "registry name == manifest name"
    );
    assert_eq!(entry["repository"], "revpidev/rpi");
    assert_eq!(entry["official"], serde_json::json!(true));
    assert_eq!(entry["lockstepHost"], serde_json::json!(true));
    for field in ["description", "author", "license", "descriptionZh"] {
        assert!(
            entry
                .get(field)
                .and_then(Value::as_str)
                .is_some_and(|value| !value.is_empty()),
            "registry entry field {field:?} must be non-empty (generate-site.py requires it)"
        );
    }
}

#[test]
fn ship_manifest_every_production_module_packs_into_the_cdylib() {
    let manifest: Value =
        serde_json::from_str(&read_crate_file("rpi-extension.json")).expect("manifest json");
    let cargo_text = read_crate_file("Cargo.toml");
    assert!(
        cargo_text.contains("crate-type = [\"rlib\", \"cdylib\"]"),
        "lib target must build both rlib (tests) and the distributed cdylib"
    );
    assert!(
        cargo_text.contains("name = \"rpi-ext-plan-mode\""),
        "package name pins the cdylib artifact name (CI derives librpi_ext_plan_mode.so)"
    );
    assert_eq!(
        manifest["native"], "librpi_ext_plan_mode.so",
        "native carrier must match the crate's cdylib artifact"
    );

    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut on_disk: BTreeSet<PathBuf> = BTreeSet::new();
    collect_rs_files(&src, &mut on_disk);
    assert!(
        !on_disk.is_empty(),
        "src tree walk found no modules (test setup bug)"
    );

    let mut reachable: BTreeSet<PathBuf> = BTreeSet::new();
    let mut queue: Vec<PathBuf> = vec![src.join("lib.rs")];
    reachable.insert(src.join("lib.rs"));
    while let Some(file) = queue.pop() {
        let source = std::fs::read_to_string(&file)
            .unwrap_or_else(|error| panic!("{}: {error}", file.display()));
        for name in file_backed_mods(&source) {
            match module_file(&file, &name) {
                Some(child) => {
                    if reachable.insert(child.clone()) {
                        queue.push(child);
                    }
                }
                None => panic!(
                    "mod {name} declared in {} has no file on disk",
                    file.display()
                ),
            }
        }
    }

    let missing: Vec<String> = on_disk
        .iter()
        .filter(|file| !reachable.contains(*file))
        .map(|file| {
            file.strip_prefix(&src)
                .unwrap_or(file)
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert!(
        missing.is_empty(),
        "production modules not reachable from src/lib.rs would miss the \
         .rpix cdylib: {missing:?}"
    );
    assert_eq!(
        reachable, on_disk,
        "reachable set must equal the on-disk production set"
    );
}

/// One file-backed module declaration: `mod x;` / `pub mod x;` /
/// `pub(crate) mod x;` (no `#[path]` attributes under `src/`).
fn file_backed_mods(source: &str) -> Vec<String> {
    let mut mods = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed
            .strip_prefix("pub mod ")
            .or_else(|| trimmed.strip_prefix("pub(crate) mod "))
            .or_else(|| trimmed.strip_prefix("mod "))
        else {
            continue;
        };
        let Some(name) = rest.strip_suffix(';') else {
            continue;
        };
        let name = name.trim();
        if name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !name.is_empty() {
            mods.push(name.to_string());
        }
    }
    mods
}

/// Resolve a declared module name to its file (`x.rs` / sibling-dir
/// forms); `None` = stale declaration.
fn module_file(file: &Path, name: &str) -> Option<PathBuf> {
    let dir = file.parent().expect("parent dir");
    let mut candidates: Vec<PathBuf> = vec![
        dir.join(format!("{name}.rs")),
        dir.join(name).join("mod.rs"),
    ];
    if file.file_stem().is_some_and(|stem| stem != "mod") {
        let sibling = dir.join(file.file_stem().expect("stem"));
        candidates.push(sibling.join(format!("{name}.rs")));
        candidates.push(sibling.join(name).join("mod.rs"));
    }
    candidates.into_iter().find(|candidate| candidate.is_file())
}

fn collect_rs_files(dir: &Path, out: &mut BTreeSet<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        panic!("cannot read {}", dir.display());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.insert(path);
        }
    }
}
