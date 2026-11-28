//! Numbered-step completion tracking after an approved plan.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use regex::{Regex, RegexBuilder};
use serde_json::{Value, json};

use crate::HostCall;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TodoItem {
    pub step: usize,
    pub text: String,
    pub completed: bool,
}

#[derive(Clone, Debug, Default)]
struct ProgressState {
    todos: Vec<TodoItem>,
}

static PROGRESS: OnceLock<Mutex<HashMap<String, ProgressState>>> = OnceLock::new();

fn progress() -> &'static Mutex<HashMap<String, ProgressState>> {
    PROGRESS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn regex(pattern: &str) -> Regex {
    RegexBuilder::new(pattern)
        .case_insensitive(true)
        .multi_line(true)
        .build()
        .expect("static plan progress pattern")
}

pub fn clean_step_text(text: &str) -> String {
    let cleaned = regex(r"\*{1,2}([^*]+)\*{1,2}").replace_all(text, "$1");
    let cleaned = regex(r"`([^`]+)`").replace_all(&cleaned, "$1");
    let cleaned = regex(
        r"^(?:Use|Run|Execute|Create|Write|Read|Check|Verify|Update|Modify|Add|Remove|Delete|Install)\s+(?:the\s+)?",
    )
    .replace(&cleaned, "");
    let cleaned = regex(r"\s+").replace_all(&cleaned, " ");
    let cleaned = cleaned.trim();
    let mut characters = cleaned.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
        None => String::new(),
    }
}

pub fn extract_todo_items(message: &str) -> Vec<TodoItem> {
    let Some(header) = regex(r"(?m)^\*{0,2}Plan:\*{0,2}[ \t]*$")
        .find_iter(message)
        .last()
    else {
        return Vec::new();
    };
    let section = &message[header.end()..];
    let mut items = Vec::new();
    for captures in regex(r"(?m)^\s*(\d+)[.)]\s+(.+)$").captures_iter(section) {
        let text = captures[2].trim().trim_end_matches('*').trim();
        if text.chars().count() <= 5 || text.starts_with('/') || text.starts_with('-') {
            continue;
        }
        let cleaned = clean_step_text(text);
        if cleaned.chars().count() > 3 {
            items.push(TodoItem {
                step: items.len() + 1,
                text: cleaned,
                completed: false,
            });
        }
    }
    items
}

pub fn extract_done_steps(message: &str) -> Vec<usize> {
    let mut steps = Vec::new();
    let mut seen = HashSet::new();
    for pattern in [
        r"\[DONE\s*:?\s*(\d+)\]",
        r"\bDone\s*:\s*(\d+)\b",
        r"\bStep\s*(\d+)\s+(?:is\s+)?(?:done|complete|completed|finished)\b",
    ] {
        for captures in regex(pattern).captures_iter(message) {
            if let Ok(step) = captures[1].parse::<usize>()
                && seen.insert(step)
            {
                steps.push(step);
            }
        }
    }
    steps
}

pub fn all_steps_complete(message: &str) -> bool {
    regex(r"(?:all|every)\s+steps?\s+(?:are\s+|is\s+)?(?:done|complete|completed|finished)\b")
        .is_match(message)
        || regex(r"\b(?:the\s+)?plan\s+(?:is\s+)?(?:complete|completed|done)\b").is_match(message)
}

fn assistant_text(message: &Value) -> Option<String> {
    (message.get("role").and_then(Value::as_str) == Some("assistant")).then(|| {
        message
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|block| match block.get("type").and_then(Value::as_str) {
                Some("text") => block.get("text").and_then(Value::as_str),
                Some("thinking") => block.get("thinking").and_then(Value::as_str),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    })
}

fn event_assistant_text(payload: &Value, final_message_only: bool) -> Option<String> {
    if final_message_only {
        payload
            .get("messages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .rev()
            .find_map(assistant_text)
    } else {
        payload.get("message").and_then(assistant_text)
    }
}

fn mark(todos: &mut [TodoItem], text: &str) {
    for step in extract_done_steps(text) {
        if let Some(item) = todos.iter_mut().find(|item| item.step == step) {
            item.completed = true;
        }
    }
}

pub fn begin(host: &dyn HostCall, plan: &str) {
    let todos = extract_todo_items(plan);
    let mut states = progress().lock().unwrap_or_else(|error| error.into_inner());
    if todos.is_empty() {
        states.remove(&crate::host::sid_of(host));
    } else {
        states.insert(crate::host::sid_of(host), ProgressState { todos });
    }
}

pub fn record_turn(host: &dyn HostCall, payload: &Value) {
    let Some(text) = event_assistant_text(payload, false) else {
        return;
    };
    let mut states = progress().lock().unwrap_or_else(|error| error.into_inner());
    if let Some(state) = states.get_mut(&crate::host::sid_of(host)) {
        mark(&mut state.todos, &text);
    }
}

pub fn record_agent_end(host: &dyn HostCall, payload: &Value) -> Option<Value> {
    let text = event_assistant_text(payload, true).unwrap_or_default();
    let mut states = progress().lock().unwrap_or_else(|error| error.into_inner());
    let session = crate::host::sid_of(host);
    let state = states.get_mut(&session)?;
    mark(&mut state.todos, &text);
    if all_steps_complete(&text) {
        for item in &mut state.todos {
            item.completed = true;
        }
    }
    if state.todos.iter().any(|item| !item.completed) {
        return None;
    }
    let completed = state
        .todos
        .iter()
        .map(|item| format!("~~{}~~", item.text))
        .collect::<Vec<_>>()
        .join("\n");
    let state = states.remove(&session).expect("progress checked above");
    debug_assert!(state.todos.iter().all(|item| item.completed));
    let message = json!({
        "customType": "plan-complete",
        "content": format!("**Plan Complete!** ✓\n\n{completed}"),
        "display": true,
    });
    let options = json!({"deliverAs": "nextTurn"});
    if let Err(error) = host.call(
        "sendMessage",
        json!({"message": message, "options": options}),
    ) {
        tracing::warn!(%error, "rpi-plan-mode: completion message could not be queued");
    }
    Some(message)
}

pub fn forget(host: &dyn HostCall) {
    progress()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(&crate::host::sid_of(host));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TEST_LOCK;
    use crate::test_host::SessionFakeHost;

    #[test]
    fn extracts_formatted_long_steps_from_the_last_plan_section() {
        let long = format!(
            "**Read** `src/main.rs` and {} without truncation",
            "detail ".repeat(100)
        );
        let plan = format!(
            "Plan:\n1. Draft step\n\nText\n\n**Plan:**\n1. {long}\n2) Verify every edge case\n3. /not-a-step"
        );
        let todos = extract_todo_items(&plan);
        assert_eq!(todos.len(), 2);
        assert_eq!(todos[0].step, 1);
        assert!(todos[0].text.contains("without truncation"));
        assert!(!todos[0].text.contains("**"));
        assert!(todos[0].text.contains(&"detail ".repeat(100)));
        assert_eq!(todos[1].text, "Every edge case");
    }

    #[test]
    fn completion_markers_reject_negated_or_unrelated_steps() {
        assert_eq!(
            extract_done_steps(
                "[DONE:1] [Done 2] Done: 3\nStep 4 is complete\nStep 9 is not done\nStep 8 incomplete"
            ),
            [1, 2, 3, 4]
        );
        assert!(all_steps_complete("All steps are complete."));
        assert!(all_steps_complete("The plan is done."));
        assert!(!all_steps_complete("Not all steps are incomplete."));
    }

    #[test]
    fn thinking_markers_complete_without_an_extra_turn() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        crate::__reset_state();
        let host = SessionFakeHost::new();
        begin(
            &host,
            "Plan:\n1. Read the implementation\n2. Verify the result",
        );
        record_turn(
            &host,
            &json!({"message": {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "[DONE:1]"},
                {"type": "text", "text": "continuing"}
            ]}}),
        );
        let completion = record_agent_end(
            &host,
            &json!({"messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "Step 2 finished; all steps are complete"}
            ]}]}),
        )
        .expect("completion card");
        assert!(
            completion["content"]
                .as_str()
                .unwrap()
                .contains("Implementation")
        );
        assert!(completion["content"].as_str().unwrap().contains("Result"));
        let (_, arguments) = host
            .calls()
            .into_iter()
            .find(|(method, _)| method == "sendMessage")
            .expect("queued card");
        assert_eq!(arguments["options"], json!({"deliverAs": "nextTurn"}));
        assert!(arguments.get("triggerTurn").is_none());
        forget(&host);
    }
}
