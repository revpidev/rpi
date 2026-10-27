//! OpenRouter `/models/user` response types and pricing conversion.
//!
//! Port of `packages/ai/src/providers/openrouter-api.ts` @ pi 1.4.0
//! (4a080252f, ee409a85f).

use serde::Deserialize;

use crate::types::{ModelCost, ModelCostRates, ModelCostTier};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct OpenRouterPricingOverride {
    pub min_prompt_tokens: Option<u64>,
    pub utc_start: Option<u32>,
    pub utc_end: Option<u32>,
    pub utc_days: Option<Vec<String>>,
    pub prompt: Option<String>,
    pub completion: Option<String>,
    pub input_cache_read: Option<String>,
    pub input_cache_write: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct OpenRouterPricing {
    pub prompt: Option<String>,
    pub completion: Option<String>,
    pub input_cache_read: Option<String>,
    pub input_cache_write: Option<String>,
    pub overrides: Option<Vec<OpenRouterPricingOverride>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct OpenRouterTopProvider {
    pub context_length: Option<u32>,
    pub max_completion_tokens: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct OpenRouterModel {
    pub id: String,
    pub context_length: Option<u32>,
    pub top_provider: Option<OpenRouterTopProvider>,
    pub pricing: Option<OpenRouterPricing>,
}

fn per_million(value: Option<&str>, fallback: f64) -> f64 {
    let per_token = value
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0);
    let Some(per_token) = per_token else {
        return fallback;
    };
    let scaled = per_token * 1_000_000.0;
    (scaled * 1_000_000.0).round() / 1_000_000.0
}

/// Convert OpenRouter's per-token decimal strings to USD per million tokens.
/// Missing, invalid, and negative base prices are zero. Prompt-length tiers
/// replace the full request price; time-based overrides are not representable.
pub fn open_router_cost(pricing: Option<&OpenRouterPricing>) -> ModelCost {
    let Some(pricing) = pricing else {
        return ModelCost::default();
    };
    let rates = ModelCostRates {
        input: per_million(pricing.prompt.as_deref(), 0.0),
        output: per_million(pricing.completion.as_deref(), 0.0),
        cache_read: per_million(pricing.input_cache_read.as_deref(), 0.0),
        cache_write: per_million(pricing.input_cache_write.as_deref(), 0.0),
    };
    let tiers: Vec<ModelCostTier> = pricing
        .overrides
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .filter_map(|override_| {
            if override_.min_prompt_tokens.is_none()
                || override_.utc_start.is_some()
                || override_.utc_end.is_some()
                || override_.utc_days.is_some()
            {
                return None;
            }
            Some(ModelCostTier {
                input_tokens_above: override_.min_prompt_tokens?,
                rates: ModelCostRates {
                    input: per_million(override_.prompt.as_deref(), rates.input),
                    output: per_million(override_.completion.as_deref(), rates.output),
                    cache_read: per_million(
                        override_.input_cache_read.as_deref(),
                        rates.cache_read,
                    ),
                    cache_write: per_million(
                        override_.input_cache_write.as_deref(),
                        rates.cache_write,
                    ),
                },
            })
        })
        .collect();
    if tiers.is_empty() {
        ModelCost { rates, tiers: None }
    } else {
        ModelCost {
            rates,
            tiers: Some(tiers),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_prices_and_missing_rates_are_zero() {
        let pricing = OpenRouterPricing {
            prompt: Some("-1".to_owned()),
            completion: Some("not-a-price".to_owned()),
            ..Default::default()
        };
        assert_eq!(open_router_cost(Some(&pricing)), ModelCost::default());
    }

    #[test]
    fn prompt_length_overrides_become_request_wide_tiers() {
        let pricing = OpenRouterPricing {
            prompt: Some("0.000001".to_owned()),
            completion: Some("0.000002".to_owned()),
            overrides: Some(vec![OpenRouterPricingOverride {
                min_prompt_tokens: Some(200_000),
                prompt: Some("0.000003".to_owned()),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let cost = open_router_cost(Some(&pricing));
        assert_eq!(cost.rates.input, 1.0);
        assert_eq!(cost.rates.output, 2.0);
        let tiers = cost.tiers.expect("tiers");
        assert_eq!(tiers.len(), 1);
        assert_eq!(tiers[0].input_tokens_above, 200_000);
        assert_eq!(tiers[0].rates.input, 3.0);
        assert_eq!(tiers[0].rates.output, 2.0);
    }
}
