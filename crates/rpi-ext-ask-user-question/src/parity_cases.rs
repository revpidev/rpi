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

/// TE31 `preview` group replay: the pure preview layout/box functions over
/// the same fixture inputs the upstream leg drives.
pub fn replay_preview_case(input: &Value) -> Value {
    let kind = input.get("fn").and_then(Value::as_str).unwrap_or("");
    let number = |key: &str| input.get(key).and_then(Value::as_u64).unwrap_or_default() as usize;
    let items: Vec<QuestionItem> = input
        .get("items")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| serde_json::from_value(item.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
    let items_by_tab: Vec<Vec<QuestionItem>> = input
        .get("itemsByTab")
        .and_then(Value::as_array)
        .map(|tabs| {
            tabs.iter()
                .map(|tab| {
                    tab.as_array()
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(|item| serde_json::from_value(item.clone()).ok())
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default();
    let question = |key: &str| -> Option<QuestionData> {
        input
            .get(key)
            .and_then(|value| serde_json::from_value(value.clone()).ok())
    };
    let questions: Vec<QuestionData> = input
        .get("questions")
        .and_then(Value::as_array)
        .map(|questions| {
            questions
                .iter()
                .filter_map(|q| serde_json::from_value(q.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
    let tabs: Vec<bool> = input
        .get("tabs")
        .and_then(Value::as_array)
        .map(|tabs| {
            tabs.iter()
                .map(|tab| {
                    tab.get("multiSelect")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    let lines: Vec<String> = input
        .get("lines")
        .and_then(Value::as_array)
        .map(|lines| {
            lines
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    match kind {
        "decideLayout" => serde_json::json!({
            "mode": crate::view::preview::decide_layout(number("terminalWidth"), number("paneWidth")).as_str(),
        }),
        "adaptiveLeftWidth" => serde_json::json!({
            "left": crate::view::preview::adaptive_left_width(
                &items,
                number("totalForNumbering"),
                number("paneWidth"),
            ),
        }),
        "crossTabMaxLeftWidth" => serde_json::json!({
            "left": cross_tab_max_left_width_stub(&tabs, &items_by_tab, number("paneWidth")),
        }),
        "previewSourceWidth" => serde_json::json!({
            "width": crate::view::preview::preview_source_width(
                &question("question").unwrap_or_else(empty_question),
            ),
        }),
        "crossTabPreviewBudget" => serde_json::json!({
            "budget": crate::view::preview::cross_tab_preview_budget(&questions, number("paneWidth")),
        }),
        "crossTabLeftWidthWithDonation" => serde_json::json!({
            "left": cross_tab_donation_stub(&tabs, &items_by_tab, &questions, number("paneWidth")),
        }),
        "columnWidths" => {
            let (left_width, right_width, gap) =
                crate::view::preview::column_widths(number("paneWidth"), number("adaptiveLeft"));
            serde_json::json!({ "leftWidth": left_width, "rightWidth": right_width, "gap": gap })
        }
        "bodyWidths" => {
            let mode = match input.get("mode").and_then(Value::as_str) {
                Some("stacked") => crate::view::preview::PreviewLayoutMode::Stacked,
                _ => crate::view::preview::PreviewLayoutMode::SideBySide,
            };
            let (options_width, preview_width) = crate::view::preview::body_widths(
                number("paneWidth"),
                mode,
                number("adaptiveLeft"),
            );
            serde_json::json!({ "optionsWidth": options_width, "previewWidth": preview_width })
        }
        "constants" => serde_json::json!({
            "PREVIEW_MIN_WIDTH": crate::view::preview::PREVIEW_MIN_WIDTH,
            "PREVIEW_COLUMN_GAP": crate::view::preview::PREVIEW_COLUMN_GAP,
            "PREVIEW_PADDING_LEFT": crate::view::preview::PREVIEW_PADDING_LEFT,
            "STACKED_GAP_ROWS": crate::view::preview::STACKED_GAP_ROWS,
            "MIN_LEFT": crate::view::preview::MIN_LEFT,
            "MAX_LEFT_RATIO": crate::view::preview::MAX_LEFT_RATIO,
            "MIN_PREVIEW_WIDTH": crate::view::preview::MIN_PREVIEW_WIDTH,
            "CONFIRMED_OVERHEAD": crate::view::preview::CONFIRMED_OVERHEAD,
            "BORDER_VERTICAL_OVERHEAD": crate::view::preview::BORDER_VERTICAL_OVERHEAD,
            "BORDER_HORIZONTAL_OVERHEAD": crate::view::preview::BORDER_HORIZONTAL_OVERHEAD,
            "BORDER_INNER_PADDING_HORIZONTAL": crate::view::preview::BORDER_INNER_PADDING_HORIZONTAL,
            "BOX_MIN_CONTENT_WIDTH": crate::view::preview::BOX_MIN_CONTENT_WIDTH,
        }),
        "stripFenceMarkers" => serde_json::json!({
            "lines": crate::view::preview::strip_fence_markers(&lines),
        }),
        "renderBorderedBox" => {
            let identity = |text: &str| text.to_owned();
            serde_json::json!({
                "lines": crate::view::preview::render_bordered_box(
                    &lines,
                    number("width"),
                    identity,
                    number("hidden"),
                ),
            })
        }
        "computeBoxDimensions" => {
            let (inner_width, box_width) =
                crate::view::preview::compute_box_dimensions(&lines, number("maxInnerWidth"));
            serde_json::json!({ "innerWidth": inner_width, "boxWidth": box_width })
        }
        _ => Value::Null,
    }
}

/// [`crate::view::preview::cross_tab_max_left_width`] with the JS leg's
/// `{multiSelect}`-only tab view (the Rust signature reads full questions;
/// the derivation is identical — only the label list matters).
fn cross_tab_max_left_width_stub(
    tabs: &[bool],
    items_by_tab: &[Vec<QuestionItem>],
    pane_width: usize,
) -> usize {
    // JS iterates `tabs.length`; `itemsByTab[i] ?? []` covers short lists.
    let mut max = crate::view::preview::MIN_LEFT;
    for index in 0..tabs.len() {
        let items = items_by_tab.get(index).cloned().unwrap_or_default();
        let tab_width = crate::view::preview::adaptive_left_width(&items, items.len(), pane_width);
        max = max.max(tab_width);
    }
    max
}

/// A minimal empty question (fixture fallback).
fn empty_question() -> QuestionData {
    QuestionData {
        question: String::new(),
        header: String::new(),
        options: Vec::new(),
        multi_select: None,
    }
}

/// [`crate::view::preview::cross_tab_left_width_with_donation`] over the
/// `{multiSelect}`-only tab view (same reasoning as
/// [`cross_tab_max_left_width_stub`]).
fn cross_tab_donation_stub(
    tabs: &[bool],
    items_by_tab: &[Vec<QuestionItem>],
    questions: &[QuestionData],
    pane_width: usize,
) -> usize {
    let _ = tabs;
    crate::view::preview::cross_tab_left_width_with_donation(questions, items_by_tab, pane_width)
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
