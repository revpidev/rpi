//! Child-side required-tools availability diagnostic (ADR-0017).
//!
//! Port of pi-subagents `src/runs/shared/tool-availability.ts` @ v0.48.0
//! (56f97234). The ADR-0017 P0 exemption set (`intercom`,
//! `contact_supervisor` counted as always-available) was removed with TE05's
//! supervisor channel: `contact_supervisor` is now checked against the
//! child's real tool list like any other tool, and an `intercom` requirement
//! (the name agent frontmatters inherited from the upstream host) is
//! satisfied by the registered `contact_supervisor` — a mapping, not an
//! exemption. Every other missing tool fails the run with the upstream
//! message.

// `Write` is only used by the unix 0o600 write branch below; the
// `cfg(not(unix))` fallback goes through `std::fs::write`.
#[cfg(unix)]
use std::io::Write;
use std::path::Path;

use serde_json::{json, Value};

/// `PI_CORE_CHILD_TOOLS` (tool-availability.ts:16).
pub const PI_CORE_CHILD_TOOLS: [&str; 7] = ["bash", "edit", "find", "grep", "ls", "read", "write"];

/// Frontmatter tool names satisfied by a differently-named registered tool:
/// upstream-host agents advertise `intercom`; rpi children carry the
/// supervisor client as `contact_supervisor` (FR-P1-10).
const TOOL_ALIASES: [(&str, &str); 1] = [("intercom", "contact_supervisor")];

/// Whether a required tool is satisfied by the available set, honouring the
/// alias table above.
fn requirement_satisfied(required: &str, available: &std::collections::BTreeSet<&str>) -> bool {
    if available.contains(required) {
        return true;
    }
    TOOL_ALIASES
        .iter()
        .any(|(name, provider)| name == &required && available.contains(provider))
}

/// Pre-spawn tool-face gate (R7.1.4.3 / #2034, TE18 FR-C): the declared
/// allowlist (minus excluded names) must be covered by the host's tool set
/// before the child launches. rpi fails closed with the ADR-0017 message —
/// the same diagnostic structure and wording the child-side check writes —
/// instead of upstream's silent omission; a host-query error also fails
/// closed (never silently lets the launch through).
///
/// Path-shaped `tools` entries (`/`, `.ts`, `.js`) are extension providers,
/// not registry names, and are not gated (upstream `requestedBuiltinTools`
/// filter, child-tool-plan.ts:336-341). Supervisor-coordination names
/// (`contact_supervisor` and its `intercom` alias) are also exempt — the
/// plugin registers them at runtime inside the child, so they are never in
/// the parent registry (upstream filters them out of the strict
/// requirements the same way, child-tool-plan.ts:374-380).
#[allow(clippy::result_large_err)]
pub fn check_host_tool_face(
    allowlist: &[String],
    exclude_tools: &[String],
    host_tools: Result<&[String], &str>,
    agent_name: &str,
) -> Result<(), String> {
    let excluded: std::collections::BTreeSet<&str> = exclude_tools
        .iter()
        .map(|name| name.trim())
        .filter(|name| !name.is_empty())
        .collect();
    let is_path_shaped =
        |tool: &str| tool.contains('/') || tool.ends_with(".ts") || tool.ends_with(".js");
    let is_coordination_name = |tool: &str| tool == "contact_supervisor" || tool == "intercom";
    let required: Vec<String> = allowlist
        .iter()
        .filter(|tool| !is_path_shaped(tool))
        .filter(|tool| !is_coordination_name(tool))
        .filter(|tool| !excluded.contains(tool.trim()))
        .cloned()
        .collect();
    if required.is_empty() {
        return Ok(());
    }
    let available = host_tools.map_err(|reason| {
        // `getAllTools` failed: the parent cannot vouch for the tool face —
        // refuse the launch instead of spawning blind.
        format!(
            "Agent '{agent_name}' requires tools {} but the host tool face is unavailable ({reason}); refusing to start the subagent without verifying its tools.",
            required.join(", ")
        )
    })?;
    let available_names: std::collections::BTreeSet<&str> =
        available.iter().map(String::as_str).collect();
    let missing: Vec<String> = required
        .iter()
        .filter(|name| !requirement_satisfied(name, &available_names))
        .cloned()
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let diagnostic = ChildToolDiagnostic {
        agent: Some(agent_name.to_string()),
        required,
        available: available.to_vec(),
        missing,
    };
    Err(format_child_tool_diagnostic(&diagnostic))
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChildToolDiagnostic {
    pub agent: Option<String>,
    pub required: Vec<String>,
    pub available: Vec<String>,
    pub missing: Vec<String>,
}

/// `writeChildToolDiagnostic` (tool-availability.ts:18-45).
pub fn write_child_tool_diagnostic(
    file_path: &Path,
    required: &[String],
    available: &[String],
    agent: Option<&str>,
) -> Option<ChildToolDiagnostic> {
    let available_names: std::collections::BTreeSet<&str> = available
        .iter()
        .map(String::as_str)
        .chain(PI_CORE_CHILD_TOOLS.iter().copied())
        .collect();
    let missing: Vec<String> = required
        .iter()
        .filter(|name| !requirement_satisfied(name, &available_names))
        .cloned()
        .collect();
    if missing.is_empty() {
        let _ = std::fs::remove_file(file_path);
        return None;
    }
    let diagnostic = ChildToolDiagnostic {
        agent: agent.map(str::to_string),
        required: required.to_vec(),
        available: available.to_vec(),
        missing,
    };
    let payload = json!({
        "agent": diagnostic.agent,
        "required": diagnostic.required,
        "available": diagnostic.available,
        "missing": diagnostic.missing,
    });
    if let Some(parent) = file_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // 0o600 write (upstream mode option).
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let Ok(mut file) = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(file_path)
        else {
            return Some(diagnostic);
        };
        let _ = file.write_all(payload.to_string().as_bytes());
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::write(file_path, payload.to_string());
    }
    Some(diagnostic)
}

/// `formatChildToolDiagnostic` (tool-availability.ts:63-74), verbatim.
pub fn format_child_tool_diagnostic(diagnostic: &ChildToolDiagnostic) -> String {
    let subject = diagnostic
        .agent
        .as_ref()
        .map(|agent| format!("Agent '{agent}'"))
        .unwrap_or_else(|| "Subagent".to_string());
    format!(
        "{} requested unavailable child tools: {}.\n{}\n{}\n{}",
        subject,
        diagnostic.missing.join(", "),
        "The `tools` field is a strict allowlist; it does not load extension code.",
        "For extension tools, add the provider path to `subagentOnlyExtensions` (child-only), `extensions`, or as a path-like entry in `tools`, while keeping each registered tool name in `tools`.",
        "For MCP tools, verify the MCP adapter configuration and selected tool names. For builtin tools, verify the name against the installed Pi version.",
    )
}

/// Read the diagnostic a child wrote (`readChildToolDiagnosticError`,
/// tool-availability.ts:76-83): missing file → None; malformed → error.
pub fn read_child_tool_diagnostic_error(file_path: Option<&Path>) -> Option<String> {
    let file_path = file_path?;
    let content = std::fs::read_to_string(file_path).ok()?;
    let parsed: Value = serde_json::from_str(&content).ok()?;
    let as_strings = |value: &Value| -> Option<Vec<String>> {
        value
            .as_array()?
            .iter()
            .map(|item| item.as_str().map(str::to_string))
            .collect()
    };
    let required = as_strings(parsed.get("required")?)?;
    let available = as_strings(parsed.get("available")?)?;
    let missing = as_strings(parsed.get("missing")?)?;
    if required.is_empty() || available.is_empty() || missing.is_empty() {
        return None;
    }
    let agent = parsed
        .get("agent")
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(format_child_tool_diagnostic(&ChildToolDiagnostic {
        agent,
        required,
        available,
        missing,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_tools_fail_with_upstream_message() {
        let dir = std::env::temp_dir().join(format!("rpi-sub-diag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tool-diagnostic.json");
        let required = vec![
            "read".to_string(),
            "web_search".to_string(),
            "intercom".to_string(),
        ];
        let available = vec!["read".to_string(), "contact_supervisor".to_string()];
        let diagnostic =
            write_child_tool_diagnostic(&path, &required, &available, Some("researcher"));
        let diagnostic = diagnostic.expect("web_search missing");
        // `intercom` is satisfied by the registered `contact_supervisor`
        // (alias mapping, TE05 follow-up of ADR-0017) — only the genuinely
        // absent tool is reported.
        assert_eq!(diagnostic.missing, vec!["web_search".to_string()]);
        let message = format_child_tool_diagnostic(&diagnostic);
        assert!(message
            .starts_with("Agent 'researcher' requested unavailable child tools: web_search."));
        assert!(message.contains("strict allowlist"));
        // Read-back matches.
        assert_eq!(read_child_tool_diagnostic_error(Some(&path)), Some(message));
        // All-present required set clears the file.
        let all_available = vec![
            "read".to_string(),
            "web_search".to_string(),
            "intercom".to_string(),
        ];
        let everything = vec![
            "read".to_string(),
            "web_search".to_string(),
            "contact_supervisor".to_string(),
        ];
        assert!(write_child_tool_diagnostic(&path, &all_available, &everything, None).is_none());
        assert!(!path.exists(), "diagnostic removed when nothing is missing");
        assert_eq!(read_child_tool_diagnostic_error(Some(&path)), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod te18_gate_tests {
    use super::*;

    #[test]
    fn gate_passes_when_host_covers_the_allowlist() {
        let host: Vec<String> = ["read", "bash", "write", "grep"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            check_host_tool_face(
                &["read".to_string(), "bash".to_string()],
                &[],
                Ok(&host[..]),
                "scout"
            ),
            Ok(())
        );
    }

    #[test]
    fn gate_fails_closed_with_adr0017_wording() {
        let host: Vec<String> = ["read"].iter().map(|s| s.to_string()).collect();
        // Missing one / missing all — both refuse with the shared ADR-0017
        // message (same wording source as the child-side diagnostic).
        for allowlist in [
            vec!["read".to_string(), "web_search".to_string()],
            vec!["web_search".to_string(), "other".to_string()],
        ] {
            let error =
                check_host_tool_face(&allowlist, &[], Ok(&host[..]), "researcher").unwrap_err();
            assert!(
                error.starts_with("Agent 'researcher' requested unavailable child tools:"),
                "{error}"
            );
            assert!(error.contains("strict allowlist"), "{error}");
        }
        let error = check_host_tool_face(
            &["read".to_string(), "web_search".to_string()],
            &[],
            Ok(&host[..]),
            "researcher",
        )
        .unwrap_err();
        // Only the genuinely absent name is reported.
        assert!(
            error.contains("unavailable child tools: web_search.") && !error.contains("read,"),
            "{error}"
        );
    }

    #[test]
    fn gate_fails_closed_on_host_query_error() {
        // `getAllTools` errored — never silently let the launch through.
        let error = check_host_tool_face(
            &["read".to_string()],
            &[],
            Err("getAllTools host call failed"),
            "worker",
        )
        .unwrap_err();
        assert!(error.contains("host tool face is unavailable"), "{error}");
        assert!(error.contains("refusing to start"), "{error}");
    }

    #[test]
    fn gate_skips_excluded_path_shaped_and_coordination_names() {
        let host: Vec<String> = ["read"].iter().map(|s| s.to_string()).collect();
        // Excluded names are not requirements (post-exclude effective list).
        assert_eq!(
            check_host_tool_face(
                &["read".to_string(), "bash".to_string()],
                &["bash".to_string()],
                Ok(&host[..]),
                "scout"
            ),
            Ok(())
        );
        // Path-shaped entries are extension providers, not registry names.
        assert_eq!(
            check_host_tool_face(
                &["./tools/search.ts".to_string()],
                &[],
                Ok(&host[..]),
                "scout"
            ),
            Ok(())
        );
        // Coordination names are registered at runtime inside the child,
        // never present in the parent registry (child-tool-plan.ts:374-380).
        assert_eq!(
            check_host_tool_face(
                &["contact_supervisor".to_string(), "intercom".to_string()],
                &[],
                Ok(&host[..]),
                "reviewer"
            ),
            Ok(())
        );
        // No declared allowlist content → nothing to gate.
        assert_eq!(
            check_host_tool_face(&[], &[], Err("unavailable"), "worker"),
            Ok(())
        );
    }
}
