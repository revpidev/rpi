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

    // 8. TE41 (rpi#52) render round-trip through the real ABI: the
    //    registration's render flags install the closures; `renderCall`
    //    answers a component tree (collapsed summary / expanded detail /
    //    streaming tolerance) and `renderResult` answers the three result
    //    states. The tree text must never contain the pretty-printed args
    //    JSON — reverting the capability flags must fail this assertion
    //    (the hooks would be absent and the host would fall back to the
    //    generic dump).
    let render_call = definition
        .render_call
        .clone()
        .expect("renderCall closure installed");
    let render_result = definition
        .render_result
        .clone()
        .expect("renderResult closure installed");
    let strip_ansi = |line: &str| {
        line.replace('\u{1b}', "")
            .replace("[1m", "")
            .replace("[22m", "")
            .replace("[39m", "")
    };
    let render_args = serde_json::json!({
        "questions": [
            {
                "question": "Which storage backend should the CLI default to?",
                "header": "Scope",
                "options": [
                    {"label": "Local SQLite", "description": "zero setup, single machine"},
                    {"label": "Postgres", "description": "networked, concurrent access"}
                ]
            },
            {
                "question": "How urgent is the migration?",
                "header": "Priority",
                "options": [
                    {"label": "Now", "description": "this sprint"}
                ]
            }
        ]
    });
    let call_context =
        |args_complete: bool, expanded: bool| rpi_ext_host::types::ToolRenderContext {
            args: render_args.clone(),
            tool_call_id: "l0-render".to_owned(),
            cwd: "/tmp".to_owned(),
            execution_started: true,
            args_complete,
            is_partial: !args_complete,
            expanded,
            show_images: false,
            is_error: false,
            terminal_width: Some(100),
        };

    // Collapsed summary (the issue #52 §1 example shape).
    let tree = render_call(call_context(true, false)).expect("collapsed tree");
    let serialized = serde_json::to_string(&tree).unwrap_or_default();
    assert_eq!(
        strip_ansi(tree["props"]["text"].as_str().unwrap_or("")),
        "ask_user_question 2 questions (Scope, Priority)"
    );
    assert_eq!(tree["props"]["truncate"], serde_json::json!(true));
    // The rpi#52 regression floor: no pretty-printed args anywhere in the
    // tree (the dump would carry the raw question/description strings).
    assert!(!serialized.contains("Which storage backend"));
    assert!(!serialized.contains("zero setup, single machine"));

    // Expanded detail (Ctrl+O / app.tools.expand).
    let tree = render_call(call_context(true, true)).expect("expanded tree");
    assert_eq!(tree["type"], "column");
    let children = tree["children"].as_array().expect("children");
    assert_eq!(
        strip_ansi(children[2]["props"]["text"].as_str().unwrap_or("")),
        "  1. Scope — Which storage backend should the CLI default to?"
    );
    assert_eq!(
        strip_ansi(children[3]["props"]["text"].as_str().unwrap_or("")),
        "       1. Local SQLite — zero setup, single machine"
    );

    // Streaming tolerance: a truncated array counts what is there.
    let mut streaming = call_context(false, false);
    streaming.args["questions"]
        .as_array_mut()
        .unwrap()
        .truncate(1);
    let tree = render_call(streaming).expect("streaming tree");
    assert_eq!(
        strip_ansi(tree["props"]["text"].as_str().unwrap_or("")),
        "ask_user_question 1 question (Scope, …)"
    );

    // renderResult over a real answered envelope (from step 7's execute).
    let answered = serde_json::json!({
        "content": serde_json::to_value(&result.content).unwrap_or_default(),
        "details": result.details.clone()
    });
    let answered: rpi_agent::types::AgentToolResult =
        serde_json::from_value(answered).expect("agent result");
    let mut result_context = call_context(true, false);
    result_context.is_error = false;
    let tree = render_result(
        answered.clone(),
        rpi_ext_host::types::ToolRenderResultOptions {
            expanded: false,
            is_partial: false,
        },
        result_context.clone(),
    )
    .expect("result tree");
    assert_eq!(
        strip_ansi(tree["props"]["text"].as_str().unwrap_or("")),
        "✓ 1 answered"
    );
    // Expanded result carries the per-answer detail line.
    let tree = render_result(
        answered,
        rpi_ext_host::types::ToolRenderResultOptions {
            expanded: true,
            is_partial: false,
        },
        result_context,
    )
    .expect("expanded result tree");
    assert_eq!(
        strip_ansi(tree["children"][1]["props"]["text"].as_str().unwrap_or("")),
        "✓ Pick one? = A"
    );

    let _ = std::fs::remove_dir_all(full.parent().unwrap());
}
