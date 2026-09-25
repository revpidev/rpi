//! Tool budget runtime (#2302 / c8450647+a586ef7e+41453ddf at v0.70):
//! per-child tool-call budgets with child-side enforcement and parent-side
//! terminal reporting.
//!
//! - [`validate_tool_budget_config`] — admission-time validation with the
//!   upstream error texts (`toolBudget.hard must be an integer >= 1.` …).
//! - Child-side enforcement — the parent carries the resolved budget in
//!   `RPI_SUBAGENT_TOOL_BUDGET`; the child extension subscribes to
//!   `tool_call`, counts every call, nudges at the soft cap
//!   (`sendUserMessage` steer), and blocks past the hard cap for tools in
//!   the block list (`block: "*" | [names]`, default
//!   `[read, grep, find, ls]`) with the exact runtime message.
//! - Parent-side terminal classification — [`is_tool_budget_blocked_message`]
//!   recognizes a hard-block reason generated for THIS budget and tool
//!   (full-message regex + embedded hard/tool attribution; incidental
//!   occurrences in ordinary tool output are rejected), and
//!   [`tool_budget_state`] renders the `ToolBudgetState` shape
//!   (`outcome: within-budget | soft-reached | hard-blocked`).

use serde_json::{json, Value};

/// `RPI_SUBAGENT_TOOL_BUDGET` — the resolved budget carried into the child:
/// `{"hard":N,"soft":M?,"block":"*"|[names]}`.
pub const TOOL_BUDGET_ENV: &str = "RPI_SUBAGENT_TOOL_BUDGET";

/// `DEFAULT_TOOL_BUDGET_BLOCK` (tool-budget.ts:4).
pub const DEFAULT_TOOL_BUDGET_BLOCK: [&str; 4] = ["read", "grep", "find", "ls"];

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedToolBudget {
    pub hard: u64,
    pub soft: Option<u64>,
    pub block: BlockList,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BlockList {
    All,
    Names(Vec<String>),
}

impl BlockList {
    fn to_json(&self) -> Value {
        match self {
            BlockList::All => json!("*"),
            BlockList::Names(names) => json!(names),
        }
    }
    fn contains(&self, tool: &str) -> bool {
        match self {
            BlockList::All => true,
            BlockList::Names(names) => names.iter().any(|name| name == tool),
        }
    }
}

/// `normalizeToolBudgetBlock` (tool-budget.ts:5-9).
pub fn normalize_tool_budget_block(block: Option<&Value>) -> BlockList {
    match block {
        Some(Value::String(raw)) if raw == "*" => BlockList::All,
        Some(Value::Array(items)) => {
            let mut names: Vec<String> = items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .collect();
            names.dedup();
            BlockList::Names(names)
        }
        _ => BlockList::Names(
            DEFAULT_TOOL_BUDGET_BLOCK
                .iter()
                .map(|name| name.to_string())
                .collect(),
        ),
    }
}

/// `validateToolBudgetConfig` (tool-budget.ts:11-38) — exact upstream texts.
pub fn validate_tool_budget_config(
    raw: Option<&Value>,
    label: &str,
) -> Result<Option<ResolvedToolBudget>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let Some(object) = raw.as_object() else {
        return Err(format!(
            "{label} must be an object with hard and optional soft/block."
        ));
    };
    let Some(hard) = object.get("hard").and_then(Value::as_u64) else {
        return Err(format!("{label}.hard must be an integer >= 1."));
    };
    if hard < 1 {
        return Err(format!("{label}.hard must be an integer >= 1."));
    }
    if let Some(soft_value) = object.get("soft") {
        let Some(soft) = soft_value.as_u64() else {
            return Err(format!(
                "{label}.soft must be an integer >= 1 when provided."
            ));
        };
        if soft < 1 {
            return Err(format!(
                "{label}.soft must be an integer >= 1 when provided."
            ));
        }
        if soft > hard {
            return Err(format!("{label}.soft must be <= {label}.hard."));
        }
    }
    if let Some(block) = object.get("block") {
        if block.as_str() != Some("*") {
            let Some(items) = block.as_array() else {
                return Err(format!(
                    "{label}.block must be \"*\" or an array of tool names."
                ));
            };
            if items.is_empty() {
                return Err(format!(
                    "{label}.block must contain at least one tool name."
                ));
            }
            for item in items {
                match item.as_str().map(str::trim) {
                    Some(name) if !name.is_empty() => {}
                    _ => {
                        return Err(format!("{label}.block must contain non-empty tool names."));
                    }
                }
            }
        }
    }
    Ok(Some(ResolvedToolBudget {
        hard,
        soft: object.get("soft").and_then(Value::as_u64),
        block: normalize_tool_budget_block(object.get("block")),
    }))
}

/// `shouldBlockToolForBudget` (tool-budget.ts:55-58).
pub fn should_block_tool_for_budget(
    budget: &ResolvedToolBudget,
    tool_name: &str,
    next_count: u64,
) -> bool {
    if next_count <= budget.hard {
        return false;
    }
    budget.block.contains(tool_name)
}

/// `toolBudgetSoftNudge` (tool-budget.ts:60-62).
pub fn tool_budget_soft_nudge(budget: &ResolvedToolBudget, tool_count: u64) -> String {
    let Some(soft) = budget.soft else {
        return String::new();
    };
    let plural = if tool_count == 1 { "" } else { "s" };
    format!(
        "Tool budget soft limit reached after {tool_count} tool call{plural} (soft {soft}, hard {}). Stop starting new browsing/search work and finalize from the context you already have.",
        budget.hard
    )
}

/// `toolBudgetBlockedMessage` (tool-budget.ts:64-66).
pub fn tool_budget_blocked_message(
    budget: &ResolvedToolBudget,
    tool_name: &str,
    tool_count: u64,
) -> String {
    let plural = if tool_count == 1 { "" } else { "s" };
    format!(
        "Tool budget hard limit reached after {tool_count} tool call{plural} (hard {}). The '{tool_name}' tool is blocked so you can finalize from the context you already have.",
        budget.hard
    )
}

/// `TOOL_BUDGET_BLOCKED_MESSAGE` (tool-budget.ts:68) — the anchored shape.
fn blocked_message_parts(text: &str) -> Option<(u64, u64, String)> {
    let text = text.trim();
    // ^Tool budget hard limit reached after (\d+) tool calls? \(hard
    // (\d+)\)\. The '([^']+)' tool is blocked so you can finalize from the
    // context you already have\.$
    let rest = text.strip_prefix("Tool budget hard limit reached after ")?;
    let (count_raw, rest) = rest.split_once(" tool call")?;
    let rest = rest.strip_prefix('s').unwrap_or(rest);
    let rest = rest.strip_prefix(" (hard ")?;
    let (hard_raw, rest) = rest.split_once(")")?;
    let rest = rest.strip_prefix(". The '")?;
    let (tool, rest) = rest
        .split_once("' tool is blocked so you can finalize from the context you already have.")?;
    if !rest.is_empty() {
        return None;
    }
    Some((
        count_raw.parse().ok()?,
        hard_raw.parse().ok()?,
        tool.to_string(),
    ))
}

/// `isToolBudgetBlockedMessage` (tool-budget.ts:76-84): the whole message
/// must match the runtime format AND the embedded hard limit and tool name
/// must belong to this run.
#[cfg_attr(not(test), allow(dead_code))]
pub fn is_tool_budget_blocked_message(
    budget: &ResolvedToolBudget,
    result_text: &str,
    blocked_tool: Option<&str>,
) -> bool {
    let Some(tool) = blocked_tool.map(str::trim).filter(|t| !t.is_empty()) else {
        return false;
    };
    let Some((blocked_count, hard, named_tool)) = blocked_message_parts(result_text) else {
        return false;
    };
    hard == budget.hard && blocked_count > budget.hard && named_tool == tool
}

/// Terminal classification helper (#2302): find a hard-block reason in the
/// child's result text that belongs to THIS budget (full-message shape +
/// embedded hard attribution); returns the blocked tool name.
pub fn classify_blocked_message(budget: &ResolvedToolBudget, result_text: &str) -> Option<String> {
    let (blocked_count, hard, named_tool) = blocked_message_parts(result_text)?;
    if hard == budget.hard && blocked_count > budget.hard {
        Some(named_tool)
    } else {
        None
    }
}

/// `initialToolBudgetState` (tool-budget.ts:39-41).
#[cfg_attr(not(test), allow(dead_code))]
pub fn initial_tool_budget_state(budget: &ResolvedToolBudget) -> Value {
    tool_budget_state(budget, 0, None)
}

/// `toolBudgetState` (tool-budget.ts:43-53).
pub fn tool_budget_state(
    budget: &ResolvedToolBudget,
    tool_count: u64,
    blocked_tool: Option<&str>,
) -> Value {
    let over_hard = tool_count > budget.hard;
    let over_soft = budget.soft.is_some_and(|soft| tool_count >= soft);
    let mut state = serde_json::Map::new();
    state.insert("hard".to_string(), json!(budget.hard));
    if let Some(soft) = budget.soft {
        state.insert("soft".to_string(), json!(soft));
    }
    state.insert("block".to_string(), budget.block.to_json());
    state.insert(
        "outcome".to_string(),
        json!(if over_hard {
            "hard-blocked"
        } else if over_soft {
            "soft-reached"
        } else {
            "within-budget"
        }),
    );
    state.insert("toolCount".to_string(), json!(tool_count));
    if over_soft {
        state.insert("softReachedAt".to_string(), json!(budget.soft));
    }
    if over_hard {
        state.insert("hardReachedAt".to_string(), json!(budget.hard));
        if let Some(tool) = blocked_tool {
            state.insert("blockedTool".to_string(), json!(tool));
        }
    }
    Value::Object(state)
}

/// Encode the resolved budget for the child env.
pub fn budget_to_env_value(budget: &ResolvedToolBudget) -> String {
    let mut object = serde_json::Map::new();
    object.insert("hard".to_string(), json!(budget.hard));
    if let Some(soft) = budget.soft {
        object.insert("soft".to_string(), json!(soft));
    }
    object.insert("block".to_string(), budget.block.to_json());
    Value::Object(object).to_string()
}

/// Decode the env form (malformed → `None`: the child runs unenforced, the
/// parent's admission already validated the raw config).
pub fn budget_from_env() -> Option<ResolvedToolBudget> {
    let raw = std::env::var(TOOL_BUDGET_ENV).ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    let value: Value = serde_json::from_str(&raw).ok()?;
    validate_tool_budget_config(Some(&value), "toolBudget")
        .ok()
        .flatten()
}

/// Child-side runtime state (per process; a child session is one rpi
/// process — ADR-0019's in-process registry model keeps this trivially
/// session-scoped).
static TOOL_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static SOFT_NUDGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Reset the runtime counters (tests).
#[cfg_attr(not(test), allow(dead_code))]
pub fn reset_runtime_for_test() {
    TOOL_COUNT.store(0, std::sync::atomic::Ordering::SeqCst);
    SOFT_NUDGED.store(false, std::sync::atomic::Ordering::SeqCst);
}

/// The child's `tool_call` handler (`registerToolBudget`,
/// subagent-prompt-runtime.ts:384-405): count, soft-nudge (steer), hard
/// block. Returns the event result (`{block, reason}`) or `Null`.
pub fn handle_tool_call_event(event: &Value, send_user_message: &dyn Fn(&str)) -> Value {
    let Some(budget) = budget_from_env() else {
        return Value::Null;
    };
    let tool_name = event
        .get("toolName")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("tool");
    let count = TOOL_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    if let Some(_soft) = budget.soft {
        if count >= _soft && !SOFT_NUDGED.swap(true, std::sync::atomic::Ordering::SeqCst) {
            // Budget nudges are advisory; blocking below stays authoritative.
            send_user_message(&tool_budget_soft_nudge(&budget, count));
        }
    }
    if !should_block_tool_for_budget(&budget, tool_name, count) {
        return Value::Null;
    }
    json!({
        "block": true,
        "reason": tool_budget_blocked_message(&budget, tool_name, count),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the env-manipulating tests (TE34 TEST_LOCK convention).
    static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn budget() -> ResolvedToolBudget {
        ResolvedToolBudget {
            hard: 2,
            soft: Some(1),
            block: BlockList::Names(vec!["bash".to_string()]),
        }
    }

    #[test]
    fn validation_matches_upstream_texts() {
        assert!(validate_tool_budget_config(None, "toolBudget")
            .unwrap()
            .is_none());
        let valid = validate_tool_budget_config(
            Some(&json!({"hard": 2, "soft": 1, "block": "*"})),
            "toolBudget",
        )
        .unwrap()
        .unwrap();
        assert_eq!(valid.hard, 2);
        assert_eq!(valid.soft, Some(1));
        assert_eq!(valid.block, BlockList::All);
        for (raw, message) in [
            (
                json!("x"),
                "toolBudget must be an object with hard and optional soft/block.",
            ),
            (json!({}), "toolBudget.hard must be an integer >= 1."),
            (
                json!({"hard": 0}),
                "toolBudget.hard must be an integer >= 1.",
            ),
            (
                json!({"hard": 2, "soft": 0}),
                "toolBudget.soft must be an integer >= 1 when provided.",
            ),
            (
                json!({"hard": 2, "soft": 3}),
                "toolBudget.soft must be <= toolBudget.hard.",
            ),
            (
                json!({"hard": 2, "block": "x"}),
                "toolBudget.block must be \"*\" or an array of tool names.",
            ),
            (
                json!({"hard": 2, "block": []}),
                "toolBudget.block must contain at least one tool name.",
            ),
            (
                json!({"hard": 2, "block": ["  "]}),
                "toolBudget.block must contain non-empty tool names.",
            ),
        ] {
            let error = validate_tool_budget_config(Some(&raw), "toolBudget").unwrap_err();
            assert_eq!(error, message, "{raw}");
        }
    }

    #[test]
    fn default_block_list_and_blocking_rule() {
        let default = normalize_tool_budget_block(None);
        assert_eq!(
            default,
            BlockList::Names(
                DEFAULT_TOOL_BUDGET_BLOCK
                    .iter()
                    .map(|n| n.to_string())
                    .collect()
            )
        );
        let b = budget();
        assert!(!should_block_tool_for_budget(&b, "bash", 2));
        assert!(should_block_tool_for_budget(&b, "bash", 3));
        // Not in the block list: never blocked.
        assert!(!should_block_tool_for_budget(&b, "read", 9));
    }

    #[test]
    fn blocked_message_exact_format_and_recognition() {
        let b = budget();
        let message = tool_budget_blocked_message(&b, "bash", 3);
        assert_eq!(
            message,
            "Tool budget hard limit reached after 3 tool calls (hard 2). The 'bash' tool is blocked so you can finalize from the context you already have."
        );
        assert!(is_tool_budget_blocked_message(&b, &message, Some("bash")));
        // Wrong tool attribution, incidental phrase, and mismatched hard
        // value are all rejected (a586ef7e).
        assert!(!is_tool_budget_blocked_message(&b, &message, Some("read")));
        assert!(!is_tool_budget_blocked_message(
            &b,
            "ordinary output mentioning Tool budget hard limit reached after 3 tool calls (hard 2). The 'bash' tool is blocked so you can finalize from the context you already have. and more",
            Some("bash")
        ));
        let other = ResolvedToolBudget {
            hard: 5,
            soft: None,
            block: BlockList::All,
        };
        assert!(!is_tool_budget_blocked_message(
            &other,
            &message,
            Some("bash")
        ));
        assert!(!is_tool_budget_blocked_message(&b, &message, None));
    }

    #[test]
    fn state_shape_matches_upstream() {
        let b = budget();
        assert_eq!(
            initial_tool_budget_state(&b)["outcome"],
            json!("within-budget")
        );
        let soft = tool_budget_state(&b, 1, None);
        assert_eq!(soft["outcome"], json!("soft-reached"));
        assert_eq!(soft["softReachedAt"], json!(1));
        assert!(soft.get("hardReachedAt").is_none());
        // #2302: a block before execution reports past-hard counting
        // (max(toolCount, hard+1)) with the blocked tool.
        let hard = tool_budget_state(&b, b.hard + 1, Some("bash"));
        assert_eq!(hard["outcome"], json!("hard-blocked"));
        assert_eq!(hard["hardReachedAt"], json!(2));
        assert_eq!(hard["blockedTool"], json!("bash"));
    }

    #[test]
    fn env_round_trip() {
        let _guard = ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let b = budget();
        let encoded = budget_to_env_value(&b);
        std::env::set_var(TOOL_BUDGET_ENV, &encoded);
        assert_eq!(budget_from_env(), Some(b));
        std::env::set_var(TOOL_BUDGET_ENV, "{broken");
        assert_eq!(budget_from_env(), None);
        std::env::remove_var(TOOL_BUDGET_ENV);
    }

    #[test]
    fn child_event_counts_nudges_and_blocks() {
        let _guard = ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_runtime_for_test();
        std::env::set_var(
            TOOL_BUDGET_ENV,
            budget_to_env_value(&ResolvedToolBudget {
                hard: 1,
                soft: Some(1),
                block: BlockList::All,
            }),
        );
        let nudges: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        let send = |text: &str| {
            if text.starts_with("Tool budget soft limit reached") {
                nudges.lock().unwrap().push(text.to_string());
            }
        };
        // Unset env → inert handler.
        std::env::remove_var(TOOL_BUDGET_ENV);
        assert_eq!(
            handle_tool_call_event(&json!({"toolName": "bash"}), &send),
            Value::Null
        );
        std::env::set_var(
            TOOL_BUDGET_ENV,
            budget_to_env_value(&ResolvedToolBudget {
                hard: 1,
                soft: Some(1),
                block: BlockList::All,
            }),
        );
        // Call 1: soft nudge fires (soft=1), no block (1 <= hard).
        assert_eq!(
            handle_tool_call_event(&json!({"toolName": "bash"}), &send),
            Value::Null
        );
        // Call 2: past hard → blocked with the exact message.
        let blocked = handle_tool_call_event(&json!({"toolName": "bash"}), &send);
        assert_eq!(blocked["block"], json!(true));
        assert_eq!(
            blocked["reason"],
            json!("Tool budget hard limit reached after 2 tool calls (hard 1). The 'bash' tool is blocked so you can finalize from the context you already have.")
        );
        // The nudge fired exactly once (soft-nudged latch).
        assert_eq!(nudges.lock().unwrap().len(), 1);
        std::env::remove_var(TOOL_BUDGET_ENV);
        reset_runtime_for_test();
    }
}
