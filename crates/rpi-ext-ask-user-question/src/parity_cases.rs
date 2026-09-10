//! Fixture replay for the parity harness (`state` / `keys` groups).
//!
//! TE30 G3: `scripts/ask-user-question-parity/upstream-runner.mjs` computes
//! the same outputs from the pinned upstream modules, `run-parity.mjs` diffs
//! the two legs, and `tests/state_vectors.rs` freezes the Rust leg against the
//! committed `rust-state.jsonl` / `rust-keys.jsonl` so `cargo test` catches
//! regressions without Node.
//!
//! The [`crate::parity`] facade re-exports both entry points.

use serde_json::Value;

use crate::i18n::I18n;
use crate::state::build::{build_items_for_question, QuestionItem};
use crate::state::key_router::{route_key, Action, Keybindings, QuestionnaireRuntime};
use crate::state::reducer::{apply, state_from_json, ApplyContext};
use crate::tool::types::QuestionData;

/// Questions of a fixture case (`questions` is always present).
pub fn fixture_questions(input: &Value) -> Vec<QuestionData> {
    serde_json::from_value(input.get("questions").cloned().unwrap_or(Value::Null))
        .unwrap_or_default()
}

/// Per-tab row lists of a fixture case: explicit `itemsByTab` wins,
/// otherwise the canonical derivation (author options + sentinels) runs with
/// the English table — mirroring the upstream leg's `buildItemsForQuestion`
/// with the identity i18n fallback.
pub fn fixture_items(input: &Value, questions: &[QuestionData]) -> Vec<Vec<QuestionItem>> {
    if let Some(tabs) = input.get("itemsByTab").and_then(Value::as_array) {
        return tabs
            .iter()
            .map(|tab| serde_json::from_value(tab.clone()).unwrap_or_default())
            .collect();
    }
    let i18n = I18n::for_locale("en");
    questions
        .iter()
        .map(|question| build_items_for_question(question, &i18n))
        .collect()
}

/// `state` group replay: apply the fixture's action sequence and return one
/// `{state, effects}` step per action.
pub fn replay_state_case(input: &Value) -> Value {
    let questions = fixture_questions(input);
    let items = fixture_items(input, &questions);
    let mut state = state_from_json(&input.get("setup").cloned().unwrap_or(Value::Null));
    let ctx = ApplyContext {
        questions: &questions,
        items_by_tab: &items,
    };
    let actions = input
        .get("actions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut steps = Vec::new();
    for action in actions {
        let Ok(action) = serde_json::from_value::<Action>(action) else {
            steps.push(Value::Null);
            continue;
        };
        let result = apply(&state, &action, &ctx);
        steps.push(serde_json::json!({
            "state": result.state.snapshot(),
            "effects": result.effects,
        }));
        state = result.state;
    }
    serde_json::json!({ "steps": steps })
}

/// `keys` group replay: route each raw key through the same runtime the
/// session builds. Keys come from the case (`keys`) or the group-level
/// `keyMatrix` the caller passes in.
pub fn replay_keys_case(input: &Value, key_matrix: &[Value]) -> Value {
    let questions = fixture_questions(input);
    let items = fixture_items(input, &questions);
    let state = state_from_json(&input.get("setup").cloned().unwrap_or(Value::Null));
    let runtime_input = input.get("runtime").cloned().unwrap_or(Value::Null);
    let mut keybindings = Keybindings::pi_defaults();
    if let Some(bindings) = runtime_input.get("bindings").and_then(Value::as_object) {
        for (name, keys) in bindings {
            if let Ok(keys) = serde_json::from_value::<Vec<String>>(keys.clone()) {
                keybindings.set(name, keys);
            }
        }
    }
    let current_items = items
        .get(state.current_tab)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let runtime = QuestionnaireRuntime {
        keybindings: &keybindings,
        input_buffer: runtime_input
            .get("inputBuffer")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        can_move_input_up: runtime_input
            .get("canMoveInputUp")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        can_move_input_down: runtime_input
            .get("canMoveInputDown")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        questions: &questions,
        is_multi: questions.len() > 1,
        current_item: current_items.get(state.option_index),
        items: current_items,
        collapse_key: runtime_input
            .get("collapseKey")
            .and_then(Value::as_str)
            .unwrap_or("ctrl+]")
            .to_owned(),
    };
    let inputs = input
        .get("keys")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_else(|| key_matrix.to_vec());
    let actions: Vec<Value> = inputs
        .iter()
        .map(|data| {
            let data = data.as_str().unwrap_or("");
            serde_json::to_value(route_key(data, &state, &runtime)).unwrap_or(Value::Null)
        })
        .collect();
    serde_json::json!({ "actions": actions })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn question() -> Value {
        json!({
            "question": "Pick one",
            "header": "H",
            "options": [
                {"label": "A", "description": "a"},
                {"label": "B", "description": "b"}
            ]
        })
    }

    #[test]
    fn state_replay_returns_one_step_per_action() {
        let input = json!({
            "questions": [question()],
            "actions": [
                {"kind": "nav", "nextIndex": 1, "inputValue": ""},
                {"kind": "confirm", "answer": {"questionIndex": 0, "question": "Pick one", "kind": "option", "answer": "A"}}
            ]
        });
        let output = replay_state_case(&input);
        let steps = output["steps"].as_array().expect("steps");
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0]["state"]["optionIndex"], 1);
        assert_eq!(steps[1]["effects"][0]["kind"], "done");
    }

    #[test]
    fn keys_replay_uses_the_group_matrix_when_the_case_has_none() {
        let input = json!({ "questions": [question()] });
        let matrix = vec![json!("\r"), json!("x")];
        let output = replay_keys_case(&input, &matrix);
        let actions = output["actions"].as_array().expect("actions");
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0]["kind"], "confirm");
        assert_eq!(actions[1]["kind"], "ignore");
    }
}
