//! Built-in model catalog validation (T13 W4).
//!
//! Three layers:
//! 1. Integrity: every vendored `src/providers/data/*.json` matches the sha256
//!    recorded in the vendored `.manifest.json` (upstream `model-data.ts`).
//! 2. Provenance: the vendored set is byte-compared, model-by-model and
//!    field-by-field, against the pinned upstream
//!    `external/pi/packages/ai/src/providers/data/` (read-only reference).
//! 3. Runtime shape: the parsed catalog (`pir_ai::generated`) preserves
//!    provider set, model order, and every field.

use std::collections::BTreeMap;
use std::path::PathBuf;

use pir_ai::generated::{builtin_catalog, get_builtin_model_data_generated_at, get_builtin_models};
use sha2::Digest;

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/providers/data")
}

fn upstream_data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../external/pi/packages/ai/src/providers/data")
        .canonicalize()
        .expect("upstream data dir (submodule external/pi must be checked out)")
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = sha2::Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn provider_files(dir: &std::path::Path) -> BTreeMap<String, PathBuf> {
    std::fs::read_dir(dir)
        .expect("read data dir")
        .map(|entry| {
            entry
                .expect("dir entry")
                .file_name()
                .into_string()
                .expect("utf-8 name")
        })
        .filter(|name| name.ends_with(".json") && !name.starts_with('.'))
        .map(|name| (name.clone(), dir.join(name)))
        .collect()
}

#[test]
fn test_vendored_files_match_manifest_sha256() {
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(data_dir().join(".manifest.json")).expect("manifest"),
    )
    .expect("manifest json");
    let files = manifest["files"].as_object().expect("files");
    assert_eq!(files.len(), 37);
    for (name, hash) in files {
        let bytes = std::fs::read(data_dir().join(name)).expect("vendored file");
        assert_eq!(
            &sha256_hex(&bytes),
            hash.as_str().expect("hash string"),
            "sha256 mismatch for {name}"
        );
    }
}

#[test]
fn test_vendored_set_matches_upstream_file_set() {
    let vendored = provider_files(&data_dir());
    let upstream = provider_files(&upstream_data_dir());
    assert_eq!(vendored.len(), 37);
    assert_eq!(
        vendored.keys().collect::<Vec<_>>(),
        upstream.keys().collect::<Vec<_>>(),
        "vendored file set diverges from upstream"
    );
}

/// TS has a single `number` type; our typed cost fields are `f64` and
/// re-serialize integer JSON as `0.0`. Normalize every number to f64 so the
/// comparison is about field content, not int/float representation.
fn normalize_numbers(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Number(n) => serde_json::Value::from(n.as_f64().expect("finite")),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(normalize_numbers).collect())
        }
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), normalize_numbers(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[test]
fn test_catalog_field_by_field_against_upstream() {
    let catalog = builtin_catalog().expect("catalog parses");
    let upstream = provider_files(&upstream_data_dir());
    let mut total = 0usize;

    for (name, path) in &upstream {
        let provider = name.trim_end_matches(".json");
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).expect("upstream file"))
                .expect("upstream json");
        let groups = json.as_object().expect("groups");

        // Upstream flattenModelCatalog order: groups in file order, models in
        // group order (both key-sorted by the generator).
        let expected: Vec<(&str, &serde_json::Value)> = groups
            .values()
            .flat_map(|group| {
                group
                    .as_object()
                    .expect("model group")
                    .iter()
                    .map(|(id, model)| (id.as_str(), model))
            })
            .collect();

        let models = catalog.models(provider);
        assert_eq!(
            models.len(),
            expected.len(),
            "model count diverges for {provider}"
        );
        for (model, (expected_id, expected_value)) in models.iter().zip(&expected) {
            assert_eq!(
                model.id, *expected_id,
                "model order diverges for {provider}"
            );
            assert_eq!(model.provider, provider);
            let actual = normalize_numbers(&serde_json::to_value(model).expect("serialize model"));
            assert_eq!(
                &actual,
                &normalize_numbers(expected_value),
                "field-level divergence for {provider}:{expected_id}"
            );
            total += 1;
        }
    }
    assert_eq!(total, 1153);
}

#[test]
fn test_catalog_accessors_and_generated_at() {
    // Every registry provider with a catalog entry yields models.
    for spec in pir_ai::providers::BUILTIN_PROVIDERS {
        if spec.in_catalog {
            assert!(!get_builtin_models(spec.id).is_empty(), "{}", spec.id);
        }
    }
    // Pinned to the vendored .manifest.json generatedAt
    // (2026-07-30T01:56:27.841Z); update on catalog refresh.
    assert_eq!(
        get_builtin_model_data_generated_at(),
        Some(1_785_376_587_841)
    );
}
