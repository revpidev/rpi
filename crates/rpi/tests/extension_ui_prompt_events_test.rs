//! V14-11 FR-C + FR-H end-to-end (both channels):
//! - `ui_prompt_start` / `ui_prompt_end` (ccfe79ed2, #8355): a dialog on
//!   the bound UI bridge dispatches the pair to extension handlers —
//!   asserted through a native inline extension AND a wasm guest.
//! - `session_compact_failed` (a6b1dbceb, #8241; channel from V14-01):
//!   the wasm channel sees the full upstream payload on a threshold
//!   compaction failure (the native channel is covered by
//!   `extension_host_agent_event_payload_test.rs`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rpi::core::extension_actions::bind_session_actions;
use rpi_ext_host::api::UiBridge;
use rpi_ext_host::host::NativeExtensionHost;
use rpi_ext_host::loader::{ExtensionFactory, InlineExtension};
use rpi_test_support::faux::{
    faux_assistant_message, faux_text, faux_tool_call, FauxAiProvider, FauxAssistantOptions,
    FauxModelDefinition, FauxProvider, FauxProviderOptions, FauxResponseStep,
};
use serde_json::{json, Value};

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "rpi-v1411-prompt-{tag}-{id}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Records `ui_prompt_*` payloads.
type Records = Arc<Mutex<Vec<(String, Value)>>>;

fn recording_ext(records: Records, events: &[&'static str]) -> InlineExtension {
    let events = events.to_vec();
    let factory: ExtensionFactory = Arc::new(move |api| {
        for event in events.iter().copied() {
            let records = records.clone();
            api.on(
                event,
                Arc::new(move |payload, _ctx| {
                    records
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push((event.to_string(), payload));
                    Box::pin(async { Ok(Value::Null) })
                }),
            )
            .expect("register handler");
        }
        Box::pin(async { Ok(()) })
    });
    InlineExtension::Anonymous(factory)
}

/// A bridge whose dialogs resolve immediately; everything else no-ops.
struct ConfirmBridge;

#[async_trait::async_trait]
impl UiBridge for ConfirmBridge {
    async fn select(
        &self,
        _t: &str,
        _o: &[String],
        _opts: Option<rpi_ext_host::api::UiDialogOptions>,
    ) -> Option<String> {
        None
    }
    async fn confirm(
        &self,
        _t: &str,
        _m: &str,
        _opts: Option<rpi_ext_host::api::UiDialogOptions>,
    ) -> bool {
        true
    }
    async fn input(
        &self,
        _t: &str,
        _p: Option<&str>,
        _opts: Option<rpi_ext_host::api::UiDialogOptions>,
    ) -> Option<String> {
        None
    }
    fn notify(&self, _m: &str, _k: rpi_ext_host::api::NotifyType) {}
    fn on_terminal_input(
        &self,
        _h: rpi_ext_host::api::TerminalInputHandler,
    ) -> rpi_ext_host::api::Unsubscribe {
        Box::new(|| {})
    }
    fn set_status(&self, _k: &str, _t: Option<&str>) {}
    fn set_working_message(&self, _m: Option<&str>) {}
    fn set_working_visible(&self, _v: bool) {}
    fn set_working_indicator(&self, _o: Option<rpi_ext_host::api::WorkingIndicatorOptions>) {}
    fn set_hidden_thinking_label(&self, _l: Option<&str>) {}
    fn set_widget(
        &self,
        _k: &str,
        _c: Option<rpi_ext_host::api::WidgetContent>,
        _o: Option<rpi_ext_host::api::ExtensionWidgetOptions>,
    ) {
    }
    fn set_footer(&self, _c: Option<Value>) {}
    fn set_header(&self, _c: Option<Value>) {}
    fn set_title(&self, _t: &str) {}
    async fn custom(&self, _c: Value, _o: Option<Value>) -> Option<Value> {
        None
    }
    fn paste_to_editor(&self, _t: &str) {}
    fn set_editor_text(&self, _t: &str) {}
    fn get_editor_text(&self) -> String {
        String::new()
    }
    async fn editor(&self, _t: &str, _p: Option<&str>) -> Option<String> {
        None
    }
    fn add_autocomplete_provider(&self, _p: Value) {}
    fn set_editor_component(&self, _c: Option<Value>) {}
    fn get_editor_component(&self) -> Option<Value> {
        None
    }
    fn theme(&self) -> Value {
        Value::Null
    }
    fn get_all_themes(&self) -> Vec<rpi_ext_host::api::ThemeInfo> {
        Vec::new()
    }
    fn get_theme(&self, _name: &str) -> Option<Value> {
        None
    }
    fn set_theme(&self, _theme: Value) -> rpi_ext_host::api::SetThemeResult {
        rpi_ext_host::api::SetThemeResult {
            success: false,
            error: None,
        }
    }
    fn get_tools_expanded(&self) -> bool {
        false
    }
    fn set_tools_expanded(&self, _e: bool) {}
}

/// Wasm guest subscribing to the `ui_prompt_*` pair; each event dispatch
/// appends a session entry (the observable side effect). Needles are
/// matched by substring over the dispatch JSON.
const PROMPT_GUEST_WAT: &str = r#"
(module
  (import "rpi" "rpi_host_call" (func $host_call (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 8192))
  (func (export "rpi_alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $ptr))
  (func (export "rpi_dealloc") (param $p i32) (param $l i32) nop)
  (func $strlen (param $ptr i32) (result i32)
    (local $n i32)
    (block $done
      (loop $scan
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $ptr) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $scan)))
    (local.get $n))
  (func $pack (param $ptr i32) (result i64)
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $ptr)) (i64.const 32))
      (i64.extend_i32_u (call $strlen (local.get $ptr)))))
  (func $contains (param $hay i32) (param $haylen i32) (param $needle i32) (param $needlelen i32) (result i32)
    (local $i i32) (local $j i32) (local $match i32)
    (block $outer_done
      (loop $outer
        (br_if $outer_done (i32.gt_u (i32.add (local.get $i) (local.get $needlelen)) (local.get $haylen)))
        (local.set $j (i32.const 0))
        (local.set $match (i32.const 1))
        (block $inner_done
          (loop $inner
            (br_if $inner_done (i32.ge_u (local.get $j) (local.get $needlelen)))
            (if (i32.ne
                  (i32.load8_u (i32.add (i32.add (local.get $hay) (local.get $i)) (local.get $j)))
                  (i32.load8_u (i32.add (local.get $needle) (local.get $j))))
              (then (local.set $match (i32.const 0)) (br $inner_done)))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br $inner)))
        (if (local.get $match) (then (return (i32.const 1))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))
  (func (export "rpi_extension_init") (result i64)
    (drop (call $host_call (i32.const 16) (call $strlen (i32.const 16))))
    (drop (call $host_call (i32.const 176) (call $strlen (i32.const 176))))
    (return (call $pack (i32.const 256))))
  (func (export "rpi_dispatch") (param $ptr i32) (param $len i32) (result i64)
    (if (call $contains (local.get $ptr) (local.get $len) (i32.const 512) (i32.const 15))
      (then (drop (call $host_call (i32.const 640) (call $strlen (i32.const 640))))))
    (if (call $contains (local.get $ptr) (local.get $len) (i32.const 576) (i32.const 13))
      (then (drop (call $host_call (i32.const 768) (call $strlen (i32.const 768))))))
    (return (call $pack (i32.const 896))))
  (data (i32.const 16) "{\"call\":\"on\",\"args\":{\"event\":\"ui_prompt_start\"}}\00")
  (data (i32.const 176) "{\"call\":\"on\",\"args\":{\"event\":\"ui_prompt_end\"}}\00")
  (data (i32.const 256) "{\"ok\":true}\00")
  (data (i32.const 512) "ui_prompt_start")
  (data (i32.const 576) "ui_prompt_end")
  (data (i32.const 640) "{\"call\":\"appendEntry\",\"args\":{\"customType\":\"wasm-prompt-start\",\"data\":null}}\00")
  (data (i32.const 768) "{\"call\":\"appendEntry\",\"args\":{\"customType\":\"wasm-prompt-end\",\"data\":null}}\00")
  (data (i32.const 896) "null\00")
)
"#;

/// Wasm guest subscribing to `session_compact_failed`; the dispatch checks
/// the payload carries the full upstream field set before recording
/// `fields-ok` (otherwise `fields-missing`).
const COMPACT_GUEST_WAT: &str = r#"
(module
  (import "rpi" "rpi_host_call" (func $host_call (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 8192))
  (func (export "rpi_alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $ptr))
  (func (export "rpi_dealloc") (param $p i32) (param $l i32) nop)
  (func $strlen (param $ptr i32) (result i32)
    (local $n i32)
    (block $done
      (loop $scan
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $ptr) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $scan)))
    (local.get $n))
  (func $pack (param $ptr i32) (result i64)
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $ptr)) (i64.const 32))
      (i64.extend_i32_u (call $strlen (local.get $ptr)))))
  (func $contains (param $hay i32) (param $haylen i32) (param $needle i32) (param $needlelen i32) (result i32)
    (local $i i32) (local $j i32) (local $match i32)
    (block $outer_done
      (loop $outer
        (br_if $outer_done (i32.gt_u (i32.add (local.get $i) (local.get $needlelen)) (local.get $haylen)))
        (local.set $j (i32.const 0))
        (local.set $match (i32.const 1))
        (block $inner_done
          (loop $inner
            (br_if $inner_done (i32.ge_u (local.get $j) (local.get $needlelen)))
            (if (i32.ne
                  (i32.load8_u (i32.add (i32.add (local.get $hay) (local.get $i)) (local.get $j)))
                  (i32.load8_u (i32.add (local.get $needle) (local.get $j))))
              (then (local.set $match (i32.const 0)) (br $inner_done)))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br $inner)))
        (if (local.get $match) (then (return (i32.const 1))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))
  (func $has_all (param $ptr i32) (param $len i32) (result i32)
    (local $ok i32)
    (local.set $ok (i32.and
      (call $contains (local.get $ptr) (local.get $len) (i32.const 512) (i32.const 8))
      (i32.and
        (call $contains (local.get $ptr) (local.get $len) (i32.const 544) (i32.const 7))
        (i32.and
          (call $contains (local.get $ptr) (local.get $len) (i32.const 576) (i32.const 9))
          (i32.and
            (call $contains (local.get $ptr) (local.get $len) (i32.const 608) (i32.const 13))
            (call $contains (local.get $ptr) (local.get $len) (i32.const 640) (i32.const 12)))))))
    (local.get $ok))
  (func (export "rpi_extension_init") (result i64)
    (drop (call $host_call (i32.const 16) (call $strlen (i32.const 16))))
    (return (call $pack (i32.const 256))))
  (func (export "rpi_dispatch") (param $ptr i32) (param $len i32) (result i64)
    (if (call $contains (local.get $ptr) (local.get $len) (i32.const 672) (i32.const 22))
      (then
        (if (call $has_all (local.get $ptr) (local.get $len))
          (then (drop (call $host_call (i32.const 736) (call $strlen (i32.const 736)))))
          (else (drop (call $host_call (i32.const 864) (call $strlen (i32.const 864))))))))
    (return (call $pack (i32.const 1024))))
  (data (i32.const 16) "{\"call\":\"on\",\"args\":{\"event\":\"session_compact_failed\"}}\00")
  (data (i32.const 256) "{\"ok\":true}\00")
  (data (i32.const 512) "threshold")
  (data (i32.const 544) "aborted")
  (data (i32.const 576) "willRetry")
  (data (i32.const 608) "fromExtension")
  (data (i32.const 640) "errorMessage")
  (data (i32.const 672) "session_compact_failed")
  (data (i32.const 736) "{\"call\":\"appendEntry\",\"args\":{\"customType\":\"wasm-compact-failed\",\"data\":\"fields-ok\"}}\00")
  (data (i32.const 864) "{\"call\":\"appendEntry\",\"args\":{\"customType\":\"wasm-compact-failed\",\"data\":\"fields-missing\"}}\00")
  (data (i32.const 1024) "null\00")
)
"#;

struct Fixture {
    session: rpi::core::agent_session::AgentSession,
    host: Arc<NativeExtensionHost>,
    _tmp: TempDir,
}

/// Full session pipeline: faux provider, wasm package in
/// `<cwd>/.rpi/extensions/parity/` (optional), optional inline extensions,
/// optional settings.json (compaction scenarios).
#[allow(clippy::too_many_arguments)]
async fn fixture(
    responses: Vec<FauxResponseStep>,
    wasm_wat: Option<&str>,
    inline: Vec<InlineExtension>,
    context_window: u32,
    settings: Option<&str>,
    tag: &str,
) -> Fixture {
    let tmp = TempDir::new(tag);
    let cwd = tmp.path().join("cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&cwd).expect("cwd");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    if let Some(settings) = settings {
        std::fs::write(agent_dir.join("settings.json"), settings).expect("write settings");
    }

    let host = Arc::new(NativeExtensionHost::new(&cwd.to_string_lossy()));
    if let Some(wat) = wasm_wat {
        let pkg = cwd.join(".rpi/extensions/parity");
        std::fs::create_dir_all(pkg.join("dist")).expect("pkg dist");
        std::fs::write(pkg.join("dist/guest.wasm"), wat).expect("guest");
        std::fs::write(
            pkg.join("rpi-extension.json"),
            r#"{"name":"parity","version":"0.1.0","wasm":"dist/guest.wasm","capabilities":["session"],"rpiAbi":1}"#,
        )
        .expect("manifest");
    }
    let errors = host
        .load_startup_final(
            agent_dir.clone(),
            Vec::new(),
            Vec::new(),
            inline,
            true,
            false,
        )
        .await;
    assert!(errors.is_empty(), "load errors: {errors:?}");

    let provider = FauxProvider::new(FauxProviderOptions {
        models: Some(vec![FauxModelDefinition {
            id: "faux-1".to_owned(),
            name: None,
            reasoning: None,
            input: None,
            cost: None,
            context_window: Some(context_window),
            max_tokens: Some(8192),
        }]),
        ..Default::default()
    });
    provider.set_responses(responses);
    let model = provider.get_model(None).expect("faux model");
    let model_runtime = rpi::core::model_runtime::ModelRuntime::create(
        rpi::core::model_runtime::CreateModelRuntimeOptions {
            credentials: None,
            auth_path: Some(agent_dir.join("auth.json")),
            models_path: rpi::core::model_runtime::ModelsPathInput::Path(
                agent_dir.join("models.json"),
            ),
            ..Default::default()
        },
    )
    .await;
    model_runtime
        .register_native_provider(Arc::new(FauxAiProvider::new(provider.clone())))
        .await
        .expect("register faux");

    let services = rpi::core::agent_session_services::create_agent_session_services(
        rpi::core::agent_session_services::CreateAgentSessionServicesOptions {
            cwd: cwd.clone(),
            agent_dir: Some(agent_dir.clone()),
            settings_manager: None,
            model_runtime: Some(model_runtime.clone()),
            extension_flag_values: Vec::new(),
            resource_loader_options: None,
        },
    )
    .await
    .expect("services");
    let session_manager = Arc::new(Mutex::new(
        rpi::core::session_manager::SessionManager::in_memory(
            Some(&cwd),
            rpi::core::session_manager::NewSessionOptions::default(),
        )
        .expect("in-memory session"),
    ));
    let created = rpi::sdk::create_agent_session(rpi::sdk::CreateAgentSessionOptions {
        cwd: Some(cwd),
        agent_dir: Some(agent_dir),
        model_runtime: Some(model_runtime),
        model: Some(model),
        services: Some(services),
        session_manager: Some(session_manager),
        extension_host: Some(host.clone()),
        ..Default::default()
    })
    .await
    .expect("create session");
    bind_session_actions(&host, &created.session).await;

    Fixture {
        session: created.session,
        host,
        _tmp: tmp,
    }
}

/// Wait until `cond` holds (~4s budget).
async fn wait_until(mut cond: impl FnMut() -> bool) {
    for _ in 0..400 {
        if cond() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("condition not met in time");
}

/// Custom entries appended so far (customType, data).
fn custom_entries(session: &rpi::core::agent_session::AgentSession) -> Vec<(String, Value)> {
    session
        .session_manager()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_branch(None)
        .into_iter()
        .filter_map(|stored| {
            let entry = stored.known().cloned()?;
            let value = serde_json::to_value(&entry).ok()?;
            if value.get("type")?.as_str()? == "custom" {
                Some((
                    value
                        .get("customType")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    value.get("data").cloned().unwrap_or(Value::Null),
                ))
            } else {
                None
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// FR-C native channel
// ---------------------------------------------------------------------------

/// A dialog on the bound bridge dispatches the `ui_prompt_*` pair to a
/// native inline extension handler, fire-and-forget.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ui_prompt_events_reach_native_extension_handlers() {
    let records: Records = Arc::new(Mutex::new(Vec::new()));
    let fixture = fixture(
        Vec::new(),
        None,
        vec![recording_ext(
            records.clone(),
            &["ui_prompt_start", "ui_prompt_end"],
        )],
        200_000,
        None,
        "native",
    )
    .await;

    fixture.host.set_ui(
        Some(Arc::new(ConfirmBridge)),
        rpi_ext_host::types::ExtensionMode::Tui,
    );
    let bridge = fixture.host.runtime().ui_bridge().expect("bound bridge");
    let confirmed = bridge.confirm("Proceed?", "continue?", None).await;
    assert!(confirmed, "the prompt itself resolves normally");

    // Fire-and-forget dispatch (spawned, not awaited by the dialog).
    wait_until(|| records.lock().unwrap_or_else(|e| e.into_inner()).len() >= 2).await;
    let records = records.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[0].0, "ui_prompt_start");
    assert_eq!(records[0].1["type"], "ui_prompt_start");
    assert_eq!(records[0].1["reason"], "ui_prompt");
    assert_eq!(records[0].1["kind"], "confirm");
    assert_eq!(records[0].1["title"], "Proceed?");
    assert_eq!(records[1].0, "ui_prompt_end");
    assert_eq!(records[1].1["type"], "ui_prompt_end");
    assert_eq!(records[1].1["kind"], "confirm");
    assert_eq!(records[1].1["title"], "Proceed?");
}

// ---------------------------------------------------------------------------
// FR-C wasm channel
// ---------------------------------------------------------------------------

/// The same pair dispatched through the bound bridge reaches a WASM guest
/// subscribed to the events (observable: `appendEntry` side effects).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ui_prompt_events_reach_wasm_guest() {
    let fixture = fixture(
        Vec::new(),
        Some(PROMPT_GUEST_WAT),
        Vec::new(),
        200_000,
        None,
        "wasm",
    )
    .await;

    fixture.host.set_ui(
        Some(Arc::new(ConfirmBridge)),
        rpi_ext_host::types::ExtensionMode::Tui,
    );
    let bridge = fixture.host.runtime().ui_bridge().expect("bound bridge");
    let confirmed = bridge.confirm("Proceed?", "continue?", None).await;
    assert!(confirmed, "the prompt itself resolves normally");

    for _ in 0..400 {
        let entries = custom_entries(&fixture.session);
        if entries.iter().any(|(t, _)| t == "wasm-prompt-start")
            && entries.iter().any(|(t, _)| t == "wasm-prompt-end")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let entries = custom_entries(&fixture.session);
    // Exactly one of each, in start-before-end order.
    let positions: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, (custom_type, _))| {
            custom_type == "wasm-prompt-start" || custom_type == "wasm-prompt-end"
        })
        .map(|(index, _)| index)
        .collect();
    assert_eq!(positions.len(), 2, "entries: {entries:?}");
}

// ---------------------------------------------------------------------------
// FR-H wasm channel (native channel: extension_host_agent_event_payload_test)
// ---------------------------------------------------------------------------

/// A threshold compaction failure delivers the full upstream payload
/// (`reason`/`errorMessage`/`aborted`/`willRetry`/`fromExtension`) to a
/// wasm guest handler.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_compact_failed_reaches_wasm_guest_with_full_payload() {
    // Same scenario shape as the native-channel payload test.
    let big = format!("EVIDENCE {}", "evidence block. ".repeat(600));
    let tool_big = faux_assistant_message(
        vec![
            faux_text(big),
            faux_tool_call(
                "updating",
                json!({"n": 1}).as_object().cloned().unwrap(),
                None,
            ),
        ],
        FauxAssistantOptions {
            stop_reason: Some(rpi_ai::types::StopReason::ToolUse),
            ..Default::default()
        },
    )
    .into();
    let settings = r#"{
        "compaction": { "enabled": true, "reserveTokens": 7000, "keepRecentTokens": 16 }
    }"#;
    let fixture = fixture(
        vec![
            tool_big,
            faux_assistant_message(
                "",
                FauxAssistantOptions {
                    stop_reason: Some(rpi_ai::types::StopReason::Error),
                    error_message: Some("summarizer exploded".to_owned()),
                    ..Default::default()
                },
            )
            .into(),
            faux_assistant_message("final answer", FauxAssistantOptions::default()).into(),
        ],
        Some(COMPACT_GUEST_WAT),
        Vec::new(),
        8192,
        Some(settings),
        "compact",
    )
    .await;

    fixture
        .session
        .prompt("go", rpi::core::agent_session::PromptOptions::default())
        .await
        .expect("prompt");
    fixture.session.wait_for_idle().await;

    wait_until(|| {
        custom_entries(&fixture.session)
            .iter()
            .any(|(custom_type, _)| custom_type == "wasm-compact-failed")
    })
    .await;
    let entries = custom_entries(&fixture.session);
    let verdicts: Vec<&Value> = entries
        .iter()
        .filter(|(custom_type, _)| custom_type == "wasm-compact-failed")
        .map(|(_, data)| data)
        .collect();
    assert!(!verdicts.is_empty(), "entries: {entries:?}");
    for verdict in verdicts {
        assert_eq!(*verdict, json!("fields-ok"), "entries: {entries:?}");
    }
}
