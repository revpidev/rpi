//! Model search text helpers — port of
//! `packages/coding-agent/src/modes/interactive/model-search.ts`
//! @ pi 0.82.1 (2efa728).
//!
//! Intentional differences:
//! - The port passes the item by reference; upstream takes the object by
//!   value (it is destructured into `{ id, provider }` immediately either
//!   way).

/// `ModelSearchItem` (model-search.ts:1-5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSearchItem {
    pub id: String,
    pub provider: String,
    pub name: Option<String>,
}

/// `getModelSearchText` (model-search.ts:7-11).
pub fn get_model_search_text(item: &ModelSearchItem) -> String {
    let name = item
        .name
        .as_deref()
        .map(|name| format!(" {name}"))
        .unwrap_or_default();
    format!(
        "{} {} {}/{} {} {}{}",
        item.id, item.provider, item.provider, item.id, item.provider, item.id, name
    )
}

/// `getModelSelectorSearchText` (model-search.ts:17-20): the /model selector
/// search should rank exact provider-prefixed queries before proxy-provider
/// IDs like openrouter/openai/gpt-5, so the bare model ID is kept out of the
/// leading position.
pub fn get_model_selector_search_text(item: &ModelSearchItem) -> String {
    let name = item
        .name
        .as_deref()
        .map(|name| format!(" {name}"))
        .unwrap_or_default();
    format!(
        "{} {}/{} {} {}{}",
        item.provider, item.provider, item.id, item.provider, item.id, name
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item() -> ModelSearchItem {
        ModelSearchItem {
            id: "gpt-5".to_string(),
            provider: "openrouter".to_string(),
            name: Some("GPT-5".to_string()),
        }
    }

    #[test]
    fn search_text_lists_id_provider_and_proxy_id() {
        assert_eq!(
            get_model_search_text(&item()),
            "gpt-5 openrouter openrouter/gpt-5 openrouter gpt-5 GPT-5"
        );
    }

    #[test]
    fn search_text_omits_missing_name() {
        let item = ModelSearchItem {
            name: None,
            ..item()
        };
        assert_eq!(
            get_model_search_text(&item),
            "gpt-5 openrouter openrouter/gpt-5 openrouter gpt-5"
        );
    }

    #[test]
    fn selector_search_text_leads_with_provider() {
        // model-search.ts:13-16: the /model selector search keeps the bare
        // model ID out of the leading position so provider-prefixed queries
        // rank before proxy-provider IDs.
        assert_eq!(
            get_model_selector_search_text(&item()),
            "openrouter openrouter/gpt-5 openrouter gpt-5 GPT-5"
        );
        let no_name = ModelSearchItem {
            name: None,
            ..item()
        };
        assert_eq!(
            get_model_selector_search_text(&no_name),
            "openrouter openrouter/gpt-5 openrouter gpt-5"
        );
    }
}
