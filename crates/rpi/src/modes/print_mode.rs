//! Print mode (single-shot): send prompts, output result, exit.
//!
//! Port of `packages/coding-agent/src/modes/print-mode.ts` @ pi 0.82.1
//! (2efa728), with the JSON event path updated to pi 0.84.1+ (4181f66, T18):
//! JSON events stream to stdout as they are emitted via
//! `writeRawStdout(JSON.stringify(toJsonEvent(event)))`
//! (print-mode.ts:108-112) instead of being buffered until exit. Covers both
//! `text` (final response only) and `json` (full event stream) output modes.
//!
//! Signal handling (print-mode.ts:50-66): upstream kills tracked detached
//! children, disposes the runtime, then exits 143 (SIGTERM) / 129 (SIGHUP).
//! rpi has no detached-child registry (D-011); the handler exits directly
//! with the same codes — abort tokens do not outlive process death.

use std::io::Write;

use rpi_agent::messages::AgentMessage;
use rpi_ai::types::{ImageContent, StopReason};

use crate::core::agent_session::AgentSessionEvent;
use crate::core::agent_session_runtime::AgentSessionRuntime;
use crate::core::extensions::ExtensionMode;
use crate::core::output_guard::RawStdout;
use crate::core::session_manager::SessionManager;
use crate::modes::json_event::to_json_event;

/// `mode: "text" | "json"` (print-mode.ts:19).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrintOutputMode {
    Text,
    Json,
}

/// `PrintModeOptions` (print-mode.ts:18-26).
pub struct PrintModeOptions {
    pub mode: PrintOutputMode,
    /// Additional prompts sent after `initial_message`.
    pub messages: Vec<String>,
    /// First message to send (may contain `@file` content).
    pub initial_message: Option<String>,
    pub initial_images: Option<Vec<ImageContent>>,
}

/// Register SIGTERM/SIGHUP handlers exiting 143/129 (print-mode.ts:47-63).
fn register_signal_handlers() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        for (kind, code) in [(SignalKind::terminate(), 143), (SignalKind::hangup(), 129)] {
            if let Ok(mut stream) = signal(kind) {
                std::thread::spawn(move || {
                    // Dedicated thread with a tiny runtime: the handler must
                    // fire even while the main runtime is parked on I/O.
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build();
                    if let Ok(runtime) = runtime {
                        runtime.block_on(async move {
                            stream.recv().await;
                        });
                    }
                    std::process::exit(code);
                });
            }
        }
    }
}

/// Serialize the session header line for JSON mode
/// (`JSON.stringify(header)`, print-mode.ts:123-126).
fn session_header_json(session_manager: &SessionManager) -> Option<String> {
    let header = session_manager.get_header()?;
    serde_json::to_string(&rpi_agent::session::FileEntry::Session(header.clone())).ok()
}

/// Build the print/json output bridge for `ui.emitText` (ADR-0040 /
/// 04 §7.4). Both flavors keep `hasUI = false` (`is_noop`) but expose the
/// mode's controlled stdout sink: plain text gets one trailing newline,
/// JSON emits the RPC-shaped notify frame.
pub(crate) fn output_ui_bridge(
    mode: PrintOutputMode,
    out: RawStdout,
    default_theme: serde_json::Value,
) -> std::sync::Arc<dyn rpi_ext_host::api::UiBridge> {
    let sink: rpi_ext_host::bridges::UiOutputSink =
        std::sync::Arc::new(move |chunk: &str| out.write(chunk));
    match mode {
        PrintOutputMode::Json => std::sync::Arc::new(
            rpi_ext_host::bridges::OutputUiBridge::json_notify(default_theme, sink),
        ),
        PrintOutputMode::Text => std::sync::Arc::new(rpi_ext_host::bridges::OutputUiBridge::text(
            default_theme,
            sink,
        )),
    }
}

/// `runPrintMode` (print-mode.ts:33-169). `out` is the shared raw-stdout
/// writer (see below); `err` is the process stderr in production and a
/// captured buffer in tests.
///
/// `out` is a [`RawStdout`] rather than the v0.1 `&mut dyn Write` because
/// JSON-mode events are now written from the session's **synchronous** event
/// listener, which requires a `'static` shared owner — a borrow cannot move
/// into the subscription closure. The blocking write inside the listener
/// *is* the backpressure: session listeners run inside the agent loop's
/// ordered listener barrier, so a slow stdout stalls the event source
/// (print-mode.ts:113-118 `session.agent.subscribe(async () =>
/// waitForRawStdoutBackpressure())`; see `core::output_guard` for the full
/// semantic mapping). No events are buffered, dropped, or merged.
pub async fn run_print_mode(
    runtime: &mut AgentSessionRuntime,
    options: PrintModeOptions,
    out: RawStdout,
    err: &mut dyn Write,
) -> i32 {
    register_signal_handlers();

    let mut exit_code = 0;

    // JSON mode: header line first (print-mode.ts:122-127).
    if options.mode == PrintOutputMode::Json {
        let manager = runtime.session().session_manager();
        let manager = manager.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(line) = session_header_json(&manager) {
            out.write(&format!("{line}\n"));
        }
    }

    // bindExtensions + event subscription (print-mode.ts:71-109).
    let session = runtime.session().clone();
    let print_mode = match options.mode {
        PrintOutputMode::Json => ExtensionMode::Json,
        PrintOutputMode::Text => ExtensionMode::Print,
    };
    session
        .bind_extensions(crate::core::agent_session::ExtensionBindings {
            mode: Some(print_mode),
            on_error: None,
            // print/json: shutdown is a no-op (docs/extensions.md:1018-1034).
            shutdown: None,
        })
        .await;
    // `setUIContext` equivalent (T15 W4; ADR-0040 / V18-02): print and
    // `--mode json` bind an output-capable bridge whose `hasUI` stays false.
    // `ui.emitText` writes controlled RawStdout (plain text, or a notify
    // frame in JSON mode) without pretending to be an interactive UI.
    if let Some(host) = session
        .extension_runner()
        .as_any()
        .and_then(|any| {
            any.downcast_ref::<crate::core::extension_host_adapter::ExtensionHostAdapter>()
        })
        .map(|adapter| adapter.host().clone())
    {
        let bridge = output_ui_bridge(
            options.mode,
            out.clone(),
            crate::core::themes::default_theme_json(),
        );
        host.set_ui(
            Some(bridge),
            match print_mode {
                ExtensionMode::Json => rpi_ext_host::types::ExtensionMode::Json,
                _ => rpi_ext_host::types::ExtensionMode::Print,
            },
        );
    }
    // JSON mode: stream each event to stdout as it is emitted
    // (print-mode.ts:108-112 `writeRawStdout(JSON.stringify(toJsonEvent(event)))`).
    // `to_json_event` strips the cumulative `message`/`partial` snapshots so
    // `message_update` stays delta-only on the wire (T18, docs/json.md:82-85).
    // The blocking write runs inside the session's synchronous event
    // dispatch — which the agent loop awaits in subscription order — so a
    // slow consumer stalls the event source itself instead of filling an
    // unbounded buffer (print-mode.ts:113-118 backpressure hook, fused; see
    // `core::output_guard`).
    let json_mode = options.mode == PrintOutputMode::Json;
    let event_out = out.clone();
    let _unsubscribe = session.subscribe(std::sync::Arc::new(move |event: AgentSessionEvent| {
        if json_mode {
            // `toJsonEvent` can throw on invariant violations
            // (json-event.ts:50, :24-26); upstream's unguarded call crashes
            // the process. rpi rejects the event instead — stderr
            // diagnostic, no wire line (V14-03 §5 实现取舍).
            match to_json_event(&event) {
                Ok(wire) => {
                    if let Ok(mut line) = serde_json::to_string(&wire) {
                        line.push('\n');
                        event_out.write(&line);
                    }
                }
                Err(error) => {
                    eprintln!("json event conversion failed: {error}");
                }
            }
        }
    }));
    runtime.set_rebind_session(Some(Box::new(|_session| {
        Box::pin(async move {
            // Print mode never replaces its session (no session-replacement
            // commands); the rebind hook is registered for parity
            // (print-mode.ts:67-69) but stays a no-op until T12/T15.
        })
    })));

    let result: Result<(), String> = async {
        if let Some(initial_message) = &options.initial_message {
            session
                .prompt(
                    initial_message,
                    crate::core::agent_session::PromptOptions {
                        images: options.initial_images.clone(),
                        ..Default::default()
                    },
                )
                .await
                // Upstream prints `error.message` verbatim (print-mode.ts:149-150).
                .map_err(|e| e.raw_message())?;
        }
        for message in &options.messages {
            session
                .prompt(message, Default::default())
                .await
                .map_err(|e| e.raw_message())?;
        }
        Ok(())
    }
    .await;

    if let Err(error) = result {
        let _ = writeln!(err, "{error}");
        exit_code = 1;
    } else if options.mode == PrintOutputMode::Text {
        // Print the last assistant message's text blocks
        // (print-mode.ts:139-156).
        let messages = session.messages();
        if let Some(AgentMessage::Assistant(assistant)) = messages.last() {
            if assistant.stop_reason == StopReason::Error
                || assistant.stop_reason == StopReason::Aborted
            {
                let message = assistant.error_message.clone().unwrap_or_else(|| {
                    format!("Request {}", stop_reason_str(assistant.stop_reason))
                });
                let _ = writeln!(err, "{message}");
                exit_code = 1;
            } else {
                for content in &assistant.content {
                    if let rpi_ai::types::AssistantContent::Text(text) = content {
                        out.write(&format!("{}\n", text.text));
                    }
                }
                // A response cut off at the output token limit still prints
                // the partial text, then warns on stderr and exits non-zero
                // (print-mode.ts:134-148, #10799).
                if assistant.stop_reason == StopReason::Length {
                    let _ = writeln!(
                        err,
                        "Response was truncated: the model reached its output token limit."
                    );
                    exit_code = 1;
                }
            }
        }
    }

    runtime.dispose().await;
    // `flushRawStdout` (print-mode.ts:167).
    out.flush();
    let _ = err.flush();
    // Upstream exits 1 when the stdout write chain rejects
    // (output-guard.ts:90-92); rpi records the error and maps it here.
    if exit_code == 0 && out.has_error() {
        exit_code = 1;
    }
    exit_code
}

fn stop_reason_str(reason: StopReason) -> &'static str {
    match reason {
        StopReason::Stop => "stop",
        StopReason::Length => "length",
        StopReason::ToolUse => "toolUse",
        StopReason::Error => "error",
        StopReason::Aborted => "aborted",
        StopReason::Pending => "pending",
        // Placeholder variant (R2.1.1); mapped to its wire name explicitly.
        StopReason::Deferred => "deferred",
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use rpi_ext_host::api::{NotifyType, UiBridge, UiDialogOptions, UiOutputError};
    use serde_json::Value;

    use super::{PrintOutputMode, output_ui_bridge};
    use crate::core::output_guard::RawStdout;

    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl SharedBuffer {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).expect("utf8")
        }
    }

    impl Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn bridge(mode: PrintOutputMode, theme: Value) -> (Arc<dyn UiBridge>, SharedBuffer) {
        let buffer = SharedBuffer::default();
        let out = RawStdout::new(Box::new(buffer.clone()));
        (output_ui_bridge(mode, out, theme), buffer)
    }

    #[test]
    fn text_bridge_writes_one_trailing_newline_and_stays_non_interactive() {
        let (bridge, buffer) = bridge(PrintOutputMode::Text, Value::Null);
        assert!(bridge.is_noop(), "hasUI must stay false");
        bridge
            .emit_text("statusline: help", true)
            .expect("validate");
        assert_eq!(buffer.text(), "", "validateOnly writes nothing");
        bridge.emit_text("statusline: help", false).expect("emit");
        assert_eq!(buffer.text(), "statusline: help\n");
        bridge.emit_text("two\nlines", false).expect("emit");
        assert_eq!(buffer.text(), "statusline: help\ntwo\nlines\n");
    }

    #[test]
    fn emit_text_write_failure_is_recorded_for_the_mode_exit_code() {
        struct Failing(Arc<AtomicUsize>);
        impl std::io::Write for Failing {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "closed pipe",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let writes = Arc::new(AtomicUsize::new(0));
        let out = RawStdout::new(Box::new(Failing(writes.clone())));
        let bridge = output_ui_bridge(PrintOutputMode::Text, out.clone(), Value::Null);
        // `emitText` is fire-and-forget like `ui.notify`; the mode records the
        // broken stdout and maps it to exit code 1 at the end of the run.
        bridge.emit_text("statusline: help", false).expect("emit");
        assert!(out.has_error());
        bridge
            .emit_text("again", false)
            .expect("later writes dropped");
        assert!(out.has_error());
        assert_eq!(
            writes.load(Ordering::SeqCst),
            1,
            "the output guard drops writes after the first error"
        );
    }

    #[test]
    fn json_bridge_emits_the_rpc_notify_frame() {
        let (bridge, buffer) = bridge(PrintOutputMode::Json, Value::Null);
        assert!(bridge.is_noop(), "hasUI must stay false");
        bridge
            .emit_text("statusline: help", true)
            .expect("validate");
        assert_eq!(buffer.text(), "");
        bridge.emit_text("statusline: help", false).expect("emit");
        let frame: Value = serde_json::from_str(buffer.text().trim()).expect("frame json");
        assert_eq!(frame["type"], "extension_ui_request");
        assert_eq!(frame["method"], "notify");
        assert_eq!(frame["message"], "statusline: help");
        assert_eq!(frame["notifyType"], "info");
        assert!(frame["id"].as_str().is_some_and(|id| !id.is_empty()));
    }

    #[tokio::test]
    async fn dialogs_stay_noops_on_the_output_bridge() {
        let (bridge, _buffer) = bridge(PrintOutputMode::Text, Value::Null);
        assert!(bridge.select("t", &[], None).await.is_none());
        assert!(!bridge.confirm("t", "m", None).await);
        assert!(bridge.input("t", None, None).await.is_none());
        assert!(bridge.editor("t", None).await.is_none());
        assert!(
            bridge
                .custom(rpi_ext_host::types::ComponentTree::default(), None)
                .await
                .is_none()
        );
    }

    /// Compile-time contract: the default trait surface is unsupported, so a
    /// bridge that does not opt in fails closed instead of silently dropping
    /// command output.
    #[test]
    fn null_default_is_unsupported_output() {
        struct NoOutput;
        #[async_trait::async_trait]
        impl UiBridge for NoOutput {
            async fn select(
                &self,
                _t: &str,
                _o: &[String],
                _opts: Option<UiDialogOptions>,
            ) -> Option<String> {
                None
            }
            async fn confirm(&self, _t: &str, _m: &str, _o: Option<UiDialogOptions>) -> bool {
                false
            }
            async fn input(
                &self,
                _t: &str,
                _p: Option<&str>,
                _o: Option<UiDialogOptions>,
            ) -> Option<String> {
                None
            }
            fn notify(&self, _m: &str, _k: NotifyType) {}
            fn on_terminal_input(
                &self,
                _h: rpi_ext_host::api::TerminalInputHandler,
            ) -> rpi_ext_host::api::Unsubscribe {
                Box::new(|| {})
            }
            fn set_status(&self, _k: &str, _t: Option<&str>) {}
            fn set_working_message(&self, _m: Option<&str>) {}
            fn set_working_visible(&self, _v: bool) {}
            fn set_working_indicator(
                &self,
                _o: Option<rpi_ext_host::api::WorkingIndicatorOptions>,
            ) {
            }
            fn set_hidden_thinking_label(&self, _l: Option<&str>) {}
            fn set_widget(
                &self,
                _k: &str,
                _c: Option<rpi_ext_host::api::WidgetContent>,
                _o: Option<rpi_ext_host::api::ExtensionWidgetOptions>,
            ) {
            }
            fn set_footer(&self, _c: Option<rpi_ext_host::types::ComponentTree>) {}
            fn set_header(&self, _c: Option<rpi_ext_host::types::ComponentTree>) {}
            fn set_title(&self, _t: &str) {}
            async fn custom(
                &self,
                _c: rpi_ext_host::types::ComponentTree,
                _o: Option<Value>,
            ) -> Option<Value> {
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
            fn set_editor_component(&self, _c: Option<rpi_ext_host::types::ComponentTree>) {}
            fn get_editor_component(&self) -> Option<rpi_ext_host::types::ComponentTree> {
                None
            }
            fn theme(&self) -> Value {
                Value::Null
            }
            fn get_all_themes(&self) -> Vec<rpi_ext_host::api::ThemeInfo> {
                Vec::new()
            }
            fn get_theme(&self, _n: &str) -> Option<Value> {
                None
            }
            fn set_theme(&self, _t: Value) -> rpi_ext_host::api::SetThemeResult {
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
        let error: UiOutputError = NoOutput.emit_text("x", true).expect_err("default");
        assert_eq!(error, UiOutputError::UnsupportedOutput);
        assert_eq!(error.to_string(), "unsupported-output");
    }
}
