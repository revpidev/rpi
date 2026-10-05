//! TE44 G12/G7: the shipped manifest, the registry skeleton
//! (`registry-entry.json`, mirrored by TE47 into rpi-pages
//! `registry/rpi-usage.json`), the script/fixture inventory, and the
//! dependency red line (no HTTP client) are pinned statically.
//!
//! [RPI-OWN] for the registry/ship legs. The lockstep `version` /
//! `minHostVersion` pair is injected at CI pack time (build.yml rewrites the
//! packed copy with the workspace version), so the source-tree manifest only
//! pins the capability/ABI shape.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

fn crate_file(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(name)
}

fn read_crate_file(name: &str) -> String {
    let path = crate_file(name);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

#[test]
fn manifest_pins_capabilities_abi_and_native_only_carrier() {
    let manifest: Value =
        serde_json::from_str(&read_crate_file("rpi-extension.json")).expect("manifest json");

    // Exactly the four consumed capabilities: `commands` (/usage), `ui`
    // (setStatus/notify), `session` (ctx.usage.*, ctx.model), `events`
    // (the four subscriptions). `exec` must NOT be declared — scripts run
    // inside the host framework, never through a plugin subprocess API.
    assert_eq!(
        manifest["capabilities"],
        serde_json::json!(["commands", "ui", "session", "events"]),
        "capabilities must be exactly commands/ui/session/events"
    );
    assert!(
        manifest["capabilities"]
            .as_array()
            .is_some_and(|caps| !caps.iter().any(|cap| cap == "exec" || cap == "tools")),
        "exec/tools capabilities must not be declared"
    );

    // ADR-0013: rpiAbi stays at 1 (ctx.usage.*, getMode/setMode, and the
    // V16-14 exposure surface all landed additively).
    assert_eq!(manifest["rpiAbi"], serde_json::json!(1), "rpiAbi: 1");

    // Native-only carrier.
    assert!(
        manifest.get("wasm").is_none(),
        "manifest must not declare a wasm carrier (native-only)"
    );
    assert_eq!(
        manifest["native"], "librpi_ext_usage.so",
        "native carrier filename (renamed to this manifest name at pack time)"
    );
    assert_eq!(
        manifest["name"], "rpi-usage",
        "official rpi- prefix (ADR-0033)"
    );
}

#[test]
fn registry_entry_is_official_and_lockstep() {
    let entry: Value =
        serde_json::from_str(&read_crate_file("registry-entry.json")).expect("registry json");
    assert_eq!(entry["name"], "rpi-usage");
    assert_eq!(entry["repository"], "revpidev/rpi");
    assert_eq!(entry["official"], true);
    assert_eq!(entry["lockstepHost"], true);
    for key in ["description", "descriptionZh"] {
        assert!(
            entry[key]
                .as_str()
                .is_some_and(|text| !text.trim().is_empty()),
            "{key} present"
        );
    }
}

#[test]
fn every_production_module_is_file_backed_and_reachable() {
    // No `mod.rs` anywhere (G15) and no inline module bodies: lib.rs declares
    // the six production modules as separate files.
    let lib = read_crate_file("src/lib.rs");
    for module in ["command", "config", "footer", "format", "host", "providers"] {
        assert!(
            lib.contains(&format!("pub mod {module};")),
            "lib.rs declares {module}"
        );
        assert!(
            crate_file(&format!("src/{module}.rs")).is_file(),
            "src/{module}.rs exists"
        );
    }
    assert!(
        !crate_file("src/mod.rs").exists(),
        "the crate must not contain mod.rs"
    );
}

#[test]
fn the_four_provider_scripts_are_present_and_embedded() {
    let embedded: Vec<(String, &str)> = rpi_ext_usage::providers::BUILTIN_SCRIPTS
        .iter()
        .map(|script| (script.file.to_owned(), script.source))
        .collect();
    assert_eq!(
        embedded
            .iter()
            .map(|(file, _)| file.as_str())
            .collect::<Vec<_>>(),
        vec![
            "deepseek.py",
            "glm_coding_plan.py",
            "minimax_token_plan.py",
            "kimi_code.py"
        ]
    );
    for (file, source) in embedded {
        let on_disk = read_crate_file(&format!("scripts/{file}"));
        assert_eq!(on_disk, source, "{file} is embedded verbatim");
        assert!(
            on_disk.starts_with("#!/usr/bin/env python3"),
            "{file} carries a python3 shebang"
        );
        assert!(
            !on_disk.contains("import requests") && !on_disk.contains("import httpx"),
            "{file} uses the standard library only"
        );
    }
}

#[test]
fn recorded_fixtures_cover_every_provider_and_degradation() {
    let expected = [
        "deepseek_balance.json",
        "glm_quota.json",
        "glm_quota_weekly.json",
        "glm_error.json",
        "minimax_remains.json",
        "minimax_error.json",
        "kimi_usages.json",
        "kimi_usages_used.json",
    ];
    for name in expected {
        let path = crate_file(&format!("fixtures/{name}"));
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        serde_json::from_str::<Value>(&text)
            .unwrap_or_else(|error| panic!("{} is not JSON: {error}", path.display()));
    }
}

#[test]
fn direct_dependencies_include_no_http_client() {
    let cargo = read_crate_file("Cargo.toml");
    let mut in_dependencies = false;
    let mut names = BTreeSet::new();
    for line in cargo.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') {
            in_dependencies = line == "[dependencies]";
            continue;
        }
        if in_dependencies && let Some((key, _)) = line.split_once('=') {
            let key = key.trim();
            let key = key.strip_suffix(".workspace").unwrap_or(key);
            names.insert(key.to_owned());
        }
    }
    assert_eq!(
        names,
        BTreeSet::from([
            "abi_stable".to_owned(),
            "rpi-ext-host".to_owned(),
            "serde".to_owned(),
            "serde_json".to_owned(),
            "tracing".to_owned(),
        ]),
        "the plugin adds no HTTP client (all provider calls run through the host framework)"
    );
    for banned in ["reqwest", "wreq", "hyper", "ureq", "isahc", "curl", "http"] {
        assert!(
            !names.contains(banned),
            "{banned} must not be a direct dependency"
        );
    }
    // The plugin source never reaches for one either.
    for module in [
        "command",
        "config",
        "footer",
        "format",
        "host",
        "providers",
        "lib",
    ] {
        let source = read_crate_file(&format!("src/{module}.rs"));
        for banned in ["reqwest", "wreq::", "hyper::", "ureq::", "isahc", "curl::"] {
            assert!(
                !source.contains(banned),
                "src/{module}.rs must not reference {banned}"
            );
        }
    }
}
