//! Host-integration smoke (TE28 G12): the real cdylib through the actual
//! `NativeExtensionHost` — manifest capabilities, `ask_user_question`
//! registration surface, and the Q0 `execute` envelopes over the real ABI
//! (`no_ui` without a UI bridge, `no_custom_ui` with one).
//!
//! One `#[tokio::test]` per binary: the env overrides below (HOME /
//! XDG_CONFIG_HOME / agent dir) are process-global.

mod common;

use std::path::{Path, PathBuf};

use common::RecordingBridge;
use rpi_ext_host::host::NativeExtensionHost;

fn plugin_path() -> PathBuf {
    let target = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("target")
        });
    let name = if cfg!(target_os = "macos") {
        "librpi_ext_ask_user_question.dylib"
    } else if cfg!(target_os = "windows") {
        "rpi_ext_ask_user_question.dll"
    } else {
        "librpi_ext_ask_user_question.so"
    };
    target.join("debug").join(name)
}

fn require_plugin() -> Option<PathBuf> {
    let plugin = plugin_path();
    if plugin.is_file() {
        return Some(plugin);
    }
    eprintln!(
        "skipping: cdylib missing at {} — build with `cargo build -p rpi-ext-ask-user-question`",
        plugin.display()
    );
    None
}

/// Package the cdylib with a manifest (the crate's real one, optionally with
/// rewritten capabilities) into a fresh temp dir.
fn package(tag: &str, capabilities: Option<&str>, plugin: &Path) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rpi-askq-l0-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    let plugin_dir = dir.join("plug");
    std::fs::create_dir_all(&plugin_dir).expect("plugin dir");
    let plugin_name = plugin.file_name().expect("plugin file name");
    std::fs::copy(plugin, plugin_dir.join(plugin_name)).expect("copy cdylib");
    let manifest =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("rpi-extension.json"))
            .expect("crate manifest");
    let manifest = match capabilities {
        Some(capabilities) => {
            let mut value: serde_json::Value =
                serde_json::from_str(&manifest).expect("manifest json");
            value["capabilities"] = serde_json::from_str(capabilities).expect("capabilities json");
            serde_json::to_string_pretty(&value).expect("manifest reserialize")
        }
        None => manifest,
    };
    std::fs::write(plugin_dir.join("rpi-extension.json"), manifest).expect("write manifest");
    plugin_dir
}

fn text_of(result: &rpi_agent::types::AgentToolResult) -> String {
    common::result_text(result)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l0_load_registers_ask_user_question_and_q0_envelopes() {
    let Some(plugin) = require_plugin() else {
        return;
    };
    // Deterministic config: empty HOME + no XDG override -> defaults.
    let sandbox = std::env::temp_dir().join(format!("rpi-askq-l0-sandbox-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sandbox);
    std::fs::create_dir_all(sandbox.join("proj/.rpi")).unwrap();
    std::fs::create_dir_all(sandbox.join("home")).unwrap();
    std::fs::create_dir_all(sandbox.join("agent")).unwrap();
    // Safety: single test in this binary (see file header).
    unsafe {
        std::env::set_var("HOME", sandbox.join("home"));
        std::env::set_var("USERPROFILE", sandbox.join("home"));
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::set_var("RPI_CODING_AGENT_DIR", sandbox.join("agent"));
    }

    // 1. Without `tools` the registerTool host call is denied -> load error.
    let denied = package("denied", Some(r#"["session","events","ui"]"#), &plugin);
    let host = std::sync::Arc::new(NativeExtensionHost::new(
        sandbox.join("proj").to_string_lossy().as_ref(),
    ));
    let errors = host.load_paths(std::slice::from_ref(&denied)).await;
    assert!(
        errors
            .iter()
            .any(|error| error.error.contains("capabilityDenied")),
        "expected capabilityDenied, got {errors:?}"
    );
    let _ = std::fs::remove_dir_all(denied.parent().unwrap());

    // 2. The real manifest loads and registers the tool with the frozen schema.
    let full = package("full", None, &plugin);
    let host = std::sync::Arc::new(NativeExtensionHost::new(
        sandbox.join("proj").to_string_lossy().as_ref(),
    ));
    let errors = host.load_paths(std::slice::from_ref(&full)).await;
    assert!(errors.is_empty(), "{errors:?}");
    let definition = host
        .get_tool_definition("ask_user_question")
        .expect("ask_user_question registered");
    assert_eq!(definition.label, "Ask User Question");
    assert!(
        definition
            .description
            .starts_with("Ask the user one or more structured questions"),
        "{}",
        definition.description
    );
    assert_eq!(definition.parameters["type"], "object");
    assert_eq!(
        definition.parameters["properties"]["questions"]["minItems"],
        1
    );
    assert_eq!(
        definition.parameters["properties"]["questions"]["maxItems"],
        4
    );
    assert_eq!(
        definition.parameters["properties"]["questions"]["items"]["properties"]["options"]
            ["minItems"],
        2
    );
    assert_eq!(
        definition.parameters["properties"]["questions"]["items"]["properties"]["options"]
            ["maxItems"],
        4
    );

    let params = serde_json::json!({
        "questions": [{
            "question": "Pick one?",
            "header": "Pick",
            "options": [
                {"label": "A", "description": "a"},
                {"label": "B", "description": "b"}
            ]
        }]
    });

    // 3. No UI bridge -> hasUI=false -> the no_ui backstop envelope.
    let result = (definition.execute)(
        rpi_ext_host::types::ToolExecuteRequest {
            tool_call_id: "l0-no-ui".to_owned(),
            params: params.clone(),
            signal: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        },
        host.core().create_context(),
    )
    .await
    .expect("execute resolves");
    assert_eq!(
        text_of(&result),
        "Error: UI not available (running in non-interactive mode)"
    );
    assert_eq!(result.details["error"], "no_ui");
    assert_eq!(result.details["cancelled"], true);
    assert_eq!(result.details["answers"], serde_json::json!([]));

    // 4. With a UI bridge -> hasUI=true -> prompt event + Q0 placeholder.
    host.set_ui(
        Some(RecordingBridge::new(vec![])),
        rpi_ext_host::types::ExtensionMode::Tui,
    );
    let result = (definition.execute)(
        rpi_ext_host::types::ToolExecuteRequest {
            tool_call_id: "l0-ui".to_owned(),
            params,
            signal: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        },
        host.core().create_context(),
    )
    .await
    .expect("execute resolves");
    assert!(
        text_of(&result).starts_with("Error: this client cannot render the questionnaire"),
        "{}",
        text_of(&result)
    );
    assert_eq!(result.details["error"], "no_custom_ui");
    assert_eq!(result.details["cancelled"], true);

    let _ = std::fs::remove_dir_all(full.parent().unwrap());
}
