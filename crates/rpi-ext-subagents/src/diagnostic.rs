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
