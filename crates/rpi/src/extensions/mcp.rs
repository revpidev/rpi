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

use crate::core::model_runtime::ModelRuntime;

pub use config::{
    LoadedMcpConfig, McpExposure, McpScope, McpServerConfig, McpServerConfigPatch, McpServerEntry,
    load_mcp_config, mcp_namespace,
};

/// The built-in hidden `mcp` extension. `replaceable`/`builtin` naming and
/// the `-builtin:mcp` disable surface are V16-13's governance scope; this
/// factory is the implementation it wires. `model_runtime` resolves
/// `auth.provider` tokens (`/login` credentials).
pub fn inline_extension(model_runtime: Arc<ModelRuntime>) -> InlineExtension {
    InlineExtension::Named {
        name: "mcp".to_owned(),
        hidden: true,
        // `extensions/index.ts:13`: replaceable builtin.
        replaceable: true,
        builtin: true,
        factory: Arc::new(move |api| {
            let model_runtime = model_runtime.clone();
            Box::pin(async move { ui::create_mcp_extension(api, model_runtime) })
        }),
    }
}
