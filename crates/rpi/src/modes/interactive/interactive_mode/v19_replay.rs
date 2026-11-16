//! Headless, fixed-cadence `UiCommand` replay for V19-00.
//!
//! This developer-only binary drives the real interactive component tree and
//! renderer without a provider, network, or process terminal. It is not part
//! of the distributed CLI command surface.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rpi_agent::messages::AgentMessage;
use rpi_ai::types::{
    ApiKind, AssistantContent, AssistantMessage, AssistantRole, ImageContent,
    SharedAssistantMessage, StopReason, TextContent, ThinkingContent, ToolCall, Usage, UserContent,
    UserContentBlock, UserMessage, UserRole,
};
use rpi_tui::test_vt::VirtualTerminal;
use rpi_tui::tui::TuiMode;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{InteractiveMode, InteractiveModeOptions, UiCommand};
use crate::core::agent_session_runtime::AgentSessionRuntime;
use crate::core::agent_session_services::{
    CreateAgentSessionServicesOptions, create_agent_session_services,
};
use crate::core::session_manager::{NewSessionOptions, SessionManager};
use crate::sdk::{CreateAgentSessionOptions, NoTools, create_agent_session};

const BUNDLE_SCHEMA: &str = "rpi-v19-replay-v1";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplayBundle {
    schema: String,
    scenario: String,
    config: ReplayConfig,
    #[serde(default)]
    history: Vec<String>,
    #[serde(default)]
    extensions: String,
    #[serde(default)]
    image_history: Vec<String>,
    events: Vec<ReplayEvent>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplayConfig {
    mode: String,
    columns: u16,
    rows: u16,
    cadence_ms: u64,
    provider: String,
    model: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplayEvent {
    at_ms: u64,
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    columns: Option<u16>,
    #[serde(default)]
    rows: Option<u16>,
    #[serde(default)]
    count: Option<usize>,
}

#[derive(Debug)]
struct ReplayArgs {
    bundle: PathBuf,
    agent_dir: PathBuf,
    cwd: PathBuf,
    summary: PathBuf,
    terminal_output: PathBuf,
    input_sha256: String,
}

#[derive(Default)]
struct NoopReportInstallTransport;

impl crate::core::telemetry::ReportInstallTransport for NoopReportInstallTransport {
    fn get<'a>(
        &'a self,
        _url: &'a str,
        _user_agent: &'a str,
        _timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Default)]
struct NoopLatestVersionTransport;

impl crate::core::version_check::LatestVersionTransport for NoopLatestVersionTransport {
    fn get<'a>(
        &'a self,
        _url: &'a str,
        _user_agent: &'a str,
        _timeout: Duration,
        _retry: bool,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, String>> + Send + 'a>> {
        Box::pin(async { Ok(None) })
    }
}

fn assistant_message(content: Vec<AssistantContent>) -> AssistantMessage {
    AssistantMessage {
        role: AssistantRole::Assistant,
        content,
        api: ApiKind("openai-completions".to_owned()),
        provider: "custom".to_owned(),
        model: "m1".to_owned(),
        response_model: None,
        response_id: None,
        provider_thinking_level: None,
        thinking_level: None,
        diagnostics: None,
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: 1_700_000_000_000,
        deferred: None,
        end_turn: None,
        raw_stop_reason: None,
        duration_ms: None,
    }
}

fn shared_update(message: &AssistantMessage) -> UiCommand {
    UiCommand::MessageUpdate(SharedAssistantMessage::new(message.clone()))
}

fn append_text(message: &mut AssistantMessage, delta: &str, thinking: bool) {
    if thinking {
        if let Some(AssistantContent::Thinking(block)) = message.content.last_mut() {
            block.thinking.push_str(delta);
            return;
        }
        message
            .content
            .push(AssistantContent::Thinking(ThinkingContent {
                thinking: delta.to_owned(),
                thinking_signature: None,
                redacted: None,
            }));
    } else {
        if let Some(AssistantContent::Text(block)) = message.content.last_mut() {
            block.text.push_str(delta);
            return;
        }
        message.content.push(AssistantContent::Text(TextContent {
            text: delta.to_owned(),
            text_signature: None,
        }));
    }
}

fn apply_tool_delta(
    message: &mut AssistantMessage,
    tools: &mut HashMap<String, String>,
    id: &str,
    name: &str,
    delta: &str,
) {
    let raw = tools.entry(id.to_owned()).or_default();
    raw.push_str(delta);
    let arguments = rpi_ai::utils::json_parse::parse_streaming_json(Some(raw));
    if let Some(AssistantContent::ToolCall(call)) = message
        .content
        .iter_mut()
        .find(|content| matches!(content, AssistantContent::ToolCall(call) if call.id == id))
    {
        call.arguments = arguments;
    } else {
        message.content.push(AssistantContent::ToolCall(ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments,
            thought_signature: None,
            namespace: None,
            arguments_error: None,
        }));
    }
}

fn parse_args() -> Result<ReplayArgs, String> {
    let mut values = HashMap::new();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        if values.insert(flag.clone(), value).is_some() {
            return Err(format!("duplicate argument {flag}"));
        }
    }
    let mut take = |name: &str| -> Result<PathBuf, String> {
        values
            .remove(name)
            .map(PathBuf::from)
            .ok_or_else(|| format!("missing {name}"))
    };
    let parsed = ReplayArgs {
        bundle: take("--bundle")?,
        agent_dir: take("--agent-dir")?,
        cwd: take("--cwd")?,
        summary: take("--summary")?,
        terminal_output: take("--terminal-output")?,
        input_sha256: values
            .remove("--input-sha256")
            .ok_or_else(|| "missing --input-sha256".to_owned())?,
    };
    if !values.is_empty() {
        let unknown = values.keys().cloned().collect::<Vec<_>>().join(",");
        return Err(format!("unknown arguments: {unknown}"));
    }
    Ok(parsed)
}

async fn build_mode(
    args: &ReplayArgs,
    config: &ReplayConfig,
    extensions: &str,
    history: &[String],
    image_history: &[String],
    terminal: &VirtualTerminal,
) -> Result<InteractiveMode, String> {
    let services = create_agent_session_services(CreateAgentSessionServicesOptions {
        cwd: args.cwd.clone(),
        agent_dir: Some(args.agent_dir.clone()),
        settings_manager: None,
        model_runtime: None,
        extension_flag_values: Vec::new(),
        resource_loader_options: None,
    })
    .await
    .map_err(|error| format!("create replay services: {error}"))?;
    let model = services
        .model_runtime
        .get_model(&config.provider, &config.model)
        .ok_or_else(|| format!("unknown replay model {}/{}", config.provider, config.model))?;
    let sessions = args.agent_dir.join("sessions");
    std::fs::create_dir_all(&sessions)
        .map_err(|error| format!("create replay session directory: {error}"))?;
    let mut session_manager =
        SessionManager::create(&args.cwd, Some(&sessions), NewSessionOptions::default())
            .map_err(|error| format!("create file-backed replay session: {error}"))?;
    for text in history {
        session_manager
            .append_message(AgentMessage::Assistant(assistant_message(vec![
                AssistantContent::Text(TextContent {
                    text: text.clone(),
                    text_signature: None,
                }),
            ])))
            .map_err(|error| format!("append replay history: {error}"))?;
    }
    for (index, data) in image_history.iter().enumerate() {
        session_manager
            .append_message(AgentMessage::User(UserMessage {
                role: UserRole::User,
                content: UserContent::Blocks(vec![
                    UserContentBlock::Text(TextContent {
                        text: format!("Replay image {index}"),
                        text_signature: None,
                    }),
                    UserContentBlock::Image(ImageContent {
                        data: data.clone(),
                        mime_type: "image/png".to_owned(),
                    }),
                ]),
                timestamp: 1_700_000_000_000 + index as i64,
            }))
            .map_err(|error| format!("append replay image history: {error}"))?;
    }
    let extension_host = if extensions == "none" {
        None
    } else {
        let host = Arc::new(rpi_ext_host::host::NativeExtensionHost::new(
            &args.cwd.to_string_lossy(),
        ));
        let errors = host
            .load_startup_final(
                args.agent_dir.clone(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                true,
                false,
            )
            .await;
        if !errors.is_empty() {
            return Err(format!("load replay extensions: {errors:?}"));
        }
        Some(host)
    };
    let created = create_agent_session(CreateAgentSessionOptions {
        cwd: Some(args.cwd.clone()),
        agent_dir: Some(args.agent_dir.clone()),
        model: Some(model),
        no_tools: Some(NoTools::All),
        services: Some(services.clone()),
        session_manager: Some(Arc::new(Mutex::new(session_manager))),
        extension_host: extension_host.clone(),
        ..Default::default()
    })
    .await
    .map_err(|error| format!("create replay agent session: {error}"))?;
    let factory_services = services.clone();
    let factory_cwd = args.cwd.clone();
    let factory_agent_dir = args.agent_dir.clone();
    let factory_extension_host = extension_host.clone();
    let runtime = AgentSessionRuntime::new(
        created.session,
        services,
        Arc::new(move |options| {
            let services = factory_services.clone();
            let cwd = factory_cwd.clone();
            let agent_dir = factory_agent_dir.clone();
            let extension_host = factory_extension_host.clone();
            Box::pin(async move {
                let created = create_agent_session(CreateAgentSessionOptions {
                    cwd: Some(cwd),
                    agent_dir: Some(agent_dir),
                    model: None,
                    no_tools: Some(NoTools::All),
                    services: Some(services),
                    session_manager: Some(options.session_manager),
                    extension_host,
                    ..Default::default()
                })
                .await?;
                Ok(
                    crate::core::agent_session_runtime::CreateAgentSessionRuntimeResult {
                        session: created.session,
                        services: created.services.ok_or_else(|| {
                            crate::RpiError::Resource(
                                "replay session factory returned no services".to_owned(),
                            )
                        })?,
                        diagnostics: Vec::new(),
                        model_fallback_message: created.model_fallback_message,
                    },
                )
            })
        }),
        Vec::new(),
        None,
    );
    let mode = match config.mode.as_str() {
        "regular" => TuiMode::Regular,
        "fullscreen" => TuiMode::Fullscreen,
        other => return Err(format!("unknown TUI mode {other}")),
    };
    let mut mode = InteractiveMode::with_terminal(
        runtime,
        InteractiveModeOptions {
            tui_mode: mode,
            ..Default::default()
        },
        Box::new(terminal.clone()),
    );
    mode.ui_state
        .set_report_install_transport(Arc::new(NoopReportInstallTransport));
    mode.ui_state
        .set_latest_version_transport(Arc::new(NoopLatestVersionTransport));
    mode.ui_state
        .set_package_update_checker(Arc::new(|_| Vec::new()));
    mode.init().await;
    Ok(mode)
}

fn validate_bundle(bundle: &ReplayBundle) -> Result<(), String> {
    if bundle.schema != BUNDLE_SCHEMA {
        return Err(format!("unsupported replay schema {}", bundle.schema));
    }
    if bundle.scenario.is_empty() || bundle.events.is_empty() {
        return Err("scenario and events must be non-empty".to_owned());
    }
    if bundle.config.columns == 0 || bundle.config.rows == 0 || bundle.config.cadence_ms == 0 {
        return Err("columns, rows and cadenceMs must be positive".to_owned());
    }
    Ok(())
}

async fn emit_provider_delta(
    host: &rpi_ext_host::host::NativeExtensionHost,
    message: &AssistantMessage,
    kind: &str,
    delta: &str,
) -> Result<(), String> {
    let shared = SharedAssistantMessage::new(message.clone());
    let content_index = message.content.len().saturating_sub(1);
    let event = match kind {
        "thinking_delta" => rpi_ai::types::StreamEvent::ThinkingDelta {
            content_index,
            delta: delta.to_owned(),
            partial: shared.clone(),
        },
        "tool_call_delta" => rpi_ai::types::StreamEvent::ToolCallDelta {
            content_index,
            delta: delta.to_owned(),
            partial: shared.clone(),
        },
        _ => rpi_ai::types::StreamEvent::TextDelta {
            content_index,
            delta: delta.to_owned(),
            partial: shared.clone(),
        },
    };
    if host.has_handlers("message_update_delta") {
        let payload = crate::modes::json_event::message_update_payload(&shared, &event)?;
        host.emit("message_update_delta", payload).await;
    }
    if host.has_handlers("message_update") {
        host.emit(
            "message_update",
            json!({
                "message": AgentMessage::Assistant(message.clone()),
                "assistantMessageEvent": event,
            }),
        )
        .await;
    }
    Ok(())
}

fn sample_checkpoint(name: &str) -> Result<(), String> {
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err("sample name must contain only ASCII alphanumerics, '-' or '_'".to_owned());
    }
    let directory = std::env::var_os("RPI_V19_SAMPLE_DIR")
        .map(PathBuf::from)
        .ok_or("sample event requires RPI_V19_SAMPLE_DIR")?;
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("create sample directory: {error}"))?;
    std::fs::write(directory.join(format!("{name}.marker")), name)
        .map_err(|error| format!("write sample marker: {error}"))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let acknowledgement = directory.join(format!("{name}.ack"));
    while !acknowledgement.is_file() {
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for sample acknowledgement {name}"
            ));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}

async fn replay(args: &ReplayArgs, bytes: &[u8]) -> Result<Value, String> {
    let bundle: ReplayBundle =
        serde_json::from_slice(bytes).map_err(|error| format!("parse replay bundle: {error}"))?;
    validate_bundle(&bundle)?;
    let terminal = VirtualTerminal::new(
        usize::from(bundle.config.columns),
        usize::from(bundle.config.rows),
    );
    let mut mode = build_mode(
        args,
        &bundle.config,
        &bundle.extensions,
        &bundle.history,
        &bundle.image_history,
        &terminal,
    )
    .await?;
    let ui = Arc::clone(&mode.ui_state);
    let extension_host = if bundle.extensions != "none" {
        let runner = mode.session.extension_runner();
        let host = runner
            .as_any()
            .and_then(|any| {
                any.downcast_ref::<crate::core::extension_host_adapter::ExtensionHostAdapter>()
            })
            .map(|adapter| adapter.host().clone())
            .ok_or("extension replay requires the native extension host")?;
        Some(host)
    } else {
        None
    };

    ui.drain_events();
    mode.ui.render_now(true);

    let started = Instant::now();
    let mut current: Option<AssistantMessage> = None;
    let mut tool_arguments = HashMap::new();
    let mut input_deltas = 0usize;
    let mut keyboard_inputs = 0usize;
    let mut input_latencies_ns = Vec::new();
    let mut last_session_path = None;
    let mut samples = Vec::new();
    let mut result = Ok(());

    for event in &bundle.events {
        let due = started + Duration::from_millis(event.at_ms);
        std::thread::sleep(due.saturating_duration_since(Instant::now()));
        let input_started = Instant::now();
        let mut keyboard_expected = None;
        let outcome = async {
            match event.kind.as_str() {
                "agent_start" => ui.push(UiCommand::AgentStart),
                "message_start" => {
                    let message = assistant_message(Vec::new());
                    current = Some(message.clone());
                    tool_arguments.clear();
                    ui.push(UiCommand::MessageStart(AgentMessage::Assistant(message)));
                }
                "text_delta" | "thinking_delta" => {
                    let delta = event.text.as_deref().ok_or("delta requires text")?;
                    let message = current.as_mut().ok_or("delta before message_start")?;
                    append_text(message, delta, event.kind == "thinking_delta");
                    ui.push(shared_update(message));
                    if let Some(host) = &extension_host {
                        emit_provider_delta(host, message, &event.kind, delta).await?;
                    }
                    input_deltas += 1;
                }
                "tool_call_delta" => {
                    let delta = event
                        .text
                        .as_deref()
                        .ok_or("tool_call_delta requires text")?;
                    let id = event.id.as_deref().ok_or("tool_call_delta requires id")?;
                    let name = event
                        .name
                        .as_deref()
                        .ok_or("tool_call_delta requires name")?;
                    let message = current.as_mut().ok_or("tool delta before message_start")?;
                    apply_tool_delta(message, &mut tool_arguments, id, name, delta);
                    ui.push(shared_update(message));
                    if let Some(host) = &extension_host {
                        emit_provider_delta(host, message, &event.kind, delta).await?;
                    }
                    input_deltas += 1;
                }
                "tool_execution_start" => {
                    ui.push(UiCommand::ToolExecutionStart {
                        tool_call_id: event.id.clone().ok_or("tool start requires id")?,
                        tool_name: event.name.clone().ok_or("tool start requires name")?,
                        args: event.arguments.clone().unwrap_or_else(|| json!({})),
                    });
                }
                "tool_execution_end" => {
                    ui.push(UiCommand::ToolExecutionEnd {
                        tool_call_id: event.id.clone().ok_or("tool end requires id")?,
                        result: event.result.clone().unwrap_or_else(|| json!({})),
                        is_error: false,
                        duration_ms: Some(0),
                    });
                }
                "message_end" => {
                    let message = current.take().ok_or("message_end before message_start")?;
                    mode.session
                        .session_manager()
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .append_message(AgentMessage::Assistant(message.clone()))
                        .map_err(|error| format!("persist replay message: {error}"))?;
                    if let Some(host) = &extension_host {
                        host.flush_throttled("message_update").await;
                        host.flush_throttled("message_update_delta").await;
                        let agent_message = AgentMessage::Assistant(message.clone());
                        let payload = serde_json::to_value(agent_message)
                            .map_err(|error| error.to_string())?;
                        host.emit_message_end(payload).await;
                    }
                    ui.push(UiCommand::MessageEnd(AgentMessage::Assistant(message)));
                }
                "agent_settled" => {
                    if let Some(host) = &extension_host {
                        host.emit("agent_end", json!({"messages": []})).await;
                        host.emit("agent_settled", json!({"aborted": false})).await;
                    }
                    ui.push(UiCommand::AgentEnd);
                    ui.push(UiCommand::AgentSettled { aborted: false });
                }
                "new_session" => {
                    last_session_path = mode
                        .session
                        .session_manager()
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .get_session_file()
                        .map(PathBuf::from);
                    mode.handle_new_command().await;
                }
                "resume_session" => {
                    let path = event
                        .text
                        .as_deref()
                        .map(PathBuf::from)
                        .or_else(|| last_session_path.clone())
                        .ok_or("resume_session requires text or a preceding new_session")?;
                    mode.handle_resume_command(&path.to_string_lossy()).await;
                }
                "model_switch" => {
                    let spec = event.text.as_deref().ok_or("model_switch requires text")?;
                    let (provider, model_id) = spec
                        .split_once('/')
                        .ok_or("model_switch text must be provider/model")?;
                    let next = mode
                        .session
                        .model_runtime()
                        .get_model(provider, model_id)
                        .ok_or_else(|| format!("unknown switch model {spec}"))?;
                    mode.session
                        .set_model(next)
                        .await
                        .map_err(|error| format!("switch replay model: {error}"))?;
                }
                "sample" => {}
                "resize" => {
                    terminal.resize(
                        usize::from(event.columns.ok_or("resize requires columns")?),
                        usize::from(event.rows.ok_or("resize requires rows")?),
                    );
                    mode.ui.pump(Some(Duration::ZERO));
                }
                "scroll_back" => {
                    for _ in 0..event.count.unwrap_or(1) {
                        terminal.send_input("\x1b[5~");
                    }
                    mode.ui.pump(Some(Duration::ZERO));
                }
                "keyboard_input" => {
                    let text = event
                        .text
                        .as_deref()
                        .ok_or("keyboard_input requires text")?;
                    keyboard_inputs += 1;
                    keyboard_expected = Some(text);
                    terminal.send_input(text);
                    mode.ui.pump(Some(Duration::ZERO));
                }
                "wait" => {}
                other => return Err(format!("unknown replay event kind {other}")),
            }
            Ok(())
        }
        .await;
        if let Err(error) = outcome {
            result = Err(error);
            break;
        }
        ui.drain_events();
        mode.ui.render_now(false);
        if let Some(expected) = keyboard_expected {
            let visible = terminal.get_viewport().join("\n").contains(expected);
            if !visible {
                result = Err(format!(
                    "keyboard input {expected:?} did not become visible"
                ));
                break;
            }
            input_latencies_ns.push(input_started.elapsed().as_nanos());
        }
        if event.kind == "sample" {
            let Some(name) = event.name.as_deref() else {
                result = Err("sample requires name".to_owned());
                break;
            };
            if let Err(error) = sample_checkpoint(name) {
                result = Err(error);
                break;
            }
            samples.push(name.to_owned());
        }
    }

    let terminal_bytes = terminal.get_writes();
    let terminal_sha256 = format!("{:x}", Sha256::digest(terminal_bytes.as_bytes()));
    let final_screen = terminal.get_viewport();
    let final_screen_sha256 = format!("{:x}", Sha256::digest(final_screen.join("\n").as_bytes()));
    std::fs::write(&args.terminal_output, terminal_bytes.as_bytes())
        .map_err(|error| format!("write terminal output: {error}"))?;
    let shutdown_result = result.clone();
    mode.shutdown().await;
    shutdown_result?;

    let frame_log = std::env::var_os("RPI_V19_BENCHMARK_LOG")
        .map(PathBuf::from)
        .ok_or("RPI_V19_BENCHMARK_LOG is required")?;
    let frames = std::fs::read_to_string(&frame_log)
        .map_err(|error| format!("read frame log {}: {error}", frame_log.display()))?
        .lines()
        .filter(|line| serde_json::from_str::<Value>(line).is_ok())
        .filter(|line| {
            serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|value| value.get("type").cloned())
                == Some(Value::String("frame".to_owned()))
        })
        .count();
    if frames == 0 {
        return Err("replay produced no valid frame measurements".to_owned());
    }
    Ok(json!({
        "schema": BUNDLE_SCHEMA,
        "scenario": bundle.scenario,
        "inputSha256": args.input_sha256,
        "mode": bundle.config.mode,
        "columns": bundle.config.columns,
        "rows": bundle.config.rows,
        "cadenceMs": bundle.config.cadence_ms,
        "provider": bundle.config.provider,
        "model": bundle.config.model,
        "historyMessages": bundle.history.len(),
        "historyBytes": bundle.history.iter().map(String::len).sum::<usize>(),
        "events": bundle.events.len(),
        "extensions": bundle.extensions,
        "inputDeltas": input_deltas,
        "keyboardInputs": keyboard_inputs,
        "samples": samples,
        "imageHistory": bundle.image_history.len(),
        "inputLatenciesNs": input_latencies_ns,
        "frames": frames,
        "terminalOutputBytes": terminal_bytes.len(),
        "terminalOutputSha256": terminal_sha256,
        "finalScreenSha256": final_screen_sha256,
        "finalScreen": final_screen,
        "terminalOutput": args.terminal_output,
        "frameLog": frame_log,
    }))
}

/// Entry point used by the `rpi-v19-replay` development binary.
pub async fn run_from_env() -> Result<(), String> {
    if cfg!(debug_assertions) {
        return Err("replay requires a release-profile build".to_owned());
    }
    let args = parse_args()?;
    let bytes = std::fs::read(&args.bundle)
        .map_err(|error| format!("read bundle {}: {error}", args.bundle.display()))?;
    let actual_hash = format!("{:x}", Sha256::digest(&bytes));
    if actual_hash != args.input_sha256 {
        return Err(format!(
            "input hash mismatch: expected {}, got {actual_hash}",
            args.input_sha256
        ));
    }
    let summary = replay(&args, &bytes).await?;
    let encoded = serde_json::to_vec_pretty(&summary)
        .map_err(|error| format!("encode replay summary: {error}"))?;
    std::fs::write(&args.summary, encoded)
        .map_err(|error| format!("write summary {}: {error}", args.summary.display()))?;
    Ok(())
}
