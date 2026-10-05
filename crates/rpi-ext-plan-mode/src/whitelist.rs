//! Read-only boundary computation for Plan mode (TE43 FR-B; plugin 02 §4).
//!
//! The pure layer turns a `getAllTools` snapshot plus the configured
//! allow/block lists into:
//!
//! - `active`: the Plan-mode active set — `(registered ∩ (allowTools −
//!   blockTools)) ∪ {write_plan}`, in registration order;
//! - `targets`: every registered tool outside that set — the names the
//!   caller hides via `setToolExposures` and, on exit, clears via
//!   `clearToolExposures`.
//!
//! `write_plan` is never a target (it is the Plan-mode write channel),
//! even when a config lists it in `blockTools` (01 §3 R-PM-2.5). Unknown
//! configured names are ignored (they are simply not part of the
//! registered intersection).

use serde_json::Value;

use crate::config::{PlanModeConfig, WRITE_PLAN_TOOL};

/// One registered tool from a `getAllTools` snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolEntry {
    /// Registered tool name.
    pub name: String,
    /// Current exposure wire value (`direct` / `model-only` / `codemode` /
    /// `deferred` / `hidden`).
    pub exposure: String,
}

impl ToolEntry {
    /// Whether the tool already sits at the `hidden` exposure (the entry
    /// path skips it and does not report it in `updated`).
    pub fn is_hidden(&self) -> bool {
        self.exposure == "hidden"
    }
}

/// Parse a `getAllTools` payload into entries (unknown shapes are skipped;
/// a missing `exposure` reads as the registration default `direct`).
pub fn parse_tools(all_tools: &[Value]) -> Vec<ToolEntry> {
    all_tools
        .iter()
        .filter_map(|entry| {
            let name = entry.get("name")?.as_str()?.to_owned();
            let exposure = entry
                .get("exposure")
                .and_then(Value::as_str)
                .unwrap_or("direct")
                .to_owned();
            Some(ToolEntry { name, exposure })
        })
        .collect()
}

/// The effective allow set: `allowTools − blockTools` (names are compared
/// verbatim; unknown configured names simply never match a registered
/// tool).
pub fn effective_allow(config: &PlanModeConfig) -> Vec<String> {
    config
        .allow_tools
        .iter()
        .filter(|name| !config.block_tools.contains(name))
        .cloned()
        .collect()
}

/// Names to hide: every registered tool outside the effective allow set,
/// excluding `write_plan`. Registration order is preserved.
pub fn boundary_targets(tools: &[ToolEntry], config: &PlanModeConfig) -> Vec<String> {
    let allow = effective_allow(config);
    tools
        .iter()
        .filter(|tool| tool.name != WRITE_PLAN_TOOL)
        .filter(|tool| !allow.contains(&tool.name))
        .map(|tool| tool.name.clone())
        .collect()
}

/// The Plan-mode active set: registered allow-set names in registration
/// order, plus `write_plan` when registered.
pub fn active_whitelist(tools: &[ToolEntry], config: &PlanModeConfig) -> Vec<String> {
    let allow = effective_allow(config);
    let mut names: Vec<String> = tools
        .iter()
        .filter(|tool| allow.contains(&tool.name))
        .map(|tool| tool.name.clone())
        .collect();
    if tools.iter().any(|tool| tool.name == WRITE_PLAN_TOOL) {
        names.push(WRITE_PLAN_TOOL.to_owned());
    }
    names
}

/// Names that need a fresh `hidden` override: boundary targets whose
/// current exposure is not already `hidden` (the "skip already hidden"
/// rule keeps a pre-existing hiding untouched and out of `updated`).
pub fn newly_hidden(tools: &[ToolEntry], config: &PlanModeConfig) -> Vec<String> {
    let targets = boundary_targets(tools, config);
    let hidden: Vec<&str> = tools
        .iter()
        .filter(|tool| tool.is_hidden())
        .map(|tool| tool.name.as_str())
        .collect();
    targets
        .into_iter()
        .filter(|name| !hidden.contains(&name.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tools(names: &[(&str, &str)]) -> Vec<ToolEntry> {
        names
            .iter()
            .map(|(name, exposure)| ToolEntry {
                name: (*name).to_owned(),
                exposure: (*exposure).to_owned(),
            })
            .collect()
    }

    fn config(allow: &[&str], block: &[&str]) -> PlanModeConfig {
        PlanModeConfig {
            allow_tools: allow.iter().map(|name| (*name).to_owned()).collect(),
            block_tools: block.iter().map(|name| (*name).to_owned()).collect(),
            plan_dir: None,
            prompt_injection: true,
        }
    }

    #[test]
    fn boundary_is_the_registered_complement_with_write_plan_kept() {
        let all = tools(&[
            ("read", "direct"),
            ("edit", "direct"),
            ("write", "direct"),
            ("bash", "direct"),
            ("write_plan", "direct"),
            ("web_fetch", "direct"),
        ]);
        let config = config(&["read", "web_fetch"], &[]);
        assert_eq!(
            boundary_targets(&all, &config),
            vec!["edit", "write", "bash"]
        );
        assert_eq!(
            active_whitelist(&all, &config),
            vec!["read", "web_fetch", "write_plan"]
        );
    }

    #[test]
    fn block_wins_over_allow_and_write_plan_stays() {
        let all = tools(&[
            ("read", "direct"),
            ("web_fetch", "direct"),
            ("write_plan", "direct"),
        ]);
        let config = config(
            &["read", "web_fetch", "write_plan"],
            &["web_fetch", "write_plan"],
        );
        assert_eq!(boundary_targets(&all, &config), vec!["web_fetch"]);
        assert_eq!(
            active_whitelist(&all, &config),
            vec!["read", "write_plan"],
            "write_plan is a reserved member regardless of blockTools"
        );
    }

    #[test]
    fn unknown_configured_names_are_ignored() {
        let all = tools(&[("read", "direct")]);
        let config = config(&["read", "no_such_tool"], &["also_missing"]);
        assert!(boundary_targets(&all, &config).is_empty());
        assert_eq!(active_whitelist(&all, &config), vec!["read"]);
    }

    #[test]
    fn empty_tool_set_yields_empty_sets() {
        let config = PlanModeConfig::default();
        assert!(boundary_targets(&[], &config).is_empty());
        assert!(active_whitelist(&[], &config).is_empty());
    }

    #[test]
    fn newly_hidden_skips_already_hidden_targets() {
        let all = tools(&[
            ("read", "direct"),
            ("edit", "direct"),
            ("write", "hidden"),
            ("bash", "direct"),
        ]);
        let config = config(&["read"], &[]);
        assert_eq!(
            boundary_targets(&all, &config),
            vec!["edit", "write", "bash"]
        );
        assert_eq!(
            newly_hidden(&all, &config),
            vec!["edit", "bash"],
            "already-hidden write is skipped in updated"
        );
    }

    #[test]
    fn parses_get_all_tools_payload_shapes() {
        let payload = vec![
            json!({"name": "read", "exposure": "direct"}),
            json!({"name": "edit"}),
            json!({"exposure": "direct"}),
            json!({"name": 7}),
        ];
        assert_eq!(
            parse_tools(&payload),
            vec![
                ToolEntry {
                    name: "read".to_owned(),
                    exposure: "direct".to_owned()
                },
                ToolEntry {
                    name: "edit".to_owned(),
                    exposure: "direct".to_owned()
                },
            ]
        );
    }
}
