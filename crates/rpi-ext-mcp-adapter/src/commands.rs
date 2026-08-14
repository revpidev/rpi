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
        let status_text;
        let icon;
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
                if meta > 0 {
                    status_text = "cached".to_string();
                    icon = "○";
                } else {
                    status_text = "not connected".to_string();
                    icon = "○";
                }
            }
        }
        let suffix = if status_text == "cached" {
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
) -> String {
    let all_tools: Vec<&str> = tool_metadata
        .iter()
        .filter(|(name, _)| !config.is_server_disabled(name))
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
        let text = format_status_text(&config, &manager, &[]);
        assert!(text.contains("No MCP servers configured"));
    }

    #[test]
    fn format_tools_text_empty() {
        let config = McpConfig::default();
        let text = format_tools_text(&config, &[]);
        assert_eq!(text, "No MCP tools available");
    }
}
