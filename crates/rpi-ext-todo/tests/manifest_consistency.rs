//! TE36 G12 (FR-A / R-T8): the shipped `rpi-extension.json`, the registry
//! skeleton (`registry-entry.json`, mirrored by TE36 into rpi-pages
//! `registry/rpiv-todo.json`) and the `.rpix` ship inventory are pinned
//! statically.
//!
//! [RPI-OWN] for the registry/ship legs — the upstream ships as an npm
//! package, and its `ship-manifest.test.ts`
//! (`packages/rpiv-todo/ship-manifest.test.ts` @ `0fdf4f8`) is the
//! expectation source ported here: upstream proves the `package.json`
//! `files` array covers every production `.ts` module (nothing the plugin
//! imports at runtime is missing from the tarball). The rpi `.rpix` packs
//! exactly one artifact — the cdylib — so the same two-way guarantee is:
//! every production module under `src/` compiles into that cdylib
//! (reachable from the lib root through `mod` declarations), and the
//! manifest's `native` filename matches the crate's cdylib artifact name
//! (the `stale` leg: the ship list points at a real artifact).
//!
//! The lockstep `version`/`minHostVersion` pair is injected at CI pack
//! time (build.yml rewrites the packed copy with the workspace version —
//! `0.1.5-rc.1` on the first RC tag, `0.1.5` at stable), so the
//! source-tree manifest here only pins the capability/ABI shape.

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

    // R-T8 / design §1: exactly the five consumed capabilities — `tools`
    // (the `todo` tool registration), `commands` (`/todos`),
    // `ui` (setWidget overlay / notify / theme / getToolsExpanded /
    // registerShortcut), `session` (ctx.sessionFile / ctx.sessionToolResults,
    // ADR-0022/ADR-0030), `events` (the six lifecycle subscriptions).
    // `exec` must NOT be declared: no subprocess surface anywhere.
    assert_eq!(
        manifest["capabilities"],
        serde_json::json!(["tools", "commands", "ui", "session", "events"]),
        "capabilities must be exactly tools/commands/ui/session/events (R-T8)"
    );
    assert!(
        manifest["capabilities"]
            .as_array()
            .is_some_and(|caps| !caps.iter().any(|cap| cap == "exec")),
        "exec capability must not be declared"
    );

    // ADR-0013: rpiAbi stays at 1 (every consumed host-call is additive;
    // sessionToolResults landed as one in V15-14).
    assert_eq!(manifest["rpiAbi"], serde_json::json!(1), "rpiAbi: 1");

    // Native-only carrier (R-T8): the manifest must NOT carry a `wasm`
    // field — a dual-carrier manifest makes the loader prefer wasm and
    // changes behavior (config unreadable there). The overlay/shortcut/
    // replay surfaces are native-only.
    assert!(
        manifest.get("wasm").is_none(),
        "manifest must not declare a wasm carrier (native-only)"
    );
    assert_eq!(
        manifest["native"], "librpi_ext_todo.so",
        "native carrier filename (renamed to this manifest name at pack time)"
    );
    assert_eq!(manifest["name"], "rpiv-todo");
}

#[test]
fn registry_entry_matches_official_lockstep_shape() {
    // TE36: the skeleton is mirrored verbatim into rpi-pages
    // `registry/rpiv-todo.json` (official first-party entry, shared
    // revpidev/rpi Release; index key `rpiv-todo` per the 2026-09-20 M10
    // ruling, following the `rpiv-ask-user-question` key precedent).
    // generate-site.py validates the same fields
    // (name/repository/description/author/license); `lockstepHost: true`
    // makes every version's minHostVersion = the version itself — the
    // first release rides the host's `v0.1.5-rc.1` tag (V14-19 rc channel;
    // `rpi install rpiv-todo --rc`), stable lands at 0.1.5 (R-T8).
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
                .is_some_and(|v| !v.is_empty()),
            "registry entry field {field:?} must be non-empty (generate-site.py requires it)"
        );
    }
}

/// One file-backed module declaration: `mod x;` / `pub mod x;` /
/// `pub(crate) mod x;` (the crate's plain convention — no `#[path]`
/// attributes anywhere under `src/`, which the walk asserts by falling
/// back to the standard `x.rs` / `x/mod.rs` resolution).
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
        // Semicolon form only — `mod x { … }` is an inline module with no
        // file of its own.
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

/// Resolve a declared module name to its file, honoring both the
/// `x.rs`/`x/mod.rs` directory convention and the Rust 2018 sibling-dir
/// form (`tool.rs` + `tool/` — children of `foo.rs` live in `foo/`),
/// returning `None` when no candidate exists (reported as a stale
/// declaration).
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

#[test]
fn ship_manifest_every_production_module_packs_into_the_cdylib() {
    // Port of upstream `ship-manifest.test.ts` (verifyShipManifest @
    // `0fdf4f8`): two-way coverage between the on-disk production tree and
    // what ships. Upstream: the npm `files` array vs every production
    // `.ts` module. rpi: the `.rpix` ships one artifact — the cdylib built
    // from the lib target — so coverage means every `.rs` file under
    // `src/` is reachable from `src/lib.rs` through file-backed `mod`
    // declarations (a stray unreferenced `.rs` would silently miss the
    // archive, exactly like an unlisted upstream module), and every
    // declaration resolves to a file on disk (the stale leg).
    //
    // Asset directories are out of scope for the walk, matching upstream
    // (`locales/*.json` there; here the nine locale tables are
    // `include_str!`-compiled into the cdylib, pinned by the i18n key
    // -completeness tests since TE35).
    let manifest: Value =
        serde_json::from_str(&read_crate_file("rpi-extension.json")).expect("manifest json");

    // The Cargo.toml is TOML — plain-text scan for the two facts the ship
    // inventory needs (kept robust to comment lines).
    let cargo_text = read_crate_file("Cargo.toml");
    assert!(
        cargo_text.contains("crate-type = [\"rlib\", \"cdylib\"]"),
        "lib target must build both rlib (tests) and the distributed cdylib"
    );
    assert!(
        cargo_text.contains("name = \"rpi-ext-todo\""),
        "package name pins the cdylib artifact name (CI derives librpi_ext_todo.so)"
    );

    // `stale` leg for the ship list: the manifest `native` filename is the
    // cdylib artifact the CI pack loop lifts from
    // target/<triple>/release/ — `lib` + package underscores + `.so`,
    // renamed to this exact name inside the archive.
    assert_eq!(
        manifest["native"], "librpi_ext_todo.so",
        "native carrier must match the crate's cdylib artifact"
    );

    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut on_disk: BTreeSet<PathBuf> = BTreeSet::new();
    collect_rs_files(&src, &mut on_disk);
    assert!(
        !on_disk.is_empty(),
        "src tree walk found no modules (test setup bug)"
    );

    // `missing` leg: BFS the file-backed module tree from lib.rs.
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
                // A declaration without a file fails the build itself; the
                // compiler is the authority — reported here as stale anyway.
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
         .rpix cdylib (upstream ship-manifest `missing` leg): {missing:?}"
    );
    assert_eq!(
        reachable, on_disk,
        "reachable set must equal the on-disk production set"
    );
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
