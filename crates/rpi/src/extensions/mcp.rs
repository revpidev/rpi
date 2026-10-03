//! The built-in `mcp` extension: `mcp.json` configuration, server
//! connections, tools/resources, `/mcp` and `rpi mcp` (port of
//! `packages/coding-agent/src/extensions/mcp/` @ a13d35a74).

pub mod cli;
pub mod config;
pub mod log;
pub mod oauth;
pub mod resources;
pub mod runtime;
pub mod tools;
pub mod ui;

use std::sync::Arc;

use rpi_ext_host::loader::InlineExtension;

pub use config::{
    LoadedMcpConfig, McpExposure, McpScope, McpServerConfig, McpServerConfigPatch, McpServerEntry,
    McpServerRegistry, RegisteredMcpServer, load_mcp_config, mcp_namespace,
};

/// The built-in hidden `mcp` extension. `replaceable`/`builtin` naming and
/// the `-builtin:mcp` disable surface are V16-13's governance scope; this
/// factory is the implementation it wires.
pub fn inline_extension() -> InlineExtension {
    InlineExtension::Named {
        name: "mcp".to_owned(),
        hidden: true,
        factory: Arc::new(|api| Box::pin(async move { ui::create_mcp_extension(api) })),
    }
}
