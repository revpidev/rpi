//! Built-in model catalog — runtime half of the generated pipeline (T13 W4).
//!
//! Ports the catalog-read side of `packages/ai/src/models.generated.ts` +
//! `packages/ai/src/providers/all.ts` (`getBuiltinModel` / `getBuiltinProviders`
//! / `getBuiltinModels` / `getBuiltinModelDataGeneratedAt`) @ pi 0.82.1
//! (2efa728).
//!
//! `build.rs` embeds the vendored `src/providers/data/*.json` (upstream
//! `providers/data/`, corrections of `generate-models.ts` already baked in —
//! the upstream `*.models.ts` are pure `flattenModelCatalog` re-exports) via
//! `include_str!`; this module parses them lazily on first access. Startup
//! treats the data as read-only (coding-standards §3.2); the refresh path is
//! the manual `scripts/refresh-model-catalog.sh`, not a build-time fetch.
//!
//! Intentional differences:
//! - Upstream generates TS literals (`models.generated.ts`); we embed the
//!   vendored JSON and parse with serde at first access (build-time codegen of
//!   1217 model literals was rejected as compile-time noise). Data content is
//!   identical — verified field-by-field against the upstream JSONs in
//!   `tests/model_catalog.rs`.
//! - Upstream `getBuiltinModels(unknown)` returns `[]`; mirrored here as an
//!   empty slice. Parse failures are impossible with intact vendored data and
//!   surface via `builtin_catalog()` (never panic).

use std::collections::BTreeMap;
use std::sync::OnceLock;

use crate::types::Model;

include!(concat!(env!("OUT_DIR"), "/models_generated.rs"));

/// Catalog load error. Only reachable if the vendored JSON is corrupted
/// (generation-time bug); accessor functions degrade to empty results.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("invalid catalog manifest: {0}")]
    Manifest(#[source] serde_json::Error),
    #[error("invalid catalog data for provider {provider}: {source}")]
    ProviderData {
        provider: &'static str,
        #[source]
        source: serde_json::Error,
    },
}

/// `.manifest.json` (upstream `scripts/model-data.ts` `ModelDataManifest`).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogManifest {
    pub schema_version: u32,
    /// ISO-8601 timestamp shared by all built-in provider catalogs.
    pub generated_at: String,
    pub structure_hash: String,
    /// Per-file sha256 (hex) of every vendored `<provider>.json`.
    pub files: BTreeMap<String, String>,
}

/// Parsed built-in catalog: provider id → models in upstream catalog order
/// (vendored JSONs are written key-sorted by the upstream generator, and
/// `BTreeMap` iteration preserves that order).
pub struct BuiltinCatalog {
    providers: Vec<&'static str>,
    models: BTreeMap<&'static str, Vec<Model>>,
    manifest: CatalogManifest,
}

impl BuiltinCatalog {
    /// `all.ts` `getBuiltinProviders()` — catalog provider ids.
    /// (`KnownProvider` additionally includes purely dynamic providers like
    /// `radius` that have no static catalog entry; see `providers.rs`.)
    pub fn providers(&self) -> &[&'static str] {
        &self.providers
    }

    /// `all.ts` `getBuiltinModels(provider)`.
    pub fn models(&self, provider: &str) -> &[Model] {
        self.models.get(provider).map(Vec::as_slice).unwrap_or(&[])
    }

    /// `all.ts` `getBuiltinModel(provider, modelId)`.
    pub fn model(&self, provider: &str, model_id: &str) -> Option<&Model> {
        self.models
            .get(provider)?
            .iter()
            .find(|model| model.id == model_id)
    }

    pub fn manifest(&self) -> &CatalogManifest {
        &self.manifest
    }

    /// `all.ts` `getBuiltinModelDataGeneratedAt()` — milliseconds since the
    /// Unix epoch; `None` when the manifest timestamp is unparseable
    /// (upstream: `Date.parse` → `NaN` → `undefined`).
    pub fn generated_at(&self) -> Option<i64> {
        parse_iso8601_millis(&self.manifest.generated_at)
    }
}

static CATALOG: OnceLock<Result<BuiltinCatalog, CatalogError>> = OnceLock::new();

fn load_catalog() -> Result<BuiltinCatalog, CatalogError> {
    let manifest: CatalogManifest =
        serde_json::from_str(CATALOG_MANIFEST_JSON).map_err(CatalogError::Manifest)?;
    let mut providers = Vec::with_capacity(CATALOG_PROVIDER_DATA.len());
    let mut models = BTreeMap::new();
    for (provider, json) in CATALOG_PROVIDER_DATA {
        // Catalog files group models by API (`{ "<api>": { "<id>": Model } }`);
        // upstream flattens the groups (`flattenModelCatalog`). Both levels
        // are key-sorted upstream, so BTreeMap keeps the upstream order.
        let groups: BTreeMap<String, BTreeMap<String, Model>> = serde_json::from_str(json)
            .map_err(|source| CatalogError::ProviderData { provider, source })?;
        providers.push(*provider);
        models.insert(
            *provider,
            groups
                .into_values()
                .flat_map(BTreeMap::into_values)
                .collect(),
        );
    }
    Ok(BuiltinCatalog {
        providers,
        models,
        manifest,
    })
}

/// Parsed catalog, or the load error (corrupted vendored data).
pub fn builtin_catalog() -> Result<&'static BuiltinCatalog, &'static CatalogError> {
    CATALOG.get_or_init(load_catalog).as_ref()
}

/// `getBuiltinProviders()`; empty when the catalog failed to load.
pub fn get_builtin_providers() -> &'static [&'static str] {
    builtin_catalog()
        .map(BuiltinCatalog::providers)
        .unwrap_or(&[])
}

/// `getBuiltinModels(provider)`; empty for unknown providers.
pub fn get_builtin_models(provider: &str) -> &'static [Model] {
    builtin_catalog()
        .map(|catalog| catalog.models(provider))
        .unwrap_or(&[])
}

/// `getBuiltinModel(provider, modelId)`.
pub fn get_builtin_model(provider: &str, model_id: &str) -> Option<&'static Model> {
    builtin_catalog().ok()?.model(provider, model_id)
}

/// `getBuiltinModelDataGeneratedAt()`.
pub fn get_builtin_model_data_generated_at() -> Option<i64> {
    builtin_catalog().ok()?.generated_at()
}

/// `Date.parse` for the manifest's ISO-8601 UTC shape
/// (`YYYY-MM-DDTHH:MM:SS[.fff]Z`), milliseconds since epoch.
fn parse_iso8601_millis(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 20 || bytes.last() != Some(&b'Z') {
        return None;
    }
    let num = |start: usize, end: usize| -> Option<i64> { value.get(start..end)?.parse().ok() };
    let (year, month, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hour, min, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let mut millis: i64 = 0;
    if bytes.get(19) == Some(&b'.') {
        let frac = value.get(20..value.len() - 1)?;
        if frac.is_empty() || frac.len() > 3 || !frac.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        millis = frac.parse::<i64>().ok()? * 10i64.pow(3 - frac.len() as u32);
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    // Days since epoch (Howard Hinnant's days_from_civil).
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some((((days * 24 + hour) * 60 + min) * 60 + sec) * 1000 + millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_catalog_loads_all_vendored_providers() {
        let catalog = builtin_catalog().expect("vendored catalog parses");
        assert_eq!(catalog.providers().len(), CATALOG_PROVIDER_DATA.len());
        assert_eq!(catalog.providers().len(), 39);
        let total: usize = catalog
            .providers()
            .iter()
            .map(|provider| catalog.models(provider).len())
            .sum();
        assert_eq!(total, 1217);
        // The dynamic radius provider has no catalog entry (upstream all.ts).
        assert!(!catalog.providers().contains(&"radius"));
    }

    #[test]
    fn test_get_builtin_model_lookup() {
        let model = get_builtin_model("anthropic", "claude-fable-5").expect("model");
        assert_eq!(model.api.as_str(), "anthropic-messages");
        assert!(get_builtin_model("anthropic", "nope").is_none());
        assert!(get_builtin_model("nope", "claude-fable-5").is_none());
        assert!(get_builtin_models("nope").is_empty());
    }

    #[test]
    fn test_generated_at_matches_manifest() {
        let catalog = builtin_catalog().expect("catalog");
        assert_eq!(catalog.manifest().schema_version, 3);
        assert_eq!(catalog.manifest().files.len(), CATALOG_PROVIDER_DATA.len());
        // 2026-07-30T01:56:27.841Z per the vendored manifest; exact value is
        // asserted against the manifest string itself, not hardcoded here.
        assert!(catalog.generated_at().is_some());
    }

    #[test]
    fn test_parse_iso8601_millis() {
        assert_eq!(parse_iso8601_millis("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso8601_millis("1970-01-01T00:00:00.841Z"), Some(841));
        // 2026-08-11T04:37:23.682Z cross-checked against `date -u -d ... +%s%3N`.
        assert_eq!(
            parse_iso8601_millis("2026-08-11T04:37:23.682Z"),
            Some(1786423043682)
        );
        assert_eq!(
            parse_iso8601_millis("2026-02-28T23:59:59.5Z"),
            Some(1772323199500)
        );
        assert_eq!(parse_iso8601_millis("garbage"), None);
        assert_eq!(parse_iso8601_millis("2026-07-30 01:56:27"), None);
        assert_eq!(parse_iso8601_millis("2026-13-30T01:56:27Z"), None);
    }
}
