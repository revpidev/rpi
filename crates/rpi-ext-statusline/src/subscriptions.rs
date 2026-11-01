//! Field-driven event subscriptions (TE52 R20 §4.4 / 04 §3.4).
//!
//! The base lifecycle set is always subscribed. Command mode adds its TE12
//! events (`message_end`, `tool_execution_end`) plus the load-time-fixed live
//! events; items mode adds only the events its selected fields need:
//!
//! | capability | events |
//! |---|---|
//! | session totals | `turn_end`, `agent_settled` |
//! | run-state | `agent_start`, `agent_settled` |
//! | permission-mode | `mode_change` |
//! | live fields + liveTokens | `message_start`, `message_update`, `message_end` |
//! | Git fields | `tool_execution_end` |
//!
//! `on` returns a per-registration `subscriptionId`; `off` removes exactly
//! that registration. Reconciliation is idempotent and runs after config
//! changes, so a saved editor draft takes effect without a restart.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::json;

use crate::config::StatusLine;
use crate::{AsyncHostCalls, host_ok};

/// Always-subscribed lifecycle events (both modes).
pub const BASE_EVENTS: [&str; 5] = [
    "session_start",
    "session_shutdown",
    "session_info_changed",
    "model_select",
    "thinking_level_select",
];

/// The TE12 command-mode event table (minus the live pair, which is fixed at
/// load time). `session_compact` is command-mode-specific here because the
/// TE52 contract forbids registering it for an items selection that does not
/// read session totals.
pub const COMMAND_EVENTS: [&str; 3] = ["message_end", "tool_execution_end", "session_compact"];

/// Live event names (command: load-time fixed; items: field-driven).
pub const LIVE_EVENTS: [&str; 3] = ["message_start", "message_update", "message_end"];

/// Compute the desired event set for a configuration.
///
/// `command_live_fixed` is the value captured at plugin install for command
/// mode (runtime `liveTokens` edits require a restart there).
pub fn desired_events(
    config: Option<&StatusLine>,
    command_live_fixed: bool,
) -> BTreeSet<&'static str> {
    let mut desired: BTreeSet<&'static str> = BASE_EVENTS.iter().copied().collect();
    match config {
        None => {}
        Some(StatusLine::Command(_)) => {
            desired.extend(COMMAND_EVENTS);
            if command_live_fixed {
                desired.extend(LIVE_EVENTS);
            }
        }
        Some(StatusLine::Items(items)) => {
            if items.has_session_total() {
                desired.insert("turn_end");
                desired.insert("agent_settled");
                desired.insert("session_compact");
            }
            if items.has_field(crate::fields::FieldId::RunState) {
                desired.insert("agent_start");
                desired.insert("agent_settled");
            }
            if items.has_field(crate::fields::FieldId::PermissionMode) {
                desired.insert("mode_change");
            }
            if items.live_enabled() {
                desired.extend(LIVE_EVENTS);
            }
            if items.needs_git() {
                desired.insert("tool_execution_end");
            }
        }
    }
    desired
}

/// Live registration bookkeeping.
#[derive(Debug, Default)]
pub struct SubscriptionState {
    ids: BTreeMap<&'static str, u64>,
}

impl SubscriptionState {
    /// Reconcile registrations with `desired`. Returns whether anything
    /// changed. Failures are logged and retried on the next reconciliation.
    pub fn reconcile(
        &mut self,
        channel: &AsyncHostCalls,
        desired: &BTreeSet<&'static str>,
    ) -> bool {
        let mut changed = false;
        // Unsubscribe first: the new selection must not receive events for a
        // capability it no longer needs.
        let stale: Vec<&'static str> = self
            .ids
            .keys()
            .copied()
            .filter(|event| !desired.contains(event))
            .collect();
        for event in stale {
            if let Some(id) = self.ids.remove(event) {
                let response =
                    crate::host_call_static(channel, "off", json!({ "subscriptionId": id }));
                if response.get("error").is_some() {
                    tracing::warn!(event, ?response, "rpi-statusline off failed");
                }
                changed = true;
            }
        }
        for event in desired {
            if self.ids.contains_key(event) {
                continue;
            }
            let Some(value) = host_ok(channel, "on", json!({ "event": event })) else {
                continue;
            };
            let Some(id) = value
                .get("subscriptionId")
                .and_then(serde_json::Value::as_u64)
            else {
                tracing::warn!(
                    event,
                    ?value,
                    "rpi-statusline on reply missing subscriptionId"
                );
                continue;
            };
            self.ids.insert(event, id);
            changed = true;
        }
        changed
    }

    /// Registered events (tests/diagnostics).
    pub fn events(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.ids.keys().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StatusLineConfig;
    use crate::fields::FieldId;
    use crate::items::ItemsConfig;

    fn items(ids: &[FieldId], live: bool) -> StatusLine {
        let mut config = ItemsConfig::default();
        config.set_items(&ids.iter().map(|id| id.id().to_owned()).collect::<Vec<_>>());
        config.live_tokens.enabled = live;
        StatusLine::Items(config)
    }

    fn command() -> StatusLine {
        StatusLine::Command(StatusLineConfig {
            command: "echo x".into(),
            padding: 0,
            refresh_interval_secs: None,
            placement: crate::config::Placement::Replace,
            timeout_ms: crate::config::DEFAULT_TIMEOUT_MS,
            live_tokens: crate::config::LiveTokens::default(),
        })
    }

    #[test]
    fn no_config_keeps_only_the_base_lifecycle_set() {
        let desired = desired_events(None, false);
        assert_eq!(desired, BASE_EVENTS.iter().copied().collect());
    }

    #[test]
    fn command_mode_uses_the_te12_table_plus_load_time_live() {
        let desired = desired_events(Some(&command()), false);
        for event in COMMAND_EVENTS {
            assert!(desired.contains(event), "{event}");
        }
        // `message_end` belongs to the command table; only the streaming
        // pair needs the live switch.
        for event in ["message_start", "message_update"] {
            assert!(
                !desired.contains(event),
                "{event} must need the live switch"
            );
        }
        assert!(desired.contains("message_end"));
        let live = desired_events(Some(&command()), true);
        for event in LIVE_EVENTS {
            assert!(live.contains(event), "{event}");
        }
    }

    #[test]
    fn items_defaults_subscribe_no_extra_collection() {
        // Default items: session-duration + pending live-output-tokens with
        // liveTokens=false → duration is a local tick, no extra event.
        let desired = desired_events(Some(&items(&[FieldId::SessionDuration], false)), false);
        assert_eq!(desired, BASE_EVENTS.iter().copied().collect());
        assert!(!desired.contains("message_end"));
        assert!(!desired.contains("tool_execution_end"));
        assert!(!desired.contains("turn_end"));
        assert!(!desired.contains("session_compact"));
        assert!(!desired.contains("mode_change"));
        assert!(!desired.contains("agent_start"));
    }

    #[test]
    fn items_capabilities_map_to_their_events() {
        let totals = desired_events(Some(&items(&[FieldId::UsedTokens], false)), false);
        // `session_compact` is a totals trigger only in items mode.
        assert!(totals.contains("turn_end"));
        assert!(totals.contains("agent_settled"));
        assert!(totals.contains("session_compact"));
        assert!(!totals.contains("message_end"));

        let run_state = desired_events(Some(&items(&[FieldId::RunState], false)), false);
        assert!(run_state.contains("agent_start"));
        assert!(run_state.contains("agent_settled"));

        let mode = desired_events(Some(&items(&[FieldId::PermissionMode], false)), false);
        assert!(mode.contains("mode_change"));

        let git = desired_events(Some(&items(&[FieldId::GitBranch], false)), false);
        assert!(git.contains("tool_execution_end"));

        let live = desired_events(Some(&items(&[FieldId::LiveOutputTokens], true)), false);
        for event in LIVE_EVENTS {
            assert!(live.contains(event), "{event}");
        }
    }

    #[test]
    fn live_switch_alone_does_not_subscribe_events() {
        let desired = desired_events(Some(&items(&[FieldId::Model], true)), false);
        for event in LIVE_EVENTS {
            assert!(!desired.contains(event), "{event}");
        }
    }
}
