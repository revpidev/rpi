//! Port of `packages/coding-agent/src/core/radius.ts` @ pi a13d35a74
//! (v1.0.0, `ed8b3bcc1`) — the gateway constants the top-level `/login`
//! Radius entry and the MCP one-click setup use.

use rpi_ai::providers::radius_config::{DEFAULT_RADIUS_GATEWAY, normalize_radius_gateway_url};

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
