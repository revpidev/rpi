//! Field-driven event subscriptions (TE52 R20 §4.4 / 04 §3.4).
//!
//! The base lifecycle set is always subscribed. Command mode adds its TE12
//! events (`message_end`, `tool_execution_end`) plus the live events when
//! `liveTokens.enabled` is currently true; items mode adds only the events its
//! selected fields need:
//!
//! | capability | events |
//! |---|---|
//! | session totals/counts | `turn_end`, `agent_settled` |
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

/// The TE12 command-mode event table (minus the live pair, which follows the
/// current `liveTokens.enabled` switch). `session_compact` is command-mode-specific here because the
/// TE52 contract forbids registering it for an items selection that does not
/// read session totals.
pub const COMMAND_EVENTS: [&str; 3] = ["message_end", "tool_execution_end", "session_compact"];

/// Live event names (command: current `liveTokens.enabled`; items:
/// field-driven) — the legacy CUMULATIVE `message_update` payload.
pub const LIVE_EVENTS: [&str; 3] = ["message_start", "message_update", "message_end"];

/// Live subscriptions while probing for the delta-only event (rpi#78 A1/1.2).
/// Both update variants are required: an old host never emits the unknown delta
/// event, while a new host emits it only when a subscriber exists.
pub const LIVE_EVENTS_PROBE: [&str; 4] = [
    "message_start",
    "message_update",
    "message_update_delta",
    "message_end",
];

/// Live event names after the host has proven delta support. Dropping the
/// cumulative subscription lets the host skip materializing the accumulated
/// `message` snapshot on every delta.
pub const LIVE_EVENTS_DELTA: [&str; 3] = ["message_start", "message_update_delta", "message_end"];

/// Compute the desired event set for a configuration.
///
/// `command_live` is the CURRENT `statusLine.liveTokens.enabled` for command
/// mode. Reconciliation runs on every poll, so enabling the switch takes
/// effect within one poll without restarting the session; disabling it removes
/// the streaming subscriptions immediately (the zero-overhead-when-off
/// property is preserved).
pub fn desired_events(
    config: Option<&StatusLine>,
    command_live: bool,
    delta_supported: bool,
) -> BTreeSet<&'static str> {
    let live_events: &[&str] = if delta_supported {
        &LIVE_EVENTS_DELTA
    } else {
        &LIVE_EVENTS_PROBE
    };
    let mut desired: BTreeSet<&'static str> = BASE_EVENTS.iter().copied().collect();
    match config {
        None => {}
        Some(StatusLine::Command(_)) => {
            desired.extend(COMMAND_EVENTS);
            if command_live {
                desired.extend(live_events.iter().copied());
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
                desired.extend(live_events.iter().copied());
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
        let desired = desired_events(None, false, false);
        assert_eq!(desired, BASE_EVENTS.iter().copied().collect());
    }

    #[test]
    fn command_mode_uses_the_te12_table_plus_the_current_live_switch() {
        let desired = desired_events(Some(&command()), false, false);
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
        let live = desired_events(Some(&command()), true, false);
        for event in LIVE_EVENTS {
            assert!(live.contains(event), "{event}");
        }
    }

    #[test]
    fn command_live_switch_is_reconciled_from_the_current_config() {
        let mut config = command();
        if let StatusLine::Command(command) = &mut config {
            command.live_tokens.enabled = false;
        }
        let off = desired_events(Some(&config), false, false);
        assert!(!off.contains("message_start"));
        assert!(!off.contains("message_update"));
        if let StatusLine::Command(command) = &mut config {
            command.live_tokens.enabled = true;
        }
        let on = desired_events(Some(&config), true, false);
        for event in LIVE_EVENTS {
            assert!(on.contains(event), "{event}");
        }
    }

    #[test]
    fn items_duration_only_subscribes_no_extra_collection() {
        // `session-duration` alone is a local tick; no message, totals or Git
        // events are needed.
        let desired = desired_events(
            Some(&items(&[FieldId::SessionDuration], false)),
            false,
            false,
        );
        assert_eq!(desired, BASE_EVENTS.iter().copied().collect());
        assert!(!desired.contains("message_end"));
        assert!(!desired.contains("tool_execution_end"));
        assert!(!desired.contains("turn_end"));
        assert!(!desired.contains("session_compact"));
        assert!(!desired.contains("mode_change"));
        assert!(!desired.contains("agent_start"));
    }

    #[test]
    fn items_default_preset_subscribes_session_totals_and_live_rate() {
        // The shipped fresh-install preset is `live-total-tokens` +
        // `live-output-rate` with liveTokens on: totals need the session-stats
        // triggers, the rate needs the live streaming pair, and nothing else
        // (no Git/mode/run-state) is collected.
        let defaults = ItemsConfig::default();
        assert_eq!(
            defaults.fields().map(FieldId::id).collect::<Vec<_>>(),
            vec!["live-total-tokens", "live-output-rate"]
        );
        let desired = desired_events(Some(&StatusLine::Items(defaults)), false, false);
        for event in ["turn_end", "agent_settled", "session_compact"] {
            assert!(desired.contains(event), "default totals add {event}");
        }
        for event in LIVE_EVENTS {
            assert!(desired.contains(event), "default rate adds {event}");
        }
        for event in ["tool_execution_end", "mode_change", "agent_start"] {
            assert!(
                !desired.contains(event),
                "default preset must not subscribe {event}"
            );
        }
    }

    #[test]
    fn items_capabilities_map_to_their_events() {
        let totals = desired_events(Some(&items(&[FieldId::UsedTokens], false)), false, false);
        // `session_compact` is a totals trigger only in items mode.
        assert!(totals.contains("turn_end"));
        assert!(totals.contains("agent_settled"));
        assert!(totals.contains("session_compact"));
        assert!(!totals.contains("message_end"));

        for field in [FieldId::TotalTokens, FieldId::TurnCount, FieldId::StepCount] {
            let desired = desired_events(Some(&items(&[field], false)), false, false);
            assert!(desired.contains("turn_end"), "{field:?} adds turn_end");
            assert!(
                desired.contains("agent_settled"),
                "{field:?} adds agent_settled"
            );
            assert!(
                desired.contains("session_compact"),
                "{field:?} adds session_compact"
            );
        }

        let run_state = desired_events(Some(&items(&[FieldId::RunState], false)), false, false);
        assert!(run_state.contains("agent_start"));
        assert!(run_state.contains("agent_settled"));

        let mode = desired_events(
            Some(&items(&[FieldId::PermissionMode], false)),
            false,
            false,
        );
        assert!(mode.contains("mode_change"));

        let git = desired_events(Some(&items(&[FieldId::GitBranch], false)), false, false);
        assert!(git.contains("tool_execution_end"));

        let live = desired_events(
            Some(&items(&[FieldId::LiveOutputTokens], true)),
            false,
            false,
        );
        for event in LIVE_EVENTS {
            assert!(live.contains(event), "{event}");
        }
    }

    #[test]
    fn live_switch_alone_does_not_subscribe_events() {
        let desired = desired_events(Some(&items(&[FieldId::Model], true)), false, false);
        for event in LIVE_EVENTS_PROBE {
            assert!(!desired.contains(event), "{event}");
        }
    }

    #[test]
    fn live_probe_subscribes_both_updates_then_drops_the_legacy_event() {
        let config = items(&[FieldId::LiveOutputRate], true);
        let probing = desired_events(Some(&config), false, false);
        for event in LIVE_EVENTS_PROBE {
            assert!(probing.contains(event), "probe is missing {event}");
        }

        let supported = desired_events(Some(&config), false, true);
        for event in LIVE_EVENTS_DELTA {
            assert!(supported.contains(event), "delta mode is missing {event}");
        }
        assert!(!supported.contains("message_update"));
    }
}
