//! V18-02 R3.1.1: the four published configuration schemas are generated from
//! the pinned Pi v1.4.0 configuration models, branded for revpi.dev and free
//! of pi.dev endpoints. Runtime loading is covered by the settings/theme
//! tests; this test pins the public artifacts and validates representative
//! documents against them.

use std::path::PathBuf;

use jsonschema::Validator;
use serde_json::Value;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn assert_no_app_brand(value: &Value, path: &str) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key == "$comment" {
                    continue;
                }
                assert_no_app_brand(child, &format!("{path}.{key}"));
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                assert_no_app_brand(child, &format!("{path}[{index}]"));
            }
        }
        Value::String(text) => {
            for needle in ["Pi ", "pi infers", "pi-controlled"] {
                let unbranded = text.match_indices(needle).any(|(index, _)| {
                    index == 0 || !text.as_bytes()[index - 1].eq_ignore_ascii_case(&b'r')
                });
                assert!(
                    !unbranded,
                    "{path}: app-branded schema text {needle:?}: {text}"
                );
            }
        }
        _ => {}
    }
}

fn schema(name: &str) -> (Value, Validator) {
    let path = root().join("schemas").join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    let value: Value = serde_json::from_str(&text).expect("schema json");
    let validator = Validator::new(&value).expect("valid draft 2020-12 schema");
    (value, validator)
}

#[test]
fn schemas_are_branded_and_complete() {
    for name in [
        "settings.schema.json",
        "models.schema.json",
        "keybindings.schema.json",
        "theme.schema.json",
    ] {
        let path = root().join("schemas").join(name);
        let text = std::fs::read_to_string(&path).expect("schema");
        assert!(!text.contains("https://pi.dev"), "{name}: pi.dev endpoint");
        assert!(text.contains("https://revpi.dev/schemas/"), "{name}: id");
        let value: Value = serde_json::from_str(&text).expect("json");
        assert_no_app_brand(&value, name);
        assert_eq!(
            value["$schema"], "https://json-schema.org/draft/2020-12/schema",
            "{name}: draft"
        );
        assert_eq!(
            value["$id"],
            format!("https://revpi.dev/schemas/{name}"),
            "{name}: $id"
        );
    }
}

#[test]
fn site_schema_copies_match_when_the_checkout_is_present() {
    let site = root().join("../rpi-pages/schemas");
    if !site.is_dir() {
        return;
    }
    for name in [
        "settings.schema.json",
        "models.schema.json",
        "keybindings.schema.json",
        "theme.schema.json",
    ] {
        assert_eq!(
            std::fs::read(root().join("schemas").join(name)).expect("repo schema"),
            std::fs::read(site.join(name)).expect("site schema"),
            "{name}: site copy is stale"
        );
    }
}

#[test]
fn settings_schema_accepts_rpi_own_keys_and_share_destination() {
    let (_schema, validator) = schema("settings.schema.json");
    let settings = serde_json::json!({
        "$schema": "https://revpi.dev/schemas/settings.schema.json",
        "defaultProvider": "dgx-spark",
        "sessionShareDestination": "gist",
        "statusLine": {"type": "command", "command": "python3 statusline.py"},
        "usage": {"providers": {"deepseek": {"script": "deepseek.py"}}},
        "modelCatalogUrl": "https://revpi.dev/api/models",
        "telemetryUrl": "",
        "versionCheckUrl": "https://revpi.dev/api/latest-version.json"
    });
    assert!(
        validator.is_valid(&settings),
        "settings: {:?}",
        validator.iter_errors(&settings).collect::<Vec<_>>()
    );
    let bad = serde_json::json!({"sessionShareDestination": "elsewhere"});
    assert!(!validator.is_valid(&bad), "unknown destination rejected");
}

#[test]
fn settings_schema_describes_rpi_share_destination_semantics() {
    let text = std::fs::read_to_string(root().join("schemas/settings.schema.json"))
        .expect("settings schema");
    let value: Value = serde_json::from_str(&text).expect("json");
    let description = value["properties"]["sessionShareDestination"]["description"]
        .as_str()
        .expect("description");
    assert_eq!(
        description,
        "Destination used by /share. \"auto\" and \"gist\" create a GitHub gist; \"radius\" is recognized but unsupported and fails explicitly."
    );
    assert!(
        !description.contains("Radius when authenticated"),
        "rpi has no Radius share backend: {description}"
    );
}

#[test]
fn builtin_theme_validates_against_the_published_theme_schema() {
    let (_schema, validator) = schema("theme.schema.json");
    let theme_path =
        root().join("external/pi/packages/coding-agent/src/modes/interactive/theme/dark.json");
    let theme: Value =
        serde_json::from_str(&std::fs::read_to_string(&theme_path).expect("upstream dark theme"))
            .expect("theme json");
    assert!(
        validator.is_valid(&theme),
        "theme: {:?}",
        validator.iter_errors(&theme).collect::<Vec<_>>()
    );
}
