//! Host-integration smoke (TE28/TE29 G12): the real cdylib through the
//! actual `NativeExtensionHost` — manifest capabilities, `ask_user_question`
//! registration surface, and the `execute` envelopes over the real ABI
//! (`no_ui` without a UI bridge; the TE29 dialog walker with one — RPC mode
//! answered, TUI-mode fallback cancelled).
//!
//! G2 (TE29): the Q0 step "with a UI bridge → `no_custom_ui` placeholder"
//! changed — the walker now owns every post-`prompt` branch (upstream
//! `runRpcPath` / `resolveUndefinedResult`; the placeholder arm was the
//! documented pre-TE29 state, task file §1/§2).
//!
//! One `#[tokio::test]` per binary: the env overrides below (HOME /
//! XDG_CONFIG_HOME / agent dir) are process-global.

mod common;

use std::path::{Path, PathBuf};

use common::{ComponentBridge, RecordingBridge};
use rpi_ext_host::host::NativeExtensionHost;
use rpi_ext_host::interactive_ui::ComponentEvent;

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

/// Removes the sandbox on drop (including assert panics) so repeated
/// `cargo test --workspace` runs do not accumulate `/tmp` dirs.
struct Sandbox(PathBuf);

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l0_load_registers_ask_user_question_and_envelopes() {
    let Some(plugin) = require_plugin() else {
        return;
    };
    // Deterministic config: empty HOME + no XDG override -> defaults.
    let sandbox_guard =
        Sandbox(std::env::temp_dir().join(format!("rpi-askq-l0-sandbox-{}", std::process::id())));
    let sandbox = sandbox_guard.0.as_path();
    let _ = std::fs::remove_dir_all(sandbox);
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

    // 4. TUI-mode bridge without the interactive-UI ABI (the RecordingBridge
    //    keeps the trait default `supportsInteractiveUi = false`) →
    //    `ui.mountComponent` answers unknownMethod → the walker fallback; a
    //    cancelled select dismisses the whole questionnaire → decline
    //    envelope.
    let tui_bridge = RecordingBridge::new(vec![None]);
    host.set_ui(
        Some(tui_bridge.clone()),
        rpi_ext_host::types::ExtensionMode::Tui,
    );
    let result = (definition.execute)(
        rpi_ext_host::types::ToolExecuteRequest {
            tool_call_id: "l0-tui-fallback".to_owned(),
            params: params.clone(),
            signal: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        },
        host.core().create_context(),
    )
    .await
    .expect("execute resolves");
    assert_eq!(text_of(&result), "User declined to answer questions");
    assert_eq!(result.details["cancelled"], true);
    assert_eq!(result.details["error"], serde_json::Value::Null);
    assert_eq!(tui_bridge.selects().len(), 1, "one select per question");

    // 5. RPC-mode bridge — `ctx.mode == "rpc"` routes to the walker up
    //    front; the answered select reaches the envelope through the full
    //    host-call chain (guard → prompt event → mode probe → blocked pair
    //    → ui.select).
    let rpc_bridge = RecordingBridge::new(vec![Some("1. A — a")]);
    host.set_ui(
        Some(rpc_bridge.clone()),
        rpi_ext_host::types::ExtensionMode::Rpc,
    );
    let result = (definition.execute)(
        rpi_ext_host::types::ToolExecuteRequest {
            tool_call_id: "l0-rpc-walker".to_owned(),
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
        "User has answered your questions: \"Pick one?\"=\"A\". You can now continue with the user's answers in mind."
    );
    assert_eq!(result.details["cancelled"], false);
    assert_eq!(result.details["answers"][0]["kind"], "option");
    let titles = rpc_bridge.selects();
    assert_eq!(titles.len(), 1);
    assert!(titles[0].starts_with("[Pick] Pick one?"), "{}", titles[0]);

    // 6. Two-question RPC walk — consecutive dialogs of BOTH kinds through
    //    the real chain (§8.1: no deadlock; one dialog per question).
    let two_questions = serde_json::json!({
        "questions": [
            params["questions"][0].clone(),
            {
                "question": "Pick a color?",
                "header": "Color",
                "multiSelect": true,
                "options": [
                    {"label": "red", "description": "r"},
                    {"label": "green", "description": "g"}
                ]
            }
        ]
    });
    let walk_bridge = RecordingBridge::scripted(vec![Some("1. A — a")], vec![Some("1")]);
    host.set_ui(
        Some(walk_bridge.clone()),
        rpi_ext_host::types::ExtensionMode::Rpc,
    );
    let result = (definition.execute)(
        rpi_ext_host::types::ToolExecuteRequest {
            tool_call_id: "l0-rpc-walk".to_owned(),
            params: two_questions,
            signal: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        },
        host.core().create_context(),
    )
    .await
    .expect("execute resolves");
    assert_eq!(
        text_of(&result),
        "User has answered your questions: \"Pick one?\"=\"A\". \"Pick a color?\"=\"red\". You can now continue with the user's answers in mind."
    );
    assert_eq!(walk_bridge.selects().len(), 1);
    assert_eq!(walk_bridge.inputs().len(), 1);
    let (input_title, placeholder) = &walk_bridge.inputs()[0];
    assert!(
        input_title.starts_with("[Color] Pick a color?"),
        "{input_title}"
    );
    assert!(input_title.contains("1. red — r"));
    assert_eq!(placeholder.as_deref(), Some("1,3"));

    // 7. TE30 TUI component path: the mount/poll/render loop runs through
    //    the real host-call chain (mountComponent -> pollComponent ->
    //    renderComponent), the scripted Enter submits the first option, and
    //    the envelope carries it. No walker dialog is touched.
    let component_bridge = ComponentBridge::new(vec![
        ComponentEvent::Resize {
            width: 80,
            height: 24,
        },
        ComponentEvent::Input {
            data: "\r".to_owned(),
        },
    ]);
    host.set_ui(
        Some(component_bridge.clone()),
        rpi_ext_host::types::ExtensionMode::Tui,
    );
    let result = (definition.execute)(
        rpi_ext_host::types::ToolExecuteRequest {
            tool_call_id: "l0-component".to_owned(),
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
        "User has answered your questions: \"Pick one?\"=\"A\". You can now continue with the user's answers in mind."
    );
    assert_eq!(result.details["cancelled"], false);
    assert_eq!(result.details["answers"][0]["kind"], "option");
    let mounts = component_bridge.mounts();
    assert_eq!(mounts.len(), 1, "one mounted component per tool call");
    assert!(mounts[0].overlay);
    assert_eq!(mounts[0].tick_ms, 0, "the dialog subscribes to no ticks");
    assert_eq!(mounts[0].keys_when_hidden, vec!["ctrl+]".to_owned()]);
    assert_eq!(mounts[0].label.as_deref(), Some("ask_user_question"));
    let frames = component_bridge.frames();
    assert!(frames.len() >= 2, "initial frame + done frame");
    assert!(
        frames[0]
            .lines
            .iter()
            .any(|line| line.contains("Pick one?")),
        "{:?}",
        frames[0].lines
    );
    let last = frames.last().expect("frames");
    assert!(last.is_done(), "the final render carries done");
    assert_eq!(
        last.done
            .value()
            .and_then(|value| value.get("answers"))
            .and_then(|answers| answers.get(0))
            .and_then(|answer| answer.get("answer")),
        Some(&serde_json::json!("A"))
    );

    let _ = std::fs::remove_dir_all(full.parent().unwrap());
}
