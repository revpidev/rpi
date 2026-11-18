//! Port of `packages/ai/src/utils/openrouter-cost.ts` @ pi v1.5.2
//! (`b4afdddf`).

use serde_json::Value;

use crate::types::{Model, Usage};

pub fn is_openrouter_model(model: &Model) -> bool {
    model.provider == "openrouter" || model.base_url.contains("openrouter.ai")
}

fn valid_cost(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .filter(|value| value.is_finite() && *value >= 0.0)
}

/// Replace the catalog estimate only when complete, valid accounting exists.
pub fn apply_openrouter_reported_cost(usage: &mut Usage, reported: &Value) {
    let Some(cost) = valid_cost(&reported["cost"]) else {
        return;
    };
    match reported["is_byok"].as_bool() {
        Some(false) => {
            usage.cost.total = cost;
            usage.cost.total_from_provider = Some(true);
        }
        Some(true) => {
            let details = &reported["cost_details"];
            if !details.is_object() {
                return;
            }
            let upstream = match valid_cost(&details["upstream_inference_cost"]) {
                Some(value) => value,
                None => {
                    let Some(prompt) = valid_cost(&details["upstream_inference_prompt_cost"])
                    else {
                        return;
                    };
                    let Some(completions) =
                        valid_cost(&details["upstream_inference_completions_cost"])
                    else {
                        return;
                    };
                    prompt + completions
                }
            };
            usage.cost.total = cost + upstream;
            usage.cost.total_from_provider = Some(true);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn reported_cost_replaces_catalog_only_with_complete_accounting() {
        let mut usage = Usage::default();
        apply_openrouter_reported_cost(
            &mut usage,
            &json!({"cost": 1.25, "is_byok": false, "cost_details": null}),
        );
        assert_eq!(usage.cost.total, 1.25);
        assert_eq!(usage.cost.total_from_provider, Some(true));

        let mut byok = Usage::default();
        apply_openrouter_reported_cost(
            &mut byok,
            &json!({
                "cost": 0.25,
                "is_byok": true,
                "cost_details": {
                    "upstream_inference_cost": null,
                    "upstream_inference_prompt_cost": 0.5,
                    "upstream_inference_completions_cost": 0.75
                }
            }),
        );
        assert_eq!(byok.cost.total, 1.5);
        assert_eq!(byok.cost.total_from_provider, Some(true));

        let mut invalid = Usage::default();
        apply_openrouter_reported_cost(
            &mut invalid,
            &json!({"cost": 1.0, "is_byok": true, "cost_details": null}),
        );
        assert_eq!(invalid.cost.total, 0.0);
        assert_eq!(invalid.cost.total_from_provider, None);
    }
}
