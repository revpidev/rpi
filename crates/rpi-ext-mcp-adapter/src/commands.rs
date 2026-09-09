//! `/mcp` and `/mcp-auth` command families plus the `mcp-config` flag and
//! the enable/disable project-override writer (FR-P1-06).
//!
//! Port of `commands.ts` + `writeProjectServerDisabledOverride` (config.ts)
//! @ 3d953f90.
//!
//! The enable/disable write path writes to `<cwd>/.rpi/mcp.json` (upstream
//! `<cwd>/.pi/mcp.json`, ADR-0001 rename), preserving unknown fields via
//! read-modify-write. The TUI panel itself (`mcp-panel.ts`) is a confirmed
//! non-goal (requirements §4: pixel-level panel reproduction is out of
//! scope; §5.6: panel is human-review-only) — closed as TE-D10; commands
//! produce text output for parity.

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::error::AdapterError;
use crate::metadata::McpConfig;

/// Registered slash commands (R7.2.1.1): `install` issues one
/// `registerCommand` host call per entry and the manifest `commands`
/// capability is asserted against this list (`tests/manifest_capabilities.rs`).
pub fn command_definitions() -> [(&'static str, &'static str); 2] {
    [
        (
            "mcp",
            "Show MCP server status (status/tools/enable/disable/reconnect/logout)",
        ),
        ("mcp-auth", "Authenticate with an MCP server (OAuth)"),
    ]
}

/// Parsed `/mcp` subcommand (R7.2.1.1). Empty args and `status` both map to
/// [`McpSubcommand::Status`] (upstream `case "status": case "":`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpSubcommand {
    Status,
    Tools,
    Enable,
    Disable,
    Reconnect,
    Logout,
    Unknown(String),
}

/// Parse `/mcp` arguments: the first whitespace token selects the
/// subcommand, the trimmed remainder is the target server name.
pub fn parse_subcommand(args: &str) -> (McpSubcommand, Option<String>) {
    let trimmed = args.trim();
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or_default();
    let rest = parts.next().unwrap_or_default().trim();
    let target = (!rest.is_empty()).then(|| rest.to_string());
    let subcommand = match head {
        "" | "status" => McpSubcommand::Status,
        "tools" => McpSubcommand::Tools,
        "enable" => McpSubcommand::Enable,
        "disable" => McpSubcommand::Disable,
        "reconnect" => McpSubcommand::Reconnect,
        "logout" => McpSubcommand::Logout,
        other => McpSubcommand::Unknown(other.to_string()),
    };
    (subcommand, target)
}

/// Usage line for the `/mcp` family (unknown subcommand / missing target).
pub const MCP_USAGE: &str = "Usage: /mcp [status|tools|enable <server>|disable <server>|reconnect [server]|logout <server>]";

/// AgentToolResult-shaped text result. The host's command dispatch drops
/// the return value today, so every handler also mirrors its text through
/// `ui.notify` when a UI is bound; the result payload stays the assertable
/// ABI contract for `hasUI == false` (print/json) and for host callers.
pub fn text_result(text: impl Into<String>) -> Value {
    json!({ "content": [{ "type": "text", "text": text.into() }] })
}

/// Text result plus a structured `details` object.
pub fn text_result_with_details(text: impl Into<String>, details: Value) -> Value {
    json!({
        "content": [{ "type": "text", "text": text.into() }],
        "details": details,
    })
}

/// Error result: text + `isError` + `details.error` kind. Command handlers
/// must return this instead of panicking or hanging (R7.2.1.4).
pub fn error_result(text: impl Into<String>, kind: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": text.into() }],
        "isError": true,
        "details": { "error": kind },
    })
}

/// `enable`/`disable` confirmation text. Mirrors upstream (`index.ts:560-565`
/// @ `3d953f90`, v2.24.0): changed → path + `/reload` guidance; unchanged →
/// already-in-target-state notice.
pub fn enable_disable_message(server: &str, disabled: bool, changed: bool, path: &Path) -> String {
    if changed {
        format!(
            "{} server \"{server}\" in {} — run /reload to apply",
            if disabled { "Disabled" } else { "Enabled" },
            path.display()
        )
    } else {
        format!(
            "Server \"{server}\" is already {}",
            if disabled { "disabled" } else { "enabled" }
        )
    }
}

/// `<cwd>/.rpi/mcp.json` (upstream `.pi/mcp.json`, ADR-0001 rename).
pub fn project_pi_config_path(cwd: &Path) -> PathBuf {
    cwd.join(".rpi").join("mcp.json")
}

/// `writeProjectServerDisabledOverride` (config.ts:939-1000): read the
/// existing project override file, set/remove `disabled: true` on a single
/// server entry, write back atomically (tmp+rename). Unknown fields are
/// preserved.
///
/// Returns `(path, changed)`.
pub fn write_project_server_disabled_override(
    cwd: &Path,
    server_name: &str,
    disabled: bool,
) -> Result<(PathBuf, bool), AdapterError> {
    let file_path = project_pi_config_path(cwd);
    let raw: Value = if file_path.exists() {
        let content = std::fs::read_to_string(&file_path).map_err(|e| {
            AdapterError::InvalidConfigValue(format!(
                "Failed to read project MCP override at {}: {e}",
                file_path.display()
            ))
        })?;
        // Strip JSONC comments before parsing (same as config loading).
        let stripped = crate::config::strip_json_comments(&content);
        let parsed: Value = serde_json::from_str(&stripped).map_err(|e| {
            AdapterError::InvalidConfigValue(format!(
                "Failed to parse project MCP override at {}: {e}",
                file_path.display()
            ))
        })?;
        if !parsed.is_object() {
            return Err(AdapterError::InvalidConfigValue(format!(
                "Failed to read project MCP override at {}: root value must be an object",
                file_path.display()
            )));
        }
        parsed
    } else {
        json!({})
    };

    // Determine the server key: `mcpServers` or legacy `mcp-servers`.
    let server_key = if raw.get("mcpServers").is_some() {
        "mcpServers"
    } else if raw.get("mcp-servers").is_some() {
        "mcp-servers"
    } else {
        "mcpServers"
    };

    let mut raw = raw;
    let raw_obj = raw.as_object_mut().ok_or_else(|| {
        AdapterError::InvalidConfigValue("config root must be an object".to_string())
    })?;

    // Get or create the servers map.
    if !raw_obj.contains_key(server_key) {
        raw_obj.insert(server_key.to_string(), json!({}));
    }

    let servers_val = raw_obj.get_mut(server_key).ok_or_else(|| {
        AdapterError::InvalidConfigValue(format!("{server_key} must be an object"))
    })?;

    if !servers_val.is_object() {
        return Err(AdapterError::InvalidConfigValue(format!(
            "{server_key} must be an object"
        )));
    }

    let servers = servers_val.as_object_mut().ok_or_else(|| {
        AdapterError::InvalidConfigValue(format!("{server_key} must be an object"))
    })?;

    let existing = servers.get(server_name).cloned();
    if let Some(ref existing) = existing {
        if !existing.is_object() {
            return Err(AdapterError::InvalidConfigValue(format!(
                "server \"{server_name}\" must be an object"
            )));
        }
    }

    let existing_obj = existing
        .as_ref()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();

    // Build the next entry.
    let next: Map<String, Value> = if disabled {
        let mut next = existing_obj.clone();
        next.insert("disabled".to_string(), json!(true));
        next
    } else {
        // Remove disabled key.
        let mut next = existing_obj.clone();
        next.remove("disabled");
        next
    };

    // Check if anything changed.
    let existing_json = existing
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default())
        .unwrap_or_default();
    let next_json = serde_json::to_string(&Value::Object(next.clone())).unwrap_or_default();

    if next.is_empty() && !existing_obj.contains_key("disabled") {
        return Ok((file_path, false));
    }
    if existing_json == next_json {
        return Ok((file_path, false));
    }

    // Apply the change.
    if next.is_empty() {
        servers.remove(server_name);
    } else {
        servers.insert(server_name.to_string(), Value::Object(next));
    }

    // Write back: ensure parent dir exists, write tmp + rename (atomic).
    if let Some(parent) = file_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            AdapterError::InvalidConfigValue(format!(
                "Failed to create directory {}: {e}",
                parent.display()
            ))
        })?;
    }

    let content = serde_json::to_string_pretty(&raw).unwrap_or_else(|_| "{}".to_string());
    let tmp_path = file_path.with_extension("tmp");
    std::fs::write(&tmp_path, &content).map_err(|e| {
        AdapterError::InvalidConfigValue(format!(
            "Failed to write project MCP override at {}: {e}",
            file_path.display()
        ))
    })?;
    std::fs::rename(&tmp_path, &file_path).map_err(|e| {
        AdapterError::InvalidConfigValue(format!(
            "Failed to rename project MCP override at {}: {e}",
            file_path.display()
        ))
    })?;

    Ok((file_path, true))
}

/// Build the text output for `/mcp status` (headless / non-panel mode).
/// Mirrors `showStatus` (commands.ts:32-76) text output.
pub fn format_status_text(
    config: &McpConfig,
    manager: &crate::manager::McpServerManager,
    tool_metadata: &[(String, Vec<crate::metadata::ToolMetadata>)],
    failures: &crate::lifecycle::FailureTracker,
) -> String {
    let mut lines = vec!["MCP Server Status:".to_string(), String::new()];

    for (name, definition) in &config.mcp_servers {
        if definition.is_disabled() {
            lines.push(format!(
                "⊘ {name}: disabled (run /mcp enable {name}, then /reload)"
            ));
            continue;
        }
        let connection = manager.get_connection(name);
        let meta = tool_metadata
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, m)| m.len())
            .unwrap_or(0);
        // showStatus (commands.ts:46-59 @ 3d953f90, the v2.24.0 baseline the TE20
        // port came from): a server inside its
        // failure window is reported as failed — never as a cached catalog
        // (R7.2.3.1/#434).
        let failed_ago = failures.failure_age_seconds(name);
        let status_text;
        let icon;
        let mut failed = false;
        match connection.as_ref().map(|c| c.status()) {
            Some(crate::manager::ConnectionStatus::Connected) => {
                status_text = "connected".to_string();
                icon = "✓";
            }
            Some(crate::manager::ConnectionStatus::NeedsAuth) => {
                status_text = "needs auth".to_string();
                icon = "⚠";
            }
            _ => {
                if let Some(failed_ago) = failed_ago {
                    let reason = crate::utils::sanitize_terminal_text(
                        failures.failure_message(name).unwrap_or_default().as_str(),
                    );
                    status_text = if reason.is_empty() {
                        format!("failed {failed_ago}s ago")
                    } else {
                        format!("failed {failed_ago}s ago — {reason}")
                    };
                    icon = "✗";
                    failed = true;
                } else if meta > 0 {
                    status_text = "cached".to_string();
                    icon = "○";
                } else {
                    status_text = "not connected".to_string();
                    icon = "○";
                }
            }
        }
        let suffix = if failed {
            String::new()
        } else if status_text == "cached" {
            format!(" ({meta} tools, cached)")
        } else if status_text == "connected" {
            format!(" ({meta} tools)")
        } else {
            String::new()
        };
        lines.push(format!("{icon} {name}: {status_text}{suffix}"));
    }

    if config.mcp_servers.is_empty() {
        lines.push("No MCP servers configured".to_string());
    }

    lines.join("\n")
}

/// Build the text output for `/mcp tools`.
pub fn format_tools_text(
    config: &McpConfig,
    tool_metadata: &[(String, Vec<crate::metadata::ToolMetadata>)],
    unavailable_servers: &[String],
) -> String {
    let all_tools: Vec<&str> = tool_metadata
        .iter()
        .filter(|(name, _)| !config.is_server_disabled(name))
        // showTools (commands.ts:158 @ 10a45367, #434): servers in active
        // failure backoff are not advertised.
        .filter(|(name, _)| !unavailable_servers.iter().any(|server| server == name))
        .flat_map(|(_, tools)| tools.iter().map(|t| t.name.as_str()))
        .collect();

    if all_tools.is_empty() {
        return "No MCP tools available".to_string();
    }

    let mut lines = vec!["MCP Tools:".to_string(), String::new()];
    for tool in &all_tools {
        lines.push(format!("  {tool}"));
    }
    lines.push(String::new());
    lines.push(format!("Total: {} tools", all_tools.len()));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::McpServerManager;
    use serde_json::json;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rpi-mcp-cmd-test-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn disable_server_writes_disabled_true() {
        let dir = temp_dir("disable");
        // Pre-write a .rpi/mcp.json with an existing server.
        let config_path = project_pi_config_path(&dir);
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            json!({
                "mcpServers": {
                    "demo": { "command": "node" }
                }
            })
            .to_string(),
        )
        .unwrap();

        let (path, changed) = write_project_server_disabled_override(&dir, "demo", true).unwrap();
        assert!(changed);
        assert_eq!(path, config_path);

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(written["mcpServers"]["demo"]["disabled"], json!(true));
        assert_eq!(written["mcpServers"]["demo"]["command"], json!("node"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enable_server_removes_disabled() {
        let dir = temp_dir("enable");
        let config_path = project_pi_config_path(&dir);
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            json!({
                "mcpServers": {
                    "demo": { "command": "node", "disabled": true }
                }
            })
            .to_string(),
        )
        .unwrap();

        let (_path, changed) = write_project_server_disabled_override(&dir, "demo", false).unwrap();
        assert!(changed);

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert!(written["mcpServers"]["demo"].get("disabled").is_none());
        assert_eq!(written["mcpServers"]["demo"]["command"], json!("node"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_fields_preserved() {
        let dir = temp_dir("preserve");
        let config_path = project_pi_config_path(&dir);
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            json!({
                "mcpServers": {
                    "demo": { "command": "node" }
                },
                "settings": { "toolPrefix": "mcp" },
                "customField": 42
            })
            .to_string(),
        )
        .unwrap();

        write_project_server_disabled_override(&dir, "demo", true).unwrap();

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(written["settings"]["toolPrefix"], json!("mcp"));
        assert_eq!(written["customField"], json!(42));
        assert_eq!(written["mcpServers"]["demo"]["disabled"], json!(true));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_change_when_already_in_target_state() {
        let dir = temp_dir("nochange");
        let config_path = project_pi_config_path(&dir);
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            json!({
                "mcpServers": {
                    "demo": { "command": "node", "disabled": true }
                }
            })
            .to_string(),
        )
        .unwrap();

        let (_path, changed) = write_project_server_disabled_override(&dir, "demo", true).unwrap();
        assert!(!changed, "should be no-op when already disabled");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disable_creates_file_when_not_exists() {
        let dir = temp_dir("create");
        let config_path = project_pi_config_path(&dir);

        let (_path, changed) =
            write_project_server_disabled_override(&dir, "new-server", true).unwrap();
        assert!(changed);
        assert!(config_path.exists());

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(written["mcpServers"]["new-server"]["disabled"], json!(true));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn format_status_text_empty_config() {
        let config = McpConfig::default();
        let manager = McpServerManager::new(None);
        let failures = crate::lifecycle::FailureTracker::new();
        let text = format_status_text(&config, &manager, &[], &failures);
        assert!(text.contains("No MCP servers configured"));
    }

    #[test]
    fn format_tools_text_empty() {
        let config = McpConfig::default();
        let text = format_tools_text(&config, &[], &[]);
        assert_eq!(text, "No MCP tools available");
    }

    /// #434 / R7.2.3.1（`commands.ts:158 @ 10a45367`）：`/mcp tools`
    /// 不宣传退避中的 server；`/mcp status` 标 failed 并带原因。
    #[test]
    fn command_surfaces_hide_backoff_servers() {
        use crate::lifecycle::FailureTracker;
        use crate::metadata::{ServerEntry, ToolMetadata};
        use indexmap::IndexMap;

        fn entry(value: Value) -> ServerEntry {
            ServerEntry(value.as_object().cloned().unwrap_or_default())
        }

        let mut mcp_servers = IndexMap::new();
        mcp_servers.insert("demo".to_string(), entry(json!({ "command": "node" })));
        let config = McpConfig {
            mcp_servers,
            ..Default::default()
        };
        let metadata = vec![(
            "demo".to_string(),
            vec![ToolMetadata {
                name: "demo_echo".to_string(),
                original_name: "echo".to_string(),
                ..Default::default()
            }],
        )];
        assert!(format_tools_text(&config, &metadata, &[]).contains("demo_echo"));
        let hidden = format_tools_text(&config, &metadata, &["demo".to_string()]);
        assert!(!hidden.contains("demo_echo"), "text: {hidden}");

        let manager = McpServerManager::new(None);
        let failures = FailureTracker::new();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        failures.record_failure_at("demo", now.saturating_sub(1_000), "boom");
        let text = format_status_text(&config, &manager, &metadata, &failures);
        assert!(text.contains("✗ demo: failed"), "text: {text}");
        assert!(text.contains("— boom"), "text: {text}");
        assert!(!text.contains("cached"), "text: {text}");
    }

    /// A1 baseline: the integration dispatch test
    /// (`tests/command_wiring.rs`) asserts the same literal, tying the
    /// wired `/mcp status` output to this pure-function baseline.
    #[test]
    fn format_status_text_known_baseline() {
        use crate::metadata::ServerEntry;
        use indexmap::IndexMap;

        fn entry(value: Value) -> ServerEntry {
            ServerEntry(value.as_object().cloned().unwrap_or_default())
        }

        let mut mcp_servers = IndexMap::new();
        mcp_servers.insert(
            "demo".to_string(),
            entry(json!({"command": "node", "lifecycle": "lazy"})),
        );
        mcp_servers.insert(
            "off".to_string(),
            entry(json!({"command": "node", "disabled": true})),
        );
        mcp_servers.insert(
            "oauth-demo".to_string(),
            entry(json!({"url": "http://127.0.0.1:9/mcp", "auth": "oauth", "lifecycle": "lazy"})),
        );
        let config = McpConfig {
            mcp_servers,
            imports: None,
            settings: None,
        };
        let manager = McpServerManager::new(None);
        let failures = crate::lifecycle::FailureTracker::new();
        assert_eq!(
            format_status_text(&config, &manager, &[], &failures),
            "MCP Server Status:\n\n○ demo: not connected\n⊘ off: disabled (run /mcp enable off, then /reload)\n○ oauth-demo: not connected"
        );
    }

    #[test]
    fn command_definitions_cover_mcp_and_mcp_auth() {
        let names: Vec<&str> = command_definitions()
            .iter()
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(names, ["mcp", "mcp-auth"]);
        for (name, description) in command_definitions() {
            assert!(!name.is_empty());
            assert!(!description.is_empty());
        }
    }

    #[test]
    fn parse_subcommand_maps_known_forms() {
        assert_eq!(parse_subcommand(""), (McpSubcommand::Status, None));
        assert_eq!(parse_subcommand("  "), (McpSubcommand::Status, None));
        assert_eq!(parse_subcommand("status"), (McpSubcommand::Status, None));
        assert_eq!(parse_subcommand("tools"), (McpSubcommand::Tools, None));
        assert_eq!(
            parse_subcommand("disable demo"),
            (McpSubcommand::Disable, Some("demo".to_string()))
        );
        assert_eq!(
            parse_subcommand("enable  demo  "),
            (McpSubcommand::Enable, Some("demo".to_string()))
        );
        assert_eq!(
            parse_subcommand("reconnect"),
            (McpSubcommand::Reconnect, None)
        );
        assert_eq!(
            parse_subcommand("reconnect demo"),
            (McpSubcommand::Reconnect, Some("demo".to_string()))
        );
        assert_eq!(
            parse_subcommand("logout demo"),
            (McpSubcommand::Logout, Some("demo".to_string()))
        );
        assert_eq!(
            parse_subcommand("setup"),
            (McpSubcommand::Unknown("setup".to_string()), None)
        );
        // Server names may contain spaces; the remainder is one target.
        assert_eq!(
            parse_subcommand("logout my server"),
            (McpSubcommand::Logout, Some("my server".to_string()))
        );
    }

    #[test]
    fn enable_disable_message_matches_upstream_shape() {
        let path = Path::new("/repo/.rpi/mcp.json");
        assert_eq!(
            enable_disable_message("demo", true, true, path),
            "Disabled server \"demo\" in /repo/.rpi/mcp.json — run /reload to apply"
        );
        assert_eq!(
            enable_disable_message("demo", false, true, path),
            "Enabled server \"demo\" in /repo/.rpi/mcp.json — run /reload to apply"
        );
        assert_eq!(
            enable_disable_message("demo", true, false, path),
            "Server \"demo\" is already disabled"
        );
        assert_eq!(
            enable_disable_message("demo", false, false, path),
            "Server \"demo\" is already enabled"
        );
    }

    #[test]
    fn result_builders_shape() {
        assert_eq!(
            text_result("hello"),
            json!({"content": [{"type": "text", "text": "hello"}]})
        );
        let error = error_result("boom", "invalid_args");
        assert_eq!(error["isError"], json!(true));
        assert_eq!(error["details"]["error"], json!("invalid_args"));
        assert_eq!(error["content"][0]["text"], json!("boom"));
        let detailed = text_result_with_details("ok", json!({"changed": true}));
        assert_eq!(detailed["details"]["changed"], json!(true));
        assert!(detailed.get("isError").is_none());
    }
}
