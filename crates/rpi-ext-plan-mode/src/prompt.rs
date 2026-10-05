//! `before_agent_start` prompt injection (TE43 FR-C; plugin 02 §7).
//!
//! Plan mode contributes a complete `promptGuidelines` array — the
//! current array plus the plan-oriented rules — and a `plan_mode` prompt
//! section carrying the current plan file path. The host merges the
//! returned `systemPromptOptions` onto the chained options (map fields
//! merge per key; `promptGuidelines` replaces wholesale), so the handler
//! always rebuilds the array from the value it observed (the "complete
//! array replacement" contract, 01 §4 R-PM-3.1). Leaving Plan mode — or
//! disabling `promptInjection` — removes exactly the contributed lines
//! and the section, leaving every other handler's options untouched.
//!
//! The handler is idempotent: it returns a payload only when the desired
//! state differs from the observed one, so the runner does not rebuild
//! the prompt on every turn for nothing.

use serde_json::{Value, json};

use crate::HostCall;
use crate::config;

/// The plan-oriented guideline lines contributed in Plan mode (01 §4
/// R-PM-3.1, the "research → plan → approve" shape).
pub const PLAN_GUIDELINES: [&str; 4] = [
    "Plan mode is active: research the codebase and produce a structured plan before acting.",
    "Do not modify files or run mutating commands — write tools are disabled until the plan is approved.",
    "When the plan is complete, write it to the plan file with the write_plan tool.",
    "write_plan asks the user to approve, revise, or abandon the plan; only continue executing after approval.",
];

/// The dedicated prompt section tag for the plan file path.
pub const PLAN_SECTION: &str = "plan_mode";

/// Read current `promptGuidelines` from an event options object.
fn current_guidelines(options: &Value) -> Vec<String> {
    options
        .get("promptGuidelines")
        .and_then(Value::as_array)
        .map(|lines| {
            lines
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Read the current `plan_mode` section text.
fn current_section(options: &Value) -> Option<String> {
    options
        .get("sections")
        .and_then(|sections| sections.get(PLAN_SECTION))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Append the plan guidelines to `base`, preserving existing order and
/// never duplicating a line.
pub fn with_plan_guidelines(base: &[String]) -> Vec<String> {
    let mut next = base.to_vec();
    for line in PLAN_GUIDELINES {
        if !next.iter().any(|entry| entry == line) {
            next.push(line.to_owned());
        }
    }
    next
}

/// Remove the plan guidelines from `base`.
pub fn without_plan_guidelines(base: &[String]) -> Vec<String> {
    base.iter()
        .filter(|line| !PLAN_GUIDELINES.contains(&line.as_str()))
        .cloned()
        .collect()
}

/// The plan section text for `path`.
pub fn plan_section_text(path: &str) -> String {
    format!(
        "Plan file: {path}\nWrite the complete plan there with the write_plan tool. Revisions overwrite the same file; the user reviews it when write_plan returns."
    )
}

/// Handle one `before_agent_start` payload: reconcile the boundary first
/// (so a new tool surface is re-tightened before the request), then
/// return the injection payload (or `Null` when nothing changes).
pub fn handle_before_agent_start(host: &dyn HostCall, payload: &Value) -> Value {
    let plan = crate::mode::reconcile(host);
    let injection_enabled = config::load_config().prompt_injection;
    let options = payload
        .get("systemPromptOptions")
        .cloned()
        .unwrap_or(Value::Null);
    let current = current_guidelines(&options);
    let current_section = current_section(&options);

    if plan && injection_enabled {
        let Some(path) = crate::mode::current_plan_path(host) else {
            return Value::Null;
        };
        let text = plan_section_text(&path);
        let next = with_plan_guidelines(&current);
        if next == current && current_section.as_deref() == Some(text.as_str()) {
            return Value::Null;
        }
        return json!({
            "systemPromptOptions": {
                "promptGuidelines": next,
                "sections": { PLAN_SECTION: text },
            }
        });
    }

    // Default mode (or injection disabled): retract the contribution only
    // when the chained options currently carry it.
    let had_guidelines = current
        .iter()
        .any(|line| PLAN_GUIDELINES.contains(&line.as_str()));
    if !had_guidelines && current_section.is_none() {
        return Value::Null;
    }
    let next = without_plan_guidelines(&current);
    json!({
        "systemPromptOptions": {
            "promptGuidelines": next,
            "sections": { PLAN_SECTION: Value::Null },
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n;

    #[test]
    fn guidelines_append_without_duplicates() {
        let base = vec!["keep me".to_owned(), PLAN_GUIDELINES[0].to_owned()];
        let next = with_plan_guidelines(&base);
        assert_eq!(next[0], "keep me");
        assert_eq!(next.len(), base.len() + PLAN_GUIDELINES.len() - 1);
        assert_eq!(with_plan_guidelines(&next), next, "idempotent");
    }

    #[test]
    fn guidelines_removal_keeps_foreign_lines() {
        let base = vec![
            "foreign".to_owned(),
            PLAN_GUIDELINES[0].to_owned(),
            "also foreign".to_owned(),
        ];
        assert_eq!(
            without_plan_guidelines(&base),
            vec!["foreign", "also foreign"]
        );
    }

    #[test]
    fn section_text_carries_the_path() {
        let text = plan_section_text("/w/.rpi/plans/s-1.md");
        assert!(text.contains("/w/.rpi/plans/s-1.md"));
        assert!(text.contains("write_plan"));
    }

    #[test]
    fn plan_section_tag_matches_the_host_name_rule() {
        assert_eq!(PLAN_SECTION, "plan_mode");
        let valid = PLAN_SECTION
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
        assert!(
            valid,
            "lowercase/underscore tag accepted by the host builder"
        );
    }

    #[test]
    fn i18n_tool_snippet_is_stable() {
        assert!(i18n::TOOL_PROMPT_SNIPPET.contains("write_plan(content)"));
    }
}
#[cfg(test)]
mod injection_tests {
    use super::*;
    use crate::TEST_LOCK;
    use crate::test_host::SessionFakeHost;
    use serde_json::json;

    fn serialized() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn plan_host() -> SessionFakeHost {
        let host = SessionFakeHost::new();
        host.register_tool(crate::config::WRITE_PLAN_TOOL, "direct", false);
        host
    }

    #[test]
    fn plan_mode_injects_the_guidelines_and_the_plan_path() {
        let _guard = serialized();
        crate::__reset_state();
        let host = plan_host();
        host.set_mode("plan");
        let payload = json!({"systemPromptOptions": {"promptGuidelines": ["foreign line"]}});
        let result = handle_before_agent_start(&host, &payload);
        let guidelines = result["systemPromptOptions"]["promptGuidelines"]
            .as_array()
            .expect("guidelines");
        assert_eq!(guidelines[0], "foreign line", "base array is kept");
        for line in PLAN_GUIDELINES {
            assert!(guidelines.iter().any(|entry| entry == line), "{line}");
        }
        let section = result["systemPromptOptions"]["sections"][PLAN_SECTION]
            .as_str()
            .expect("section");
        assert!(
            section.contains("/work/cwd/.rpi/plans/s-1-1.md"),
            "{section}"
        );
    }

    #[test]
    fn already_injected_options_answer_null() {
        let _guard = serialized();
        crate::__reset_state();
        let host = plan_host();
        host.set_mode("plan");
        let base = json!({"promptGuidelines": []});
        let first = handle_before_agent_start(&host, &json!({"systemPromptOptions": base}));
        let second = handle_before_agent_start(
            &host,
            &json!({"systemPromptOptions": first["systemPromptOptions"]}),
        );
        assert_eq!(second, Value::Null, "idempotent");
    }

    #[test]
    fn default_mode_retracts_the_injection() {
        let _guard = serialized();
        crate::__reset_state();
        let host = plan_host();
        let payload = json!({"systemPromptOptions": {
            "promptGuidelines": ["foreign", PLAN_GUIDELINES[0]],
            "sections": {PLAN_SECTION: "stale"},
        }});
        let result = handle_before_agent_start(&host, &payload);
        assert_eq!(
            result["systemPromptOptions"]["promptGuidelines"],
            json!(["foreign"])
        );
        assert_eq!(
            result["systemPromptOptions"]["sections"][PLAN_SECTION],
            Value::Null,
            "an explicit null clears the section"
        );
    }

    #[test]
    fn default_mode_without_a_contribution_answers_null() {
        let _guard = serialized();
        crate::__reset_state();
        let host = plan_host();
        let payload = json!({"systemPromptOptions": {"promptGuidelines": ["foreign"]}});
        assert_eq!(handle_before_agent_start(&host, &payload), Value::Null);
    }

    #[test]
    fn prompt_injection_disabled_only_applies_the_boundary() {
        let _guard = serialized();
        crate::__reset_state();
        crate::config::set_test_config(Some(Some("promptInjection = false".to_owned())));
        let host = plan_host();
        host.set_mode("plan");
        let payload = json!({"systemPromptOptions": {"promptGuidelines": []}});
        assert_eq!(handle_before_agent_start(&host, &payload), Value::Null);
        assert!(
            crate::mode::state_for_test("s-1").in_plan,
            "boundary applied"
        );
    }
}
