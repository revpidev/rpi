use std::cmp::Ordering;

pub const PREFERRED_PROVIDER_IDS: [&str; 2] = ["radius", "openrouter"];

pub fn is_preferred_provider(provider_id: &str) -> bool {
    PREFERRED_PROVIDER_IDS.contains(&provider_id)
}

fn preferred_rank(provider_id: &str) -> usize {
    PREFERRED_PROVIDER_IDS
        .iter()
        .position(|candidate| *candidate == provider_id)
        .unwrap_or(PREFERRED_PROVIDER_IDS.len())
}

pub fn compare_providers(
    left_id: &str,
    left_name: &str,
    right_id: &str,
    right_name: &str,
) -> Ordering {
    preferred_rank(left_id)
        .cmp(&preferred_rank(right_id))
        .then_with(|| left_name.cmp(right_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partners_precede_alphabetical_provider_names() {
        let mut providers = [
            ("zai", "Z.AI"),
            ("openrouter", "OpenRouter"),
            ("anthropic", "Anthropic"),
            ("radius", "Radius"),
        ];
        providers.sort_by(|left, right| compare_providers(left.0, left.1, right.0, right.1));
        assert_eq!(
            providers
                .iter()
                .map(|provider| provider.0)
                .collect::<Vec<_>>(),
            vec!["radius", "openrouter", "anthropic", "zai"]
        );
    }
}
