//! Plan-mode state machine: the host permission mode is the single
//! authority (01 §2 R-PM-1.3); this module mirrors it into the exposure
//! override boundary and the editor hint.
//!
//! Events (`mode_change` / `session_start` / `session_tree` /
//! `mcp_servers_change` / `before_agent_start`) all funnel into
//! [`reconcile`], which:
//!
//! - reads the host mode and re-derives the Plan boundary from the live
//!   `getAllTools` surface (so tools registered mid-plan — codemode, MCP,
//!   subagents — are re-tightened before the next request);
//! - on the first Plan reconcile, snapshots `getActiveTools` and applies
//!   the whitelist active set; on leaving Plan, clears the recorded
//!   exposure names and restores the snapshot;
//! - keeps the plan-file path and the editor hint in step.
//!
//! State is keyed by session id, so it survives an extension reload (the
//! native library stays mapped) and fresh sessions start clean. The state
//! mutex is never held across a host call (host calls can re-enter this
//! plugin); [`RECONCILE_LOCK`] serializes boundary application and is held
//! across host calls on purpose.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde_json::{Map, Value, json};

use crate::HostCall;
use crate::config;
use crate::i18n;
use crate::plan_file;
use crate::view;
use crate::whitelist;

/// Widget key for the Plan-mode editor hint (the host namespaces it per
/// extension).
pub const HINT_WIDGET_KEY: &str = "plan-mode-hint";

/// Per-session Plan-mode mirror state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionPlanState {
    /// Whether this plugin instance applied the boundary for the session.
    pub in_plan: bool,
    /// The active-tool snapshot captured on entry (restored on exit).
    pub active_snapshot: Option<Vec<String>>,
    /// The exposure names to clear on exit (the full boundary complement,
    /// including entries already hidden before entry — clearing a name
    /// without an override is a no-op).
    pub hidden_targets: Vec<String>,
    /// Every registered tool name captured on entry: tools hidden later that
    /// are not in this set registered during Plan and return to the active
    /// set on exit (v0.1.6 review P2-10). A pre-entry tool that only enters
    /// the boundary through a hot config change stays out of the late set.
    pub entry_known: Vec<String>,
    /// Whether the editor hint widget is currently registered.
    pub hint_shown: bool,
    /// The current plan file for the session; `None` until the first
    /// write (or after a new Plan entry). Kept across exit so `/plan file`
    /// can report the last plan.
    pub plan_path: Option<PathBuf>,
    /// Whether the user was already warned that leaving Plan mode could not
    /// release the boundary; the warning fires once per entry so a
    /// persistent host outage does not re-notify on every event (round-2
    /// review note).
    pub exit_cleanup_notified: bool,
    /// Whether the user was already warned that the pre-plan active set
    /// could not be restored on a clean exit; fires once per entry so a
    /// retry loop cannot repeat it (round-2 review note).
    pub degraded_exit_notified: bool,
}

static STATES: OnceLock<Mutex<HashMap<String, SessionPlanState>>> = OnceLock::new();

/// Serializes reconciles: the `/plan` command runs one synchronously and
/// the spawned `mode_change` event may race it; without the lock both
/// snapshots could predate the other's commit (double entry would
/// re-snapshot the already-rewritten active set).
static RECONCILE_LOCK: Mutex<()> = Mutex::new(());

fn states() -> &'static Mutex<HashMap<String, SessionPlanState>> {
    STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn snapshot(session: &str) -> SessionPlanState {
    states()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(session)
        .cloned()
        .unwrap_or_default()
}

fn commit(session: &str, state: SessionPlanState) {
    states()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(session.to_owned(), state);
}

/// Recompute the Plan boundary from the host mode + live tool surface.
/// Returns whether the session is in Plan mode after the reconcile.
pub fn reconcile(host: &dyn HostCall) -> bool {
    let _guard = RECONCILE_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let session = crate::host::sid_of(host);
    // An unreadable host mode must not be treated as `default`: that would
    // look like leaving Plan mode and release the boundary (O4). Keep the
    // mirror state and retry on the next event.
    let Some(mode) = crate::host::get_mode(host) else {
        tracing::warn!("rpi-plan-mode: getMode failed; leaving the Plan boundary untouched");
        return snapshot(&session).in_plan;
    };
    let plan = mode == "plan";
    let mut state = snapshot(&session);

    if plan {
        let config = config::load_config();
        // A failed tool snapshot is not an empty tool surface: releasing the
        // hidden targets against it would fail open (O4). Keep the current
        // boundary and retry on the next event.
        let Some(raw_tools) = crate::host::get_all_tools(host) else {
            tracing::warn!(
                "rpi-plan-mode: getAllTools failed; keeping the current boundary and retrying"
            );
            return state.in_plan;
        };
        let tools = whitelist::parse_tools(&raw_tools);
        let targets = whitelist::boundary_targets(&tools, &config);
        let active = whitelist::active_whitelist(&tools, &config);
        let entering = !state.in_plan;
        let current_active = crate::host::get_active_tools(host);
        if entering {
            // The snapshot backs the exit restore; a failed call leaves it
            // None and the exit keeps the active set instead of restoring a
            // wrong one.
            state.active_snapshot = current_active.clone();
            state.entry_known = tools.iter().map(|tool| tool.name.clone()).collect();
            // A Plan entry starts a fresh plan file (revisions within the
            // entry overwrite it); session events reset it the same way.
            state.plan_path = None;
            state.hint_shown = false;
            state.exit_cleanup_notified = false;
            state.degraded_exit_notified = false;
        }
        let newly = whitelist::newly_hidden(&tools, &config);
        if !entering {
            // Config hot change: a name we hid earlier that is whitelisted
            // again returns to its natural exposure before new targets are
            // hidden.
            let release: Vec<String> = state
                .hidden_targets
                .iter()
                .filter(|name| !targets.contains(name))
                .cloned()
                .collect();
            if !release.is_empty()
                && let Err(error) = host.call("clearToolExposures", json!({ "names": release }))
            {
                tracing::warn!(%error, "rpi-plan-mode: exposure release failed");
            }
        }
        if !newly.is_empty() {
            let mut exposures = Map::new();
            for name in &newly {
                exposures.insert(name.clone(), Value::String("hidden".to_owned()));
            }
            if let Err(error) = host.call("setToolExposures", json!({ "exposures": exposures })) {
                tracing::warn!(%error, "rpi-plan-mode: setToolExposures failed");
            }
        }
        // Keep the active set equal to the whitelist (late-registered or
        // hot-re-allowed tools become usable; a user edit mid-plan is
        // re-asserted at the next trigger — the Plan boundary is the
        // authority while the mode is active). A failed `getActiveTools`
        // still asserts the whitelist (the boundary is the safe default).
        let active_mismatch = current_active
            .as_ref()
            .is_none_or(|current| *current != active);
        if active_mismatch
            && let Err(error) = host.call("setActiveTools", json!({ "toolNames": active }))
        {
            tracing::warn!(%error, "rpi-plan-mode: setActiveTools failed");
        }
        if !state.hint_shown && crate::host::has_ui(host) {
            let content = Value::Array(view::hint_lines().into_iter().map(Value::String).collect());
            match host.call(
                "ui.setWidget",
                json!({
                    "key": HINT_WIDGET_KEY,
                    "content": content,
                    "placement": "aboveEditor",
                }),
            ) {
                Ok(_) => state.hint_shown = true,
                Err(error) => {
                    tracing::warn!(%error, "rpi-plan-mode: hint widget push failed");
                }
            }
        }
        state.in_plan = true;
        state.hidden_targets = targets;
        commit(&session, state);
        return true;
    }

    if state.in_plan {
        let mut cleanup_failed = false;
        let mut degraded_snapshot = false;
        if !state.hidden_targets.is_empty()
            && let Err(error) = host.call(
                "clearToolExposures",
                json!({ "names": state.hidden_targets }),
            )
        {
            tracing::warn!(%error, "rpi-plan-mode: clearToolExposures failed");
            cleanup_failed = true;
        }
        // Restore the entry snapshot AFTER the release, augmented with the
        // tools that were hidden only after entry (for example an MCP server
        // that connected mid-plan): the release re-activates them in the
        // session, and restoring the bare snapshot afterwards wiped them
        // again (v0.1.6 review P2-10). Tools that existed at entry keep
        // exactly their pre-plan active state — including ones that were
        // inactive before entry.
        let late_registered: Vec<String> = state
            .hidden_targets
            .iter()
            .filter(|name| !state.entry_known.contains(name))
            .cloned()
            .collect();
        if let Some(snapshot) = state.active_snapshot.clone() {
            let mut restored = snapshot;
            for name in &late_registered {
                if !restored.contains(name) {
                    restored.push(name.clone());
                }
            }
            if let Err(error) = host.call("setActiveTools", json!({ "toolNames": restored })) {
                tracing::warn!(%error, "rpi-plan-mode: active-set restore failed");
                cleanup_failed = true;
            }
        } else if !state.hidden_targets.is_empty() {
            tracing::warn!(
                "rpi-plan-mode: no active-set snapshot was captured on entry; keeping the active set"
            );
            degraded_snapshot = true;
        }
        if cleanup_failed {
            // Keep the mirror state so the next event retries the release
            // instead of leaking the hidden overrides for the rest of the
            // session (v0.1.6 review round 2, O6), and tell the user once
            // per Plan entry (a persistent outage would otherwise re-notify
            // on every before_agent_start; round-2 review note). The
            // degraded-snapshot note waits for a clean exit so the message
            // matches what actually happened.
            if !state.exit_cleanup_notified {
                let _ = host.call(
                    "ui.notify",
                    json!({
                        "message": i18n::EXIT_CLEANUP_FAILED,
                        "notifyType": "warning",
                    }),
                );
                state.exit_cleanup_notified = true;
            }
            // Persist the notification latch while keeping the boundary
            // state untouched for the retry.
            commit(&session, state);
            return false;
        }
        if degraded_snapshot && !state.degraded_exit_notified {
            state.degraded_exit_notified = true;
            let _ = host.call(
                "ui.notify",
                json!({
                    "message": i18n::ACTIVE_SET_NOT_RESTORED,
                    "notifyType": "warning",
                }),
            );
        }
        if state.hint_shown && crate::host::has_ui(host) {
            let _ = host.call(
                "ui.setWidget",
                json!({
                    "key": HINT_WIDGET_KEY,
                    "content": Value::Null,
                    "placement": "aboveEditor",
                }),
            );
        }
        state.in_plan = false;
        state.active_snapshot = None;
        state.hidden_targets.clear();
        state.entry_known.clear();
        state.hint_shown = false;
        state.exit_cleanup_notified = false;
        state.degraded_exit_notified = false;
        // `plan_path` deliberately survives: `/plan file` reports the last
        // plan after exit, and a new Plan entry resets it.
        commit(&session, state);
    }
    false
}

/// A `session_start` (including the reload replay) invalidates host-side
/// widget chrome: re-arm the hint so the next reconcile re-pushes it on
/// the rebuilt UI, then reconcile.
pub fn on_session_start(host: &dyn HostCall) -> bool {
    let session = crate::host::sid_of(host);
    let mut state = snapshot(&session);
    state.hint_shown = false;
    commit(&session, state);
    reconcile(host)
}

/// Drop the session's mirror state (`session_shutdown`). The boundary
/// should already be released; dropping the entry keeps [`STATES`] bounded
/// across the sessions one process hosts (v0.1.6 review round 2).
pub fn forget_session(host: &dyn HostCall) {
    let session = crate::host::sid_of(host);
    if session.is_empty() {
        return;
    }
    states()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(&session);
}

/// The plan path for this session: the remembered file, or the next free
/// `<session>-<n>.md` path without creating anything.
pub fn current_plan_path(host: &dyn HostCall) -> Option<String> {
    let session = crate::host::sid_of(host);
    let state = snapshot(&session);
    if let Some(path) = state.plan_path {
        return Some(path.to_string_lossy().into_owned());
    }
    let cwd = crate::host::cwd_of(host)?;
    let key = plan_file::session_key(&session);
    let dir = plan_file::plan_dir(
        std::path::Path::new(&cwd),
        config::load_config().plan_dir.as_deref(),
    );
    Some(
        plan_file::next_path(&dir, &key)
            .to_string_lossy()
            .into_owned(),
    )
}

/// Remember the plan file written for this session.
pub fn remember_plan_path(host: &dyn HostCall, path: PathBuf) {
    let session = crate::host::sid_of(host);
    let mut state = snapshot(&session);
    state.plan_path = Some(path);
    commit(&session, state);
}

/// The remembered plan path (may not exist on disk yet), for the first
/// write of a Plan entry.
pub fn cached_plan_path(host: &dyn HostCall) -> Option<PathBuf> {
    let session = crate::host::sid_of(host);
    snapshot(&session).plan_path
}

/// The remembered plan path (existing file only), for `/plan file|edit`.
pub fn existing_plan_path(host: &dyn HostCall) -> Option<PathBuf> {
    let session = crate::host::sid_of(host);
    snapshot(&session).plan_path.filter(|path| path.is_file())
}

/// Test seam: read one session's state.
#[cfg(test)]
pub fn state_for_test(session: &str) -> SessionPlanState {
    snapshot(session)
}

/// Test seam: clear all state.
#[cfg(test)]
pub fn reset_for_test() {
    states()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::TEST_LOCK;
    use crate::config;
    use crate::test_host::SessionFakeHost;

    fn serialized() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Fake host with the plugin's write_plan registered, in Default mode.
    fn base_host() -> SessionFakeHost {
        let host = SessionFakeHost::new();
        host.register_tool("write_plan", "direct", false);
        host
    }

    #[test]
    fn enter_plan_hides_targets_and_sets_the_whitelist() {
        let _guard = serialized();
        crate::__reset_state();
        let host = base_host();
        host.set_mode("plan");
        assert!(reconcile(&host));
        assert_eq!(
            host.view().exposures,
            vec![
                ("read".to_owned(), "direct".to_owned()),
                ("edit".to_owned(), "hidden".to_owned()),
                ("write".to_owned(), "hidden".to_owned()),
                ("bash".to_owned(), "hidden".to_owned()),
                ("write_plan".to_owned(), "direct".to_owned()),
            ]
        );
        assert_eq!(host.view().active, vec!["read", "write_plan"]);
        assert_eq!(
            host.view().widget,
            Some(crate::view::hint_lines()),
            "editor hint registered"
        );
        let state = state_for_test("s-1");
        assert!(state.in_plan);
        assert_eq!(state.hidden_targets, vec!["edit", "write", "bash"]);
        assert_eq!(
            state.active_snapshot,
            Some(vec![
                "read".to_owned(),
                "edit".to_owned(),
                "bash".to_owned()
            ])
        );
    }

    #[test]
    fn exit_plan_clears_the_recorded_names_and_restores_the_snapshot() {
        let _guard = serialized();
        crate::__reset_state();
        let host = base_host();
        host.set_mode("plan");
        reconcile(&host);
        host.set_mode("default");
        assert!(!reconcile(&host));
        assert_eq!(
            host.view().exposures,
            vec![
                ("read".to_owned(), "direct".to_owned()),
                ("edit".to_owned(), "direct".to_owned()),
                ("write".to_owned(), "direct".to_owned()),
                ("bash".to_owned(), "direct".to_owned()),
                ("write_plan".to_owned(), "direct".to_owned()),
            ],
            "natural exposures return"
        );
        assert_eq!(
            host.view().active,
            vec!["read", "edit", "bash"],
            "the entry snapshot is restored"
        );
        assert_eq!(host.view().widget, None, "hint removed");
        let state = state_for_test("s-1");
        assert!(!state.in_plan);
        assert!(state.hidden_targets.is_empty());
        assert!(state.entry_known.is_empty());
        assert!(state.active_snapshot.is_none());
    }

    /// v0.1.6 review P2-10: a tool registered while Plan is active (for
    /// example an MCP server that connected mid-plan) is hidden by the
    /// boundary and must return to the active set on exit; restoring the
    /// bare entry snapshot afterwards dropped it.
    #[test]
    fn exit_plan_activates_tools_registered_during_plan() {
        let _guard = serialized();
        crate::__reset_state();
        let host = base_host();
        host.set_mode("plan");
        reconcile(&host);
        // A server connects mid-plan and registers a direct tool; the next
        // reconcile hides it with the rest of the boundary.
        host.register_tool("mcp_extra", "direct", false);
        reconcile(&host);
        let state = state_for_test("s-1");
        assert!(state.hidden_targets.contains(&"mcp_extra".to_owned()));
        assert!(!state.entry_known.contains(&"mcp_extra".to_owned()));
        assert_eq!(
            host.view().exposures.last(),
            Some(&("mcp_extra".to_owned(), "hidden".to_owned()))
        );

        host.set_mode("default");
        assert!(!reconcile(&host));
        assert_eq!(
            host.view().active,
            vec![
                "read".to_owned(),
                "edit".to_owned(),
                "bash".to_owned(),
                "mcp_extra".to_owned(),
            ],
            "late-registered tool is active after exit"
        );
    }

    #[test]
    fn reconcile_is_idempotent_while_in_plan() {
        let _guard = serialized();
        crate::__reset_state();
        let host = base_host();
        host.set_mode("plan");
        reconcile(&host);
        let before = host.calls().len();
        reconcile(&host);
        let calls = host.calls();
        let after = calls[before..]
            .iter()
            .map(|(method, _)| method.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            after,
            vec![
                "ctx.sessionFile",
                "getMode",
                "getAllTools",
                "getActiveTools"
            ]
        );
        assert_eq!(host.view().active, vec!["read", "write_plan"]);
    }

    #[test]
    fn late_registered_tools_are_re_tightened_and_cleared() {
        let _guard = serialized();
        crate::__reset_state();
        let host = base_host();
        host.set_mode("plan");
        reconcile(&host);
        host.register_tool("mcp_thing", "direct", true);
        reconcile(&host);
        assert_eq!(
            host.view()
                .exposures
                .iter()
                .find(|(name, _)| name == "mcp_thing")
                .map(|(_, exposure)| exposure.as_str()),
            Some("hidden")
        );
        assert_eq!(
            state_for_test("s-1").hidden_targets,
            vec!["edit", "write", "bash", "mcp_thing"]
        );
        host.set_mode("default");
        reconcile(&host);
        assert_eq!(
            host.view()
                .exposures
                .iter()
                .find(|(name, _)| name == "mcp_thing")
                .map(|(_, exposure)| exposure.as_str()),
            Some("direct"),
            "late tool returns to its natural exposure"
        );
    }

    #[test]
    fn hot_config_changes_recompute_the_boundary_on_the_next_trigger() {
        let _guard = serialized();
        crate::__reset_state();
        config::set_test_config(Some(Some("allowTools = [\"read\", \"edit\"]".to_owned())));
        let host = base_host();
        host.set_mode("plan");
        reconcile(&host);
        assert_eq!(
            host.view()
                .exposures
                .iter()
                .find(|(name, _)| name == "edit")
                .map(|(_, exposure)| exposure.as_str()),
            Some("direct"),
            "edit is allowed by the first config"
        );
        config::set_test_config(Some(Some("allowTools = [\"read\"]".to_owned())));
        reconcile(&host);
        assert_eq!(
            host.view()
                .exposures
                .iter()
                .find(|(name, _)| name == "edit")
                .map(|(_, exposure)| exposure.as_str()),
            Some("hidden"),
            "the hot edit hides edit at the next trigger"
        );
        // The reverse direction: a re-allowed name is released back to its
        // natural exposure (not left hidden from the previous config).
        config::set_test_config(Some(Some("allowTools = [\"read\", \"edit\"]".to_owned())));
        reconcile(&host);
        assert_eq!(
            host.view()
                .exposures
                .iter()
                .find(|(name, _)| name == "edit")
                .map(|(_, exposure)| exposure.as_str()),
            Some("direct"),
            "re-allowed names are released"
        );
        assert_eq!(
            host.view().active,
            vec!["read", "edit", "write_plan"],
            "re-allowed names rejoin the active whitelist"
        );
    }

    #[test]
    fn naturally_hidden_tools_are_skipped_but_recovered() {
        let _guard = serialized();
        crate::__reset_state();
        let host = base_host();
        host.register_tool("secret", "hidden", false);
        host.set_mode("plan");
        reconcile(&host);
        let state = state_for_test("s-1");
        assert!(
            state.hidden_targets.contains(&"secret".to_owned()),
            "recovery list keeps the natural hidden name"
        );
        // The fake's `updated`/`cleared` lists model the layer: a natural
        // hidden tool never receives an override, so clearing it is a
        // no-op (the effective exposure is unchanged either way).
        host.set_mode("default");
        reconcile(&host);
        assert_eq!(
            host.view()
                .exposures
                .iter()
                .find(|(name, _)| name == "secret")
                .map(|(_, exposure)| exposure.as_str()),
            Some("hidden")
        );
    }

    #[test]
    fn no_ui_skips_the_hint_widget() {
        let _guard = serialized();
        crate::__reset_state();
        let host = base_host();
        host.set_has_ui(false);
        host.set_mode("plan");
        reconcile(&host);
        assert_eq!(host.view().widget, None);
        assert!(!state_for_test("s-1").hint_shown);
    }

    #[test]
    fn current_plan_path_derives_after_the_cwd_and_respects_the_override() {
        let _guard = serialized();
        crate::__reset_state();
        let host = base_host();
        assert_eq!(
            current_plan_path(&host).as_deref(),
            Some("/work/cwd/.rpi/plans/s-1-1.md")
        );
        config::set_test_config(Some(Some("planDir = \"plans\"".to_owned())));
        assert_eq!(
            current_plan_path(&host).as_deref(),
            Some("/work/cwd/plans/s-1-1.md")
        );
    }

    #[test]
    fn session_start_rearms_the_hint() {
        let _guard = serialized();
        crate::__reset_state();
        let host = base_host();
        host.set_mode("plan");
        reconcile(&host);
        assert!(state_for_test("s-1").hint_shown);
        // The fake records the widget push; a re-arm must push it again.
        on_session_start(&host);
        let pushes = host
            .calls()
            .into_iter()
            .filter(|(method, _)| method == "ui.setWidget")
            .count();
        assert_eq!(pushes, 2, "session_start re-pushes the hint");
    }

    #[test]
    fn remember_and_cached_path_are_session_scoped() {
        let _guard = serialized();
        crate::__reset_state();
        let host = base_host();
        assert!(cached_plan_path(&host).is_none());
        remember_plan_path(&host, PathBuf::from("/tmp/x/s-1-1.md"));
        assert_eq!(
            cached_plan_path(&host),
            Some(PathBuf::from("/tmp/x/s-1-1.md"))
        );
        host.set_session_id("s-2");
        assert!(cached_plan_path(&host).is_none(), "state is session-scoped");
    }

    /// Round-2 O4: a failed `getAllTools` is not an empty tool surface;
    /// the boundary must survive and the next event retries. The old
    /// empty-vec folding released every hidden override (fail open).
    #[test]
    fn failed_tool_snapshot_keeps_the_boundary() {
        let _guard = serialized();
        crate::__reset_state();
        let host = base_host();
        host.set_mode("plan");
        assert!(reconcile(&host));
        let entered = host.view();
        host.fail_method("getAllTools");
        assert!(reconcile(&host), "the host mode is still plan");
        assert_eq!(
            host.view().exposures,
            entered.exposures,
            "a failed snapshot must not release the hidden overrides"
        );
        assert_eq!(host.view().active, entered.active);
        // Recovery: the retry is idempotent with the entry boundary.
        host.clear_failed_methods();
        assert!(reconcile(&host));
        assert_eq!(host.view().exposures, entered.exposures);
        assert_eq!(host.view().active, entered.active);
    }

    /// Round-2 O4: a failed `getMode` must not look like an exit; the
    /// boundary survives until the host answers again.
    #[test]
    fn failed_get_mode_keeps_the_boundary() {
        let _guard = serialized();
        crate::__reset_state();
        let host = base_host();
        host.set_mode("plan");
        assert!(reconcile(&host));
        let entered = host.view();
        host.fail_method("getMode");
        assert!(reconcile(&host), "the mirror state is still in plan");
        assert_eq!(host.view().exposures, entered.exposures);
        assert_eq!(host.view().active, entered.active);
        host.clear_failed_methods();
        assert!(reconcile(&host));
    }

    /// Round-2 O4: a failed `getActiveTools` still asserts the whitelist
    /// (the boundary is the safe default) and the exit keeps the active set
    /// instead of restoring a wrong snapshot.
    #[test]
    fn failed_active_snapshot_keeps_the_whitelist() {
        let _guard = serialized();
        crate::__reset_state();
        let host = base_host();
        host.set_mode("plan");
        host.fail_method("getActiveTools");
        assert!(reconcile(&host));
        assert_eq!(host.view().active, vec!["read", "write_plan"]);
        host.clear_failed_methods();
        host.set_mode("default");
        assert!(!reconcile(&host));
        assert_eq!(
            host.view().active,
            vec!["read", "write_plan"],
            "without a snapshot the active set is left as-is"
        );
        assert!(
            host.view()
                .notifications
                .iter()
                .any(|message| message.contains("could not be restored")),
            "the degraded exit must warn: {:?}",
            host.view().notifications
        );
    }

    /// Round-2 O6: an exit whose cleanup fails keeps the mirror state,
    /// warns the user, and retries on the next event instead of leaking the
    /// hidden overrides for the rest of the session.
    #[test]
    fn failed_exit_cleanup_is_notified_and_retried() {
        let _guard = serialized();
        crate::__reset_state();
        let host = base_host();
        host.set_mode("plan");
        assert!(reconcile(&host));
        host.set_mode("default");
        host.fail_method("clearToolExposures");
        assert!(!reconcile(&host));
        assert!(
            host.view()
                .notifications
                .iter()
                .any(|message| message.contains("could not be restored")),
            "the user must see the failed cleanup: {:?}",
            host.view().notifications
        );
        assert_eq!(
            host.view()
                .exposures
                .iter()
                .find(|(name, _)| name == "edit")
                .map(|(_, exposure)| exposure.as_str()),
            Some("hidden"),
            "the boundary is still applied until the retry succeeds"
        );
        // A second failure retries but does not re-notify (one warning per
        // Plan entry; round-2 review note).
        let notifications_after_first = host.view().notifications.len();
        assert!(!reconcile(&host));
        assert_eq!(
            host.view().notifications.len(),
            notifications_after_first,
            "a persistent outage must not re-notify on every event"
        );
        // Recovery: the retry releases the boundary and clears the state.
        host.clear_failed_methods();
        assert!(!reconcile(&host));
        assert_eq!(
            host.view().exposures,
            vec![
                ("read".to_owned(), "direct".to_owned()),
                ("edit".to_owned(), "direct".to_owned()),
                ("write".to_owned(), "direct".to_owned()),
                ("bash".to_owned(), "direct".to_owned()),
                ("write_plan".to_owned(), "direct".to_owned()),
            ]
        );
        assert_eq!(host.view().active, vec!["read", "edit", "bash"]);
        assert!(!state_for_test("s-1").in_plan);
    }

    /// Round-2 follow-up: when the entry snapshot is unavailable AND the
    /// cleanup keeps failing, the user gets the accurate cleanup note (once)
    /// and the degraded-snapshot note only after a clean exit — never a
    /// re-notify on every event, and never an "off" message while the
    /// boundary is still applied.
    #[test]
    fn combined_snapshot_and_cleanup_failures_notify_once() {
        let _guard = serialized();
        crate::__reset_state();
        let host = base_host();
        host.set_mode("plan");
        host.fail_method("getActiveTools");
        assert!(reconcile(&host), "entry without a snapshot still hides");
        assert_eq!(host.view().active, vec!["read", "write_plan"]);
        host.clear_failed_methods();
        host.set_mode("default");
        host.fail_method("clearToolExposures");
        assert!(!reconcile(&host));
        let first: Vec<String> = host.view().notifications;
        assert!(
            first
                .iter()
                .any(|message| message.contains("cleanup will be retried")),
            "the failing cleanup must be named: {first:?}"
        );
        assert!(
            !first
                .iter()
                .any(|message| message.contains("pre-plan active tool set")),
            "the degraded note must wait for a clean exit: {first:?}"
        );
        assert!(!reconcile(&host));
        assert_eq!(
            host.view().notifications,
            first,
            "a persistent outage must not re-notify"
        );
        host.clear_failed_methods();
        assert!(!reconcile(&host));
        assert!(
            host.view()
                .notifications
                .iter()
                .any(|message| message.contains("pre-plan active tool set")),
            "the recovered exit reports the unrestored active set: {:?}",
            host.view().notifications
        );
        assert!(!state_for_test("s-1").in_plan);
    }

    /// Round-2: `session_shutdown` evicts the entry so STATES stays bounded
    /// across the sessions one process hosts.
    #[test]
    fn session_shutdown_forgets_the_mirror_state() {
        let _guard = serialized();
        crate::__reset_state();
        let host = base_host();
        host.set_mode("plan");
        assert!(reconcile(&host));
        assert!(
            states()
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .contains_key("s-1")
        );
        forget_session(&host);
        assert!(
            !states()
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .contains_key("s-1"),
            "session_shutdown must drop the per-session state"
        );
    }
}
