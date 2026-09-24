//! Strict-prefer JSON-schema sampling for built-in tools (`fcff255b0`,
//! upstream `test/builtin-tool-strict-mode.test.ts`).
//!
//! - Built-in `read`/`bash`/`edit`/`write` prefer strict sampling with no
//!   experimental gate (rpi never carried `PI_EXPERIMENTAL`); the strictness
//!   is a provider-side conversion, not a change to the execution schema.
//!   Upstream's list also has `powershell` — not ported ([N/A], V14-13 FR-G).
//! - Extensions re-registering a same-named tool with
//!   `constrainedSampling: false` (or a config object) override the built-in
//!   default: the `HostToolAdapter` passes the definition value through.

use std::path::PathBuf;
use std::sync::Arc;

use rpi::core::extension_host_adapter::ExtensionHostAdapter;
use rpi::core::extensions::ExtensionRunner;
use rpi::tools::{create_builtin_tools, BuiltinToolOptions, ToolContext};
use rpi_ai::types::{ConstrainedSampling, ConstrainedSamplingConfig, ConstrainedSamplingStrict};
use rpi_ext_host::host::NativeExtensionHost;
use rpi_ext_host::loader::{ExtensionFactory, InlineExtension};
use serde_json::json;

fn test_ctx() -> ToolContext {
    ToolContext {
        cwd: PathBuf::from("."),
        session_env: None,
    }
}

fn prefer() -> Option<ConstrainedSampling> {
    Some(ConstrainedSampling::Config(
        ConstrainedSamplingConfig::JsonSchema {
            strict: ConstrainedSamplingStrict::Prefer,
        },
    ))
}

/// Upstream: "prefers strict sampling with PI_EXPERIMENTAL=%s" — in rpi the
/// gate never existed, so the four gated tools are strict-prefer and the
/// optional tools (`grep`/`find`/`ls`) stay unconstrained.
#[test]
fn builtin_tools_prefer_strict_sampling() {
    let ctx = test_ctx();
    let all = ["read", "bash", "edit", "write", "grep", "find", "ls"];
    let tools = create_builtin_tools(
        &ctx,
        &all.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
        &BuiltinToolOptions::default(),
    );
    let mut by_name = std::collections::HashMap::new();
    for tool in tools {
        by_name.insert(tool.name().to_owned(), tool);
    }
    for name in ["read", "bash", "edit", "write"] {
        let tool = by_name.get(name).unwrap_or_else(|| panic!("{name} built"));
        assert_eq!(
            tool.constrained_sampling(),
            prefer(),
            "{name} must prefer strict sampling"
        );
    }
    for name in ["grep", "find", "ls"] {
        let tool = by_name.get(name).unwrap_or_else(|| panic!("{name} built"));
        assert_eq!(
            tool.constrained_sampling(),
            None,
            "{name} must stay unconstrained"
        );
    }
    // Strictness is a provider-side conversion, not a change to the
    // execution schema (upstream asserts `parameters.required` unchanged).
    let read = by_name["read"].parameters().clone();
    assert_eq!(read["required"], json!(["path"]));
    let bash = by_name["bash"].parameters().clone();
    assert_eq!(bash["required"], json!(["command"]));
}

/// Upstream: "allows extensions to re-register tools without strict
/// sampling" — the definition's `constrainedSampling` reaches the executable
/// tool (`wrapToolDefinition` passes it through untouched).
#[tokio::test]
async fn extension_registration_overrides_sampling() {
    let host = NativeExtensionHost::new("/sampling-cwd");
    let factory: ExtensionFactory = Arc::new(|api| {
        api.register_tool(rpi_ext_host::types::ToolDefinition {
            name: "read".to_owned(),
            label: "read".to_owned(),
            description: "override without strict sampling".to_owned(),
            prompt_snippet: None,
            prompt_guidelines: None,
            parameters: json!({"type": "object"}),
            constrained_sampling: Some(json!(false)),
            render_shell: None,
            prepare_arguments: None,
            execution_mode: None,
            execute: Arc::new(|_request, _ctx| {
                Box::pin(async { Ok(rpi_agent::types::AgentToolResult::default()) })
            }),
            render_call: None,
            render_result: None,
        })
        .expect("registerTool");
        api.register_tool(rpi_ext_host::types::ToolDefinition {
            name: "cfg_tool".to_owned(),
            label: "cfg".to_owned(),
            description: "explicit config".to_owned(),
            prompt_snippet: None,
            prompt_guidelines: None,
            parameters: json!({"type": "object"}),
            constrained_sampling: Some(json!({"type": "json_schema", "strict": "require"})),
            render_shell: None,
            prepare_arguments: None,
            execution_mode: None,
            execute: Arc::new(|_request, _ctx| {
                Box::pin(async { Ok(rpi_agent::types::AgentToolResult::default()) })
            }),
            render_call: None,
            render_result: None,
        })
        .expect("registerTool");
        Box::pin(async { Ok(()) })
    });
    let errors = host
        .load_inline(&[InlineExtension::Anonymous(factory)])
        .await;
    assert!(errors.is_empty(), "{errors:?}");
    let runner: Arc<dyn ExtensionRunner> = Arc::new(ExtensionHostAdapter::new(Arc::new(host)));

    let entries = runner.extension_tool_entries();
    let read = entries
        .iter()
        .find(|entry| entry.name == "read")
        .expect("read override");
    assert_eq!(
        read.tool.constrained_sampling(),
        Some(ConstrainedSampling::Disabled(false)),
        "constrainedSampling: false must override strict-prefer"
    );
    let cfg = entries
        .iter()
        .find(|entry| entry.name == "cfg_tool")
        .expect("cfg tool");
    assert_eq!(
        cfg.tool.constrained_sampling(),
        Some(ConstrainedSampling::Config(
            ConstrainedSamplingConfig::JsonSchema {
                strict: ConstrainedSamplingStrict::Require
            }
        )),
        "a config object must pass through"
    );
}
