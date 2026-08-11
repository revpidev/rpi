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
        use tokio::signal::unix::{signal, SignalKind};
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
    // `setUIContext(noOpUIContext)` equivalent (T15 W4): print/json modes
    // bind the null bridge — `hasUI` stays false, `ctx.ui` never throws.
    if let Some(host) = session
        .extension_runner()
        .as_any()
        .and_then(|any| {
            any.downcast_ref::<crate::core::extension_host_adapter::ExtensionHostAdapter>()
        })
        .map(|adapter| adapter.host().clone())
    {
        host.set_ui(
            Some(std::sync::Arc::new(
                rpi_ext_host::bridges::NullUiBridge::new(crate::core::themes::default_theme_json()),
            )),
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
            if let Ok(mut line) = serde_json::to_string(&to_json_event(&event)) {
                line.push('\n');
                event_out.write(&line);
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
