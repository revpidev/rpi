//! TE32 G12 (FR-Q4-G/R-Q8.2/R-Q8.3): the shipped `rpi-extension.json` and
//! the registry skeleton (`registry-entry.json`, mirrored by TE32 into
//! rpi-pages `registry/rpiv-ask-user-question.json`) are pinned statically.
//!
//! [RPI-OWN] — no upstream parity leg (the upstream ships as an npm
//! package); these assertions are the equivalent behavioral anchor
//! (task §4.2「manifest」row). The lockstep `version`/`minHostVersion` pair
//! is injected at CI pack time (build.yml rewrites the packed copy with the
//! workspace version), so the source-tree manifest only pins the
//! capability/ABI shape here.

use std::path::Path;

use serde_json::Value;

fn read_crate_file(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

#[test]
fn manifest_pins_capabilities_abi_and_native_only_carrier() {
    let manifest: Value =
        serde_json::from_str(&read_crate_file("rpi-extension.json")).expect("manifest json");

    // R-Q8.3: exactly the four consumed capabilities — `tools` (the
    // ask_user_question registration), `ui` (interactive-ui host-calls +
    // ui.notify/editor fallbacks), `session` (events wiring), `events`
    // (rpiv:ask-user:* emit). `exec` must NOT be declared: the external
    // editor goes through the host primitives (`ui.editExternal`).
    assert_eq!(
        manifest["capabilities"],
        serde_json::json!(["tools", "ui", "session", "events"]),
        "capabilities must be exactly tools/ui/session/events (R-Q8.3)"
    );
    // No `exec` capability anywhere in the list (asserted via the exact
    // match above; kept explicit for the failure message).
    assert!(
        manifest["capabilities"]
            .as_array()
            .is_some_and(|caps| !caps.iter().any(|cap| cap == "exec")),
        "exec capability must not be declared (R-Q8.3)"
    );

    // ADR-0013: rpiAbi stays at 1 (all consumed host-calls are additive).
    assert_eq!(manifest["rpiAbi"], serde_json::json!(1), "rpiAbi: 1");

    // R-Q8.2 native-only: the manifest must NOT carry a `wasm` field — a
    // manifest with both carriers would make the loader prefer wasm and
    // change behavior (config unreadable on wasm). Only the native carrier
    // ships; wasm stays a build-time sandbox option.
    assert!(
        manifest.get("wasm").is_none(),
        "manifest must not declare a wasm carrier (R-Q8.2 native-only)"
    );
    assert_eq!(
        manifest["native"], "librpi_ext_ask_user_question.so",
        "native carrier filename (renamed to the manifest name at pack time)"
    );
    assert_eq!(manifest["name"], "rpiv-ask-user-question");
}

#[test]
fn registry_entry_matches_official_lockstep_shape() {
    // TE32: the skeleton is mirrored verbatim into rpi-pages
    // `registry/rpiv-ask-user-question.json` (official first-party entry,
    // shared revpidev/rpi Release). generate-site.py validates the same
    // fields (name/repository/description/author/license); `lockstepHost:
    // true` makes every version's minHostVersion = the version itself —
    // for the first release that is 0.1.4, the first host version with
    // interactive-ui-abi (R-Q8.4).
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
