//! Subagent control/attention model (pi-subagents `src/runs/shared/subagent-control.ts`
//! @ v0.83.0 `3a63e0ad`).
//!
//! The rpi port covers the local Pi child path: idle/needs-attention,
//! active-long-running thresholds, open-tool attention, mutating-failure
//! escalation, event dedupe and the parent notice text. Intercom and the
//! native command controller are absent from rpi, so their notice lines are
//! intentionally omitted (registered as [N/A-struct] in the TE51 notes).

use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;

/// Activity state (`ActivityState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityState {
    ActiveLongRunning,
    NeedsAttention,
}

impl ActivityState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ActiveLongRunning => "active_long_running",
            Self::NeedsAttention => "needs_attention",
        }
    }
}

/// Control event type (`ControlEventType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlEventType {
    ActiveLongRunning,
    NeedsAttention,
}

impl ControlEventType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ActiveLongRunning => "active_long_running",
            Self::NeedsAttention => "needs_attention",
        }
    }
}

/// Notification channel (`ControlNotificationChannel`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlNotificationChannel {
    Event,
    Async,
    Intercom,
}

/// Resolved control config (`ResolvedControlConfig`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedControlConfig {
    pub enabled: bool,
    pub needs_attention_after_ms: u64,
    pub needs_attention_after_ms_is_explicit: bool,
    pub active_notice_after_ms: u64,
    pub active_notice_after_turns: Option<u64>,
    pub active_notice_after_tokens: Option<u64>,
    pub failed_tool_attempts_before_attention: u64,
    pub notify_on: Vec<ControlEventType>,
    pub notify_channels: Vec<ControlNotificationChannel>,
}

impl Default for ResolvedControlConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            needs_attention_after_ms: 60_000,
            needs_attention_after_ms_is_explicit: false,
            active_notice_after_ms: 240_000,
            active_notice_after_turns: None,
            active_notice_after_tokens: None,
            failed_tool_attempts_before_attention: 3,
            notify_on: vec![
                ControlEventType::ActiveLongRunning,
                ControlEventType::NeedsAttention,
            ],
            notify_channels: vec![
                ControlNotificationChannel::Event,
                ControlNotificationChannel::Async,
                ControlNotificationChannel::Intercom,
            ],
        }
    }
}

fn positive_int(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64).filter(|value| *value >= 1)
}

fn parse_list(value: Option<&Value>, allowed: &[&str]) -> Option<Vec<String>> {
    let items = value?.as_array()?;
    if items.is_empty() {
        return Some(Vec::new());
    }
    let mut seen = BTreeSet::new();
    let mut parsed = Vec::new();
    for item in items {
        let Some(item) = item.as_str() else {
            continue;
        };
        if allowed.contains(&item) && seen.insert(item.to_string()) {
            parsed.push(item.to_string());
        }
    }
    (!parsed.is_empty()).then_some(parsed)
}

/// `resolveControlConfig` (`subagent-control.ts:41-81`). Both inputs are raw
/// JSON (global `config.control` and per-call `control`); invalid positive
/// integers and non-array lists fall through to the next/default layer.
pub fn resolve(global: Option<&Value>, override_value: Option<&Value>) -> ResolvedControlConfig {
    let default = ResolvedControlConfig::default();
    let global = global.and_then(Value::as_object);
    let override_value = override_value.and_then(Value::as_object);
    let enabled = override_value
        .and_then(|value| value.get("enabled"))
        .and_then(Value::as_bool)
        .or_else(|| {
            global
                .and_then(|value| value.get("enabled"))
                .and_then(Value::as_bool)
        })
        .unwrap_or(default.enabled);
    let override_needs =
        positive_int(override_value.and_then(|value| value.get("needsAttentionAfterMs")));
    let global_needs = positive_int(global.and_then(|value| value.get("needsAttentionAfterMs")));
    let needs_attention_after_ms = override_needs
        .or(global_needs)
        .unwrap_or(default.needs_attention_after_ms);
    let needs_attention_after_ms_is_explicit = override_needs.is_some() || global_needs.is_some();
    let active_notice_after_ms =
        positive_int(override_value.and_then(|value| value.get("activeNoticeAfterMs")))
            .or_else(|| positive_int(global.and_then(|value| value.get("activeNoticeAfterMs"))))
            .unwrap_or(default.active_notice_after_ms);
    let active_notice_after_turns =
        positive_int(override_value.and_then(|value| value.get("activeNoticeAfterTurns")))
            .or_else(|| positive_int(global.and_then(|value| value.get("activeNoticeAfterTurns"))));
    let active_notice_after_tokens =
        positive_int(override_value.and_then(|value| value.get("activeNoticeAfterTokens")))
            .or_else(|| {
                positive_int(global.and_then(|value| value.get("activeNoticeAfterTokens")))
            });
    let failed_tool_attempts_before_attention = positive_int(
        override_value.and_then(|value| value.get("failedToolAttemptsBeforeAttention")),
    )
    .or_else(|| {
        positive_int(global.and_then(|value| value.get("failedToolAttemptsBeforeAttention")))
    })
    .unwrap_or(default.failed_tool_attempts_before_attention);
    let notify_on = parse_list(
        override_value.and_then(|value| value.get("notifyOn")),
        &["active_long_running", "needs_attention"],
    )
    .or_else(|| {
        parse_list(
            global.and_then(|value| value.get("notifyOn")),
            &["active_long_running", "needs_attention"],
        )
    })
    .unwrap_or_else(|| {
        default
            .notify_on
            .iter()
            .map(|value| value.as_str().to_string())
            .collect()
    });
    let notify_channels = parse_list(
        override_value.and_then(|value| value.get("notifyChannels")),
        &["event", "async", "intercom"],
    )
    .or_else(|| {
        parse_list(
            global.and_then(|value| value.get("notifyChannels")),
            &["event", "async", "intercom"],
        )
    })
    .unwrap_or_else(|| {
        default
            .notify_channels
            .iter()
            .map(|value| match value {
                ControlNotificationChannel::Event => "event".to_string(),
                ControlNotificationChannel::Async => "async".to_string(),
                ControlNotificationChannel::Intercom => "intercom".to_string(),
            })
            .collect()
    });
    ResolvedControlConfig {
        enabled,
        needs_attention_after_ms,
        needs_attention_after_ms_is_explicit,
        active_notice_after_ms,
        active_notice_after_turns,
        active_notice_after_tokens,
        failed_tool_attempts_before_attention,
        notify_on: notify_on
            .into_iter()
            .filter_map(|value| match value.as_str() {
                "active_long_running" => Some(ControlEventType::ActiveLongRunning),
                "needs_attention" => Some(ControlEventType::NeedsAttention),
                _ => None,
            })
            .collect(),
        notify_channels: notify_channels
            .into_iter()
            .filter_map(|value| match value.as_str() {
                "event" => Some(ControlNotificationChannel::Event),
                "async" => Some(ControlNotificationChannel::Async),
                "intercom" => Some(ControlNotificationChannel::Intercom),
                _ => None,
            })
            .collect(),
    }
}

fn scaled_needs_attention_after_ms(config: &ResolvedControlConfig, thinking: Option<&str>) -> u64 {
    if config.needs_attention_after_ms_is_explicit {
        return config.needs_attention_after_ms;
    }
    match thinking {
        Some("medium") => config.needs_attention_after_ms.saturating_mul(2),
        Some("high") => config.needs_attention_after_ms.saturating_mul(5),
        Some("xhigh" | "max") => config.needs_attention_after_ms.saturating_mul(10),
        _ => config.needs_attention_after_ms,
    }
}

/// `deriveActivityState` (`subagent-control.ts:98-112`).
pub fn derive_activity_state(
    config: &ResolvedControlConfig,
    started_at: u64,
    last_activity_at: Option<u64>,
    turn_count: u64,
    current_tool: Option<&str>,
    thinking: Option<&str>,
    now: u64,
) -> Option<ActivityState> {
    if !config.enabled || current_tool.is_some() || turn_count == 0 {
        return None;
    }
    let last = last_activity_at.unwrap_or(started_at);
    let age = now.saturating_sub(last);
    (age > scaled_needs_attention_after_ms(config, thinking))
        .then_some(ActivityState::NeedsAttention)
}

/// `isToolTimeoutExempt` (`tool-timeout.ts`): tools that may legitimately run
/// long without producing a notice.
pub fn is_tool_timeout_exempt(tool: &str) -> bool {
    matches!(
        tool,
        "contact_supervisor" | "intercom" | "subagent_wait" | "bg_wait"
    )
}

/// `shouldEmitOpenToolAttention` (`subagent-control.ts:114-124`).
pub fn should_emit_open_tool_attention(
    config: &ResolvedControlConfig,
    current_tool: Option<&str>,
    current_tool_started_at: Option<u64>,
    now: u64,
) -> bool {
    let (Some(tool), Some(started_at)) = (current_tool, current_tool_started_at) else {
        return false;
    };
    if !config.enabled || is_tool_timeout_exempt(tool) {
        return false;
    }
    now.saturating_sub(started_at) >= config.active_notice_after_ms
}

/// `ControlEvent` (`shared/types.ts:358-390`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlEvent {
    #[serde(rename = "type")]
    pub event_type: ControlEventType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<ActivityState>,
    pub to: ActivityState,
    pub ts: u64,
    pub agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<u32>,
    pub run_id: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_tool_duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recent_failure_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_preview: Option<String>,
}

/// Builder inputs for [`build_control_event`].
#[derive(Debug, Default, Clone)]
pub struct ControlEventInput {
    pub event_type: Option<ControlEventType>,
    pub from: Option<ActivityState>,
    pub to: Option<ActivityState>,
    pub run_id: String,
    pub agent: String,
    pub index: Option<u32>,
    pub ts: u64,
    pub last_activity_at: Option<u64>,
    pub message: Option<String>,
    pub reason: Option<String>,
    pub turns: Option<u64>,
    pub tokens: Option<u64>,
    pub tool_count: Option<u64>,
    pub current_tool: Option<String>,
    pub tool_call_id: Option<String>,
    pub current_tool_duration_ms: Option<u64>,
    pub current_path: Option<String>,
    pub recent_failure_summary: Option<String>,
    pub task_preview: Option<String>,
}

/// `buildControlEvent` (`subagent-control.ts:126-186`).
pub fn build_control_event(input: ControlEventInput) -> ControlEvent {
    let to = input.to.unwrap_or(ActivityState::NeedsAttention);
    let event_type = input.event_type.unwrap_or(match to {
        ActivityState::ActiveLongRunning => ControlEventType::ActiveLongRunning,
        ActivityState::NeedsAttention => ControlEventType::NeedsAttention,
    });
    let elapsed_ms = input
        .last_activity_at
        .map(|last| input.ts.saturating_sub(last));
    let message = input.message.unwrap_or_else(|| match event_type {
        ControlEventType::ActiveLongRunning => {
            format!("{} is still active but long-running", input.agent)
        }
        ControlEventType::NeedsAttention => elapsed_ms
            .map(|elapsed| {
                format!(
                    "{} needs attention (no observed activity for {}s)",
                    input.agent,
                    elapsed / 1000
                )
            })
            .unwrap_or_else(|| format!("{} needs attention", input.agent)),
    });
    ControlEvent {
        event_type,
        from: input.from,
        to,
        ts: input.ts,
        agent: input.agent,
        index: input.index,
        run_id: input.run_id,
        message,
        reason: input.reason.or_else(|| {
            Some(match event_type {
                ControlEventType::ActiveLongRunning => "active_long_running".to_string(),
                ControlEventType::NeedsAttention => "idle".to_string(),
            })
        }),
        turns: input.turns,
        tokens: input.tokens,
        tool_count: input.tool_count,
        current_tool: input.current_tool,
        tool_call_id: input.tool_call_id,
        current_tool_duration_ms: input.current_tool_duration_ms,
        current_path: input.current_path,
        elapsed_ms,
        recent_failure_summary: input.recent_failure_summary,
        task_preview: input.task_preview.map(|task| preview_text(&task, 160)),
    }
}

/// `previewDisplayText` subset: collapse whitespace and bound to `max`.
fn preview_text(text: &str, max: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > max {
        let mut out: String = collapsed.chars().take(max).collect();
        out.push('…');
        out
    } else {
        collapsed
    }
}

/// `shouldNotifyControlEvent` (`subagent-control.ts:188-190`).
pub fn should_notify(config: &ResolvedControlConfig, event: &ControlEvent) -> bool {
    config.enabled && config.notify_on.contains(&event.event_type)
}

/// `controlNotificationKey` (`subagent-control.ts:192-197`): the context hash
/// is the sha256 of the nudge text, truncated to 8 hex chars.
pub fn notification_key(event: &ControlEvent) -> String {
    use sha2::{Digest, Sha256};
    let nudge = format_control_nudge(event);
    let context_hash = format!("{:x}", Sha256::digest(nudge.as_bytes()));
    let call_key = if event.reason.as_deref() == Some("tool_open_threshold") {
        event
            .tool_call_id
            .as_deref()
            .map(|id| format!(":{id}"))
            .unwrap_or_default()
    } else {
        String::new()
    };
    let child_key = event
        .index
        .map(|index| format!("{}:{index}", event.run_id))
        .unwrap_or_else(|| event.run_id.clone());
    format!(
        "{child_key}:{}:{}{call_key}:{}",
        event.event_type.as_str(),
        event.reason.as_deref().unwrap_or("idle"),
        &context_hash[..8]
    )
}

/// `claimControlNotification` (`subagent-control.ts:199-205`).
pub fn claim_notification(
    config: &ResolvedControlConfig,
    event: &ControlEvent,
    seen: &mut BTreeSet<String>,
) -> bool {
    if !should_notify(config, event) {
        return false;
    }
    let key = notification_key(event);
    seen.insert(key)
}

/// `formatControlNudge` (`subagent-control.ts:218-227`).
pub fn format_control_nudge(event: &ControlEvent) -> String {
    if let Some(summary) = &event.recent_failure_summary {
        return format!(
            "Resolve the recent failure: {summary}. Report the smallest next step or ask for a decision."
        );
    }
    if event.current_tool.is_some() || event.current_path.is_some() {
        let current = [
            event
                .current_tool
                .as_ref()
                .map(|tool| format!("tool {tool}")),
            event
                .current_path
                .as_ref()
                .map(|path| format!("path {path}")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" at ");
        return format!("Check {current}. Report the smallest next step or ask for a decision.");
    }
    "What are you blocked on? Reply with the smallest next step or ask for a decision.".to_string()
}

fn format_long_running_facts(event: &ControlEvent) -> Option<String> {
    let mut facts = Vec::new();
    if let Some(elapsed) = event.elapsed_ms {
        facts.push(format!("elapsed {}s", elapsed / 1000));
    }
    if let Some(turns) = event.turns {
        facts.push(format!("{turns} turns"));
    }
    if let Some(tokens) = event.tokens {
        facts.push(format!("{tokens} tokens"));
    }
    if let Some(tools) = event.tool_count {
        facts.push(format!("{tools} tools"));
    }
    if let Some(tool) = &event.current_tool {
        let duration = event
            .current_tool_duration_ms
            .map(|ms| format!(" {}s", ms / 1000))
            .unwrap_or_default();
        facts.push(format!("tool {tool}{duration}"));
    }
    if let Some(path) = &event.current_path {
        facts.push(format!("path {path}"));
    }
    (!facts.is_empty()).then(|| facts.join(" | "))
}

/// `formatControlNoticeMessage` (`subagent-control.ts:229-281`), adapted to
/// the rpi action surface (`interrupt`/`status`/`steer`).
pub fn format_control_notice(event: &ControlEvent) -> String {
    let nudge = format_control_nudge(event);
    let steer = format!(
        "subagent({{ action: \"steer\", id: \"{}\"{}, message: {} }})",
        event.run_id,
        event
            .index
            .map(|index| format!(", index: {index}"))
            .unwrap_or_default(),
        serde_json::to_string(&nudge).unwrap_or_else(|_| "\"\"".to_string())
    );
    let status = format!(
        "Status: subagent({{ action: \"status\", id: \"{}\" }})",
        event.run_id
    );
    let interrupt = format!(
        "Interrupt: subagent({{ action: \"interrupt\", id: \"{}\" }})",
        event.run_id
    );
    let mut lines = vec![
        format!(
            "{}: {}",
            if event.event_type == ControlEventType::ActiveLongRunning {
                "Subagent active but long-running"
            } else {
                "Subagent needs attention"
            },
            event.agent
        ),
        format!(
            "Run: {}{}",
            event.run_id,
            event
                .index
                .map(|index| format!(" step {}", index + 1))
                .unwrap_or_default()
        ),
        format!("Signal: {}", event.message),
    ];
    if let Some(facts) = format_long_running_facts(event) {
        lines.push(format!("Facts: {facts}"));
    }
    if let Some(summary) = &event.recent_failure_summary {
        lines.push(format!("Recent failures: {summary}"));
    }
    lines.push(format!("Hint: {nudge}"));
    lines.push(format!("Top-level live async nudge: {steer}"));
    lines.push(status);
    lines.push(interrupt);
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_and_thinking_scaling() {
        let config = resolve(None, None);
        assert_eq!(config.needs_attention_after_ms, 60_000);
        assert_eq!(config.active_notice_after_ms, 240_000);
        assert_eq!(config.failed_tool_attempts_before_attention, 3);
        assert_eq!(
            derive_activity_state(&config, 0, Some(0), 1, None, None, 60_001),
            Some(ActivityState::NeedsAttention)
        );
        assert_eq!(
            derive_activity_state(&config, 0, Some(0), 1, None, Some("high"), 299_999),
            None,
            "high scales 60s to 300s"
        );
        assert_eq!(
            derive_activity_state(&config, 0, Some(0), 1, None, Some("high"), 300_001),
            Some(ActivityState::NeedsAttention)
        );
    }

    #[test]
    fn explicit_threshold_wins_over_scaling() {
        let config = resolve(Some(&json!({"needsAttentionAfterMs": 1000})), None);
        assert!(config.needs_attention_after_ms_is_explicit);
        assert_eq!(
            derive_activity_state(&config, 0, Some(0), 1, None, Some("max"), 1001),
            Some(ActivityState::NeedsAttention)
        );
    }

    #[test]
    fn open_tool_attention_exempts_waits() {
        let config = resolve(None, None);
        assert!(!should_emit_open_tool_attention(
            &config,
            Some("bash"),
            Some(0),
            239_999
        ));
        assert!(should_emit_open_tool_attention(
            &config,
            Some("bash"),
            Some(0),
            240_000
        ));
        assert!(!should_emit_open_tool_attention(
            &config,
            Some("subagent_wait"),
            Some(0),
            999_999
        ));
    }

    #[test]
    fn event_notice_and_dedupe() {
        let config = resolve(None, None);
        let event = build_control_event(ControlEventInput {
            to: Some(ActivityState::NeedsAttention),
            run_id: "run-1".to_string(),
            agent: "worker".to_string(),
            index: Some(0),
            ts: 61_000,
            last_activity_at: Some(1_000),
            ..ControlEventInput::default()
        });
        assert_eq!(event.reason.as_deref(), Some("idle"));
        let mut seen = BTreeSet::new();
        assert!(claim_notification(&config, &event, &mut seen));
        assert!(!claim_notification(&config, &event, &mut seen));
        let notice = format_control_notice(&event);
        assert!(
            notice.contains("Subagent needs attention: worker"),
            "{notice}"
        );
        assert!(notice.contains("subagent"), "{notice}");
        assert!(notice.contains("index: 0"), "{notice}");
        let no_index = build_control_event(ControlEventInput {
            to: Some(ActivityState::NeedsAttention),
            run_id: "run-2".to_string(),
            agent: "worker".to_string(),
            ts: 61_000,
            last_activity_at: Some(1_000),
            ..ControlEventInput::default()
        });
        assert!(!format_control_notice(&no_index).contains("index:"));
    }
}
