//! Edit-distance helpers shared by the settings/agent validators.
//!
//! Port of pi-subagents `src/shared/edit-distance.ts` @ v0.83.0 (`3a63e0ad`):
//! used by the `agentOverrides` unknown-field hint (#3064) and the
//! `subagent` options-object hint (#2786). Distances are measured on Unicode
//! scalar values; upstream's `closestMatch` compares case-insensitively and
//! accepts a distance of at most a quarter of the candidate's length, or a
//! single adjacent transposition.

/// Levenshtein distance between two strings (`editDistance`).
pub fn edit_distance(left: &str, right: &str) -> usize {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    for (left_index, left_char) in left.iter().enumerate() {
        let mut diagonal = previous[0];
        previous[0] = left_index + 1;
        for right_index in 1..=right.len() {
            let above = previous[right_index];
            previous[right_index] = if *left_char == right[right_index - 1] {
                diagonal
            } else {
                diagonal.min(above).min(previous[right_index - 1]) + 1
            };
            diagonal = above;
        }
    }
    previous[right.len()]
}

/// Whether `left` and `right` differ by exactly one adjacent transposition
/// (`hasSingleAdjacentTransposition`).
pub fn has_single_adjacent_transposition(left: &str, right: &str) -> bool {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    if left.len() != right.len() {
        return false;
    }
    let Some(mismatch) = left.iter().zip(&right).position(|(a, b)| a != b) else {
        return false;
    };
    mismatch + 1 < left.len()
        && left[mismatch] == right[mismatch + 1]
        && left[mismatch + 1] == right[mismatch]
        && left[mismatch + 2..] == right[mismatch + 2..]
}

/// The closest candidate that is a likely typo of `requested`, compared
/// case-insensitively (`closestMatch`).
pub fn closest_match<'a, I>(requested: &str, candidates: I) -> Option<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let lower = requested.to_lowercase();
    candidates
        .into_iter()
        .map(|candidate| {
            let candidate_lower = candidate.to_lowercase();
            let distance = edit_distance(&lower, &candidate_lower);
            (candidate, distance)
        })
        .filter(|(candidate, distance)| {
            *distance <= std::cmp::max(1, candidate.chars().count() / 4)
                || has_single_adjacent_transposition(&lower, &candidate.to_lowercase())
        })
        .min_by(
            |(left_candidate, left_distance), (right_candidate, right_distance)| {
                left_distance
                    .cmp(right_distance)
                    .then_with(|| left_candidate.cmp(right_candidate))
            },
        )
        .map(|(candidate, _)| candidate.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_distance_matches_the_dynamic_programming_definition() {
        assert_eq!(edit_distance("", ""), 0);
        assert_eq!(edit_distance("abc", "abc"), 0);
        assert_eq!(edit_distance("modle", "model"), 2);
        assert_eq!(edit_distance("hgih", "high"), 2);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("т", "n"), 1);
    }

    #[test]
    fn single_adjacent_transposition_is_recognized() {
        assert!(has_single_adjacent_transposition("modle", "model"));
        assert!(has_single_adjacent_transposition("hgih", "high"));
        assert!(!has_single_adjacent_transposition("modle", "models"));
        assert!(!has_single_adjacent_transposition("model", "model"));
        assert!(has_single_adjacent_transposition("mdoel", "model"));
        assert!(!has_single_adjacent_transposition("modle", "modeel"));
    }

    #[test]
    fn closest_match_prefers_the_nearest_case_insensitive_candidate() {
        let fields = ["model", "thinking", "systemPrompt", "tools", "extensions"];
        assert_eq!(closest_match("modle", fields), Some("model".to_string()));
        assert_eq!(
            closest_match("THINKIN", fields),
            Some("thinking".to_string())
        );
        assert_eq!(closest_match("tool", fields), Some("tools".to_string()));
        assert_eq!(closest_match("zzzz", fields), None);
        assert_eq!(closest_match("x", [] as [&str; 0]), None);
    }
}
