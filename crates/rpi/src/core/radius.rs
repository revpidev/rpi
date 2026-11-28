//! Port of `packages/coding-agent/src/core/radius.ts` @ pi b4afdddf
//! (v1.0.0, `ed8b3bcc1`) — the gateway constants the top-level `/login`
//! Radius entry and the MCP one-click setup use.

use std::path::{Path, PathBuf};

use rpi_ai::providers::radius_config::{DEFAULT_RADIUS_GATEWAY, normalize_radius_gateway_url};

use crate::extensions::mcp::config::{
    McpAuthConfig, McpHttpServerConfig, McpServerCommon, McpServerConfig, load_mcp_config,
};

/// `RADIUS_PROVIDER_ID`.
pub const RADIUS_PROVIDER_ID: &str = "radius";

/// `RADIUS_MCP_URL` — MCP endpoint of the gateway the built-in Radius
/// provider signs in to. Upstream computes it from `DEFAULT_RADIUS_GATEWAY`
/// (the `PI_RADIUS_GATEWAY` override only feeds `getRadiusGatewayUrl`).
pub fn radius_mcp_url() -> String {
    format!(
        "{}/mcp",
        normalize_radius_gateway_url(DEFAULT_RADIUS_GATEWAY)
    )
}

pub struct RadiusMcpServerPlan {
    pub mcp_path: PathBuf,
    pub name: String,
    pub config: McpServerConfig,
}

/// `planRadiusMcpServer` (core/radius-mcp.ts @ b4afdddf): select the global
/// entry that should use this provider login, or return `None` when it is
/// already configured.
pub fn plan_radius_mcp_server(
    provider_id: &str,
    agent_dir: &Path,
    cwd: &Path,
) -> Option<RadiusMcpServerPlan> {
    fn normalize_url(url: &str) -> &str {
        url.trim_end_matches('/')
    }
    let expected_url = radius_mcp_url();
    let loaded = load_mcp_config(agent_dir, cwd, false);
    let existing = loaded.servers.iter().find(|entry| match &entry.config {
        McpServerConfig::Http(http) => normalize_url(&http.url) == normalize_url(&expected_url),
        McpServerConfig::Stdio(_) => false,
    });
    if let Some(entry) = existing
        && let McpServerConfig::Http(http) = &entry.config
        && http
            .auth
            .as_ref()
            .is_some_and(|auth| auth.provider == provider_id)
    {
        return None;
    }

    let mut name = existing
        .map(|entry| entry.name.clone())
        .unwrap_or_else(|| "radius".to_owned());
    if existing.is_none() {
        let namespaces = loaded
            .servers
            .iter()
            .map(|entry| rpi_mcp::mcp_namespace(&entry.name))
            .collect::<std::collections::HashSet<_>>();
        let mut suffix = 1;
        while namespaces.contains(&rpi_mcp::mcp_namespace(&name)) {
            name = if suffix == 1 {
                "radius-mcp".to_owned()
            } else {
                format!("radius-mcp-{suffix}")
            };
            suffix += 1;
        }
    }

    let mut config = existing
        .map(|entry| entry.config.clone())
        .unwrap_or_else(|| {
            McpServerConfig::Http(McpHttpServerConfig {
                common: McpServerCommon::default(),
                kind: None,
                url: expected_url,
                headers: None,
                oauth: None,
                auth: None,
                extra: serde_json::Map::new(),
            })
        });
    if let McpServerConfig::Http(http) = &mut config {
        http.auth = Some(McpAuthConfig {
            provider: provider_id.to_owned(),
        });
        http.oauth = None;
    }
    Some(RadiusMcpServerPlan {
        mcp_path: agent_dir.join("mcp.json"),
        name,
        config,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `RADIUS_MCP_URL` hangs off the normalized default gateway.
    #[test]
    fn radius_mcp_url_uses_the_default_gateway() {
        assert_eq!(radius_mcp_url(), "https://radius.pi.dev/mcp");
        assert_eq!(RADIUS_PROVIDER_ID, "radius");
    }
}
