//! Built-in model catalog (`@generated`, do not edit by hand).
//!
//! T03 placeholder: `build.rs` emits an empty catalog; the real generator
//! (models.dev data source, aligned with `packages/ai/scripts/generate-models.ts`
//! correction rules) and the remote overlay (`pir update --models`,
//! ETag/4h freshness) land in T13/T14 — design doc 模型目录 section.

include!(concat!(env!("OUT_DIR"), "/models_generated.rs"));

/// The built-in (generated) model catalog. Empty until T13/T14.
pub fn generated_models() -> &'static [crate::types::Model] {
    GENERATED_MODELS
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_generated_models_placeholder_is_empty() {
        assert!(super::generated_models().is_empty());
    }
}
