//! Rust SDK surface (requirements §2.5): `create_agent_session` and
//! re-exports, mirroring `packages/coding-agent/src/core/sdk.ts` @ pi 0.82.1
//! (2efa728).
//!
//! The `setDefaultStreamFn(streamSimple)` fallback (sdk.ts:36) has no pir
//! counterpart: the assembly layer always injects a `StreamFn`
//! (coding-standards §4.2). The injected stream fn does mirror sdk.ts:312
//! by converting the plain-`StreamOptions` shape into
//! `SimpleStreamOptions` at the `stream_simple` boundary (reasoning +
//! thinking_budgets), so the agent path reaches the adapters' reasoning
//! mapping exactly like upstream `modelRuntime.streamSimple(...)`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use pir_agent::types::ThinkingLevel;
use pir_agent::{Agent, AgentMessage, AgentOptions, AgentTool, InitialAgentState};
use pir_ai::types::{Message, Model, StreamOptions, TextContent};

use crate::config::{get_agent_dir, get_default_session_dir_path};
use crate::core::agent_session::{AgentSession, AgentSessionConfig};
use crate::core::agent_session_services::CreateAgentSessionServicesOptions;
use crate::core::auth_guidance::format_no_models_available_message;
use crate::core::extensions::{new_extension_runner_ref, NoopExtensionRunner, SessionStartEvent};
use crate::core::model_resolver::{
    find_initial_model, FindInitialModelOptions, ScopedModel, DEFAULT_THINKING_LEVEL,
};
use crate::core::model_runtime::{CreateModelRuntimeOptions, ModelRuntime, ModelsPathInput};
use crate::core::session_manager::{NewSessionOptions, SessionManager};
use crate::core::settings_manager::{SettingsManager, SettingsManagerCreateOptions};
use crate::error::PirError;
use crate::tools::path_utils::resolve_path;

pub use crate::core::agent_session_runtime::{
    create_agent_session_runtime, AgentSessionRuntime, CreateAgentSessionRuntimeResult,
    CreateRuntimeOptions,
};
pub use crate::core::agent_session_services::{
    create_agent_session_services, AgentSessionRuntimeDiagnostic, AgentSessionServices,
};

/// `noTools: "all" | "builtin"` (sdk.ts:60-61).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoTools {
    /// Start with no tools enabled.
    All,
    /// Disable the default built-in tools but keep extension/custom tools.
    Builtin,
}

/// `CreateAgentSessionOptions` (sdk.ts:38-85), T10 subset.
#[derive(Default)]
pub struct CreateAgentSessionOptions {
    /// Working directory for project-local discovery. Default: process cwd.
    pub cwd: Option<PathBuf>,
    /// Global config directory. Default: `~/.pir/agent`.
    pub agent_dir: Option<PathBuf>,
    /// Canonical model/auth runtime.
    pub model_runtime: Option<Arc<ModelRuntime>>,
    /// Model to use. Default: from settings, else first available.
    pub model: Option<Model>,
    /// Thinking level. Default: from settings, else `medium` (clamped).
    pub thinking_level: Option<ThinkingLevel>,
    /// Models available for cycling (`--models` scope).
    pub scoped_models: Vec<ScopedModel>,
    pub no_tools: Option<NoTools>,
    /// Tool allowlist (only these are enabled).
    pub tools: Option<Vec<String>>,
    /// Tool denylist, applied after `tools`.
    pub exclude_tools: Option<Vec<String>>,
    /// Custom tools to register (in addition to built-in tools).
    pub custom_tools: Vec<Arc<dyn AgentTool>>,
    /// Pre-built services (skips internal service creation; the services'
    /// model runtime is used unless `model_runtime` overrides it).
    pub services: Option<AgentSessionServices>,
    /// Session manager. Default: `SessionManager::create(cwd)`.
    pub session_manager: Option<Arc<Mutex<SessionManager>>>,
    /// Session start event metadata for extension runtime startup.
    pub session_start_event: Option<SessionStartEvent>,
    /// Loaded extension host (T15 W2). When present, the session's
    /// `ExtensionRunnerRef` is seeded with the host adapter instead of the
    /// no-op runner.
    pub extension_host: Option<Arc<pir_ext_host::host::NativeExtensionHost>>,
}

/// `CreateAgentSessionResult` (sdk.ts:88-95), T10 subset (no extensions
/// result until T15).
pub struct CreateAgentSessionResult {
    pub session: AgentSession,
    /// Warning if the session was restored with a different model than saved.
    pub model_fallback_message: Option<String>,
    /// The services passed in via `options.services`
    /// (`createAgentSessionFromServices`, agent-session-services.ts:200-219).
    pub services: Option<AgentSessionServices>,
}

/// `createAgentSession` (sdk.ts:169-398).
pub async fn create_agent_session(
    options: CreateAgentSessionOptions,
) -> Result<CreateAgentSessionResult, PirError> {
    let cwd = resolve_path(
        &options
            .cwd
            .clone()
            .or_else(|| {
                options.session_manager.as_ref().map(|manager| {
                    manager
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get_cwd()
                        .to_path_buf()
                })
            })
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")))
            .to_string_lossy(),
        &std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
    );
    let agent_dir = options
        .agent_dir
        .clone()
        .map(|dir| {
            resolve_path(
                &dir.to_string_lossy(),
                &std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            )
        })
        .unwrap_or_else(get_agent_dir);

    let provided_services = options.services;
    let (model_runtime, resource_loader) = match (options.model_runtime, provided_services.as_ref())
    {
        (Some(runtime), Some(services)) => (runtime, services.resource_loader.clone()),
        (None, Some(services)) => (
            services.model_runtime.clone(),
            services.resource_loader.clone(),
        ),
        (explicit_runtime, None) => {
            let runtime = match explicit_runtime {
                Some(runtime) => runtime,
                None => {
                    // Upstream only passes explicit paths when agentDir was
                    // given; ModelRuntime's defaults cover the default case
                    // (sdk.ts:174-176).
                    ModelRuntime::create(CreateModelRuntimeOptions {
                        credentials: None,
                        auth_path: options
                            .agent_dir
                            .as_ref()
                            .map(|_| agent_dir.join("auth.json")),
                        models_path: match options.agent_dir.as_ref() {
                            Some(_) => ModelsPathInput::Path(agent_dir.join("models.json")),
                            None => ModelsPathInput::Default,
                        },
                        ..Default::default()
                    })
                    .await
                }
            };
            let settings_manager = SettingsManager::create(
                &cwd,
                Some(&agent_dir),
                SettingsManagerCreateOptions::default(),
            );
            let services = create_agent_session_services(CreateAgentSessionServicesOptions {
                cwd: cwd.clone(),
                agent_dir: Some(agent_dir.clone()),
                settings_manager: Some(settings_manager),
                model_runtime: Some(runtime.clone()),
                extension_flag_values: Vec::new(),
                resource_loader_options: None,
            })
            .await?;
            (runtime, services.resource_loader)
        }
    };

    let session_manager = match options.session_manager {
        Some(session_manager) => session_manager,
        None => {
            let session_dir = get_default_session_dir_path(&cwd, Some(&agent_dir));
            Arc::new(Mutex::new(SessionManager::create(
                &cwd,
                Some(&session_dir),
                NewSessionOptions::default(),
            )?))
        }
    };

    // Check if the session has existing data to restore (sdk.ts:187-190).
    let (existing_messages, existing_model, existing_thinking_level, has_thinking_entry) = {
        let manager = session_manager.lock().unwrap_or_else(|e| e.into_inner());
        let context = manager.build_session_context();
        let has_thinking_entry = manager
            .get_branch(None)
            .iter()
            .any(|entry| entry.type_tag() == "thinking_level_change");
        (
            context.messages,
            context.model,
            context.thinking_level,
            has_thinking_entry,
        )
    };
    let has_existing_session = !existing_messages.is_empty();

    let mut model = options.model;
    let mut model_fallback_message: Option<String> = None;

    // If the session has data, try to restore the model from it. The whole
    // branch requires a saved model entry (sdk.ts:196 `existingSession.model`)
    // — a session with messages but no model_change entry falls through to
    // findInitialModel, like upstream.
    if model.is_none() && has_existing_session {
        if let Some(saved) = &existing_model {
            if let Some(restored) = model_runtime.get_model(&saved.provider, &saved.model_id) {
                if model_runtime.has_configured_auth(&restored.provider) {
                    model = Some(restored);
                }
            }
            if model.is_none() {
                model_fallback_message = Some(format!(
                    "Could not restore model {}/{}",
                    saved.provider, saved.model_id
                ));
            }
        }
    }

    // If still no model, use findInitialModel (sdk.ts:207-222).
    if model.is_none() {
        let (default_provider, default_model_id, default_thinking) = {
            let loader = resource_loader.lock().unwrap_or_else(|e| e.into_inner());
            let settings = loader.settings_manager();
            (
                settings.get_default_provider(),
                settings.get_default_model(),
                settings.get_default_thinking_level(),
            )
        };
        let result = find_initial_model(FindInitialModelOptions {
            cli_provider: None,
            cli_model: None,
            scoped_models: &[],
            is_continuing: has_existing_session,
            default_provider: default_provider.as_deref(),
            default_model_id: default_model_id.as_deref(),
            default_thinking_level: default_thinking,
            model_runtime: &model_runtime,
        })
        .await
        .map_err(PirError::Session)?;
        model = result.model;
        if model.is_none() {
            model_fallback_message = Some(format_no_models_available_message());
        } else if let (Some(message), Some(current)) =
            (model_fallback_message.take(), model.as_ref())
        {
            model_fallback_message = Some(format!(
                "{message}. Using {}/{}",
                current.provider, current.id
            ));
        }
    }

    // Thinking level (sdk.ts:224-243).
    let mut thinking_level = options.thinking_level;
    if thinking_level.is_none() && has_existing_session {
        thinking_level = if has_thinking_entry {
            Some(parse_thinking_level_str(&existing_thinking_level))
        } else {
            let default_level = {
                let loader = resource_loader.lock().unwrap_or_else(|e| e.into_inner());
                loader.settings_manager().get_default_thinking_level()
            };
            Some(default_level.unwrap_or(DEFAULT_THINKING_LEVEL))
        };
    }
    if thinking_level.is_none() {
        let default_level = {
            let loader = resource_loader.lock().unwrap_or_else(|e| e.into_inner());
            loader.settings_manager().get_default_thinking_level()
        };
        thinking_level = Some(default_level.unwrap_or(DEFAULT_THINKING_LEVEL));
    }
    let thinking_level = match &model {
        None => ThinkingLevel::Off,
        Some(model) => {
            pir_ai::models::clamp_thinking_level(model, thinking_level.expect("set above"))
        }
    };

    // Tool set (sdk.ts:245-251).
    let default_active_tool_names = ["read", "bash", "edit", "write"];
    let excluded: Vec<String> = options.exclude_tools.clone().unwrap_or_default();
    let initial_active_tool_names: Vec<String> = match &options.tools {
        Some(tools) => tools.clone(),
        None => match options.no_tools {
            Some(NoTools::All) | Some(NoTools::Builtin) => Vec::new(),
            None => default_active_tool_names.map(str::to_owned).to_vec(),
        },
    }
    .into_iter()
    .filter(|name| !excluded.contains(name))
    .collect();
    let allowed_tool_names: Option<Vec<String>> =
        options.tools.clone().or(match options.no_tools {
            Some(NoTools::All) => Some(Vec::new()),
            _ => None,
        });

    // convertToLlm with the blockImages filter (sdk.ts:256-290).
    let convert_to_llm = {
        let loader = resource_loader.clone();
        Arc::new(move |messages: Vec<AgentMessage>| {
            let loader = loader.clone();
            Box::pin(async move {
                let converted = pir_agent::convert_to_llm(&messages);
                let block_images = {
                    let loader = loader.lock().unwrap_or_else(|e| e.into_inner());
                    loader.settings_manager().get_block_images()
                };
                if !block_images {
                    return converted;
                }
                converted
                    .into_iter()
                    .map(|message| match message {
                        Message::User(mut user) => {
                            if let pir_ai::types::UserContent::Blocks(blocks) = &mut user.content {
                                filter_user_image_blocks(blocks);
                            }
                            Message::User(user)
                        }
                        Message::ToolResult(mut tool_result) => {
                            filter_tool_result_image_blocks(&mut tool_result.content);
                            Message::ToolResult(tool_result)
                        }
                        other => other,
                    })
                    .collect()
            }) as BoxFuture<'static, Vec<Message>>
        })
    };

    // Agent stream function (sdk.ts:302-330) with provider retry/timeout
    // settings and the extension header hook.
    let extension_runner: Arc<dyn crate::core::extensions::ExtensionRunner> =
        match &options.extension_host {
            Some(host) => Arc::new(
                crate::core::extension_host_adapter::ExtensionHostAdapter::new(host.clone()),
            ),
            None => NoopExtensionRunner::shared(),
        };
    let extension_runner_ref = new_extension_runner_ref(extension_runner);
    let stream_fn = {
        let model_runtime = model_runtime.clone();
        let loader = resource_loader.clone();
        let runner_ref = extension_runner_ref.clone();
        Arc::new(
            move |model: Model, context: pir_ai::types::Context, options: StreamOptions| {
                let model_runtime = model_runtime.clone();
                let loader = loader.clone();
                let runner_ref = runner_ref.clone();
                let (retry, http_idle_timeout_ms, websocket_connect_timeout_ms) = {
                    let loader = loader.lock().unwrap_or_else(|e| e.into_inner());
                    let settings = loader.settings_manager();
                    (
                        settings.get_provider_retry_settings(),
                        settings
                            .get_http_idle_timeout_ms()
                            .unwrap_or(crate::core::settings_manager::DEFAULT_HTTP_IDLE_TIMEOUT_MS),
                        settings.get_websocket_connect_timeout_ms().unwrap_or(None),
                    )
                };
                // SDKs treat timeout=0 as 0ms; use max int32 to effectively
                // disable (sdk.ts:305-308).
                let effective_timeout_ms = if http_idle_timeout_ms == 0 {
                    2_147_483_647
                } else {
                    http_idle_timeout_ms
                };
                let mut stream_options = options;
                stream_options.timeout_ms = stream_options
                    .timeout_ms
                    .or(retry.timeout_ms)
                    .or(Some(effective_timeout_ms));
                stream_options.websocket_connect_timeout_ms = stream_options
                    .websocket_connect_timeout_ms
                    .or(websocket_connect_timeout_ms);
                stream_options.max_retries = stream_options
                    .max_retries
                    .or(retry.max_retries.map(|r| r as u32));
                stream_options.max_retry_delay_ms = stream_options
                    .max_retry_delay_ms
                    .or(Some(retry.max_retry_delay_ms));
                // Upstream routes the agent path through `streamSimple`
                // (sdk.ts:36/312) so `reasoning` reaches the adapters'
                // thinking mapping; the pir `StreamFn` shape (design §4.4)
                // carries plain `StreamOptions`, so the conversion happens
                // here: `StreamOptions.reasoning` (bound by the agent loop
                // each turn) → `SimpleStreamOptions.reasoning`, plus the
                // settings `thinking_budgets` (the same source that seeds
                // `AgentOptions.thinking_budgets` below). The plain `stream`
                // path would drop both fields — adapter reasoning mapping
                // lives only in `stream_simple` (design §3.3), which is why
                // sessions recorded no thinking blocks despite a non-off
                // thinking level.
                let thinking_budgets = {
                    let loader = loader.lock().unwrap_or_else(|e| e.into_inner());
                    thinking_budgets_to_ai(loader.settings_manager().get_thinking_budgets())
                };
                let simple = pir_ai::types::SimpleStreamOptions {
                    reasoning: stream_options
                        .reasoning
                        .and_then(pir_ai::types::ThinkingLevel::from_model_level),
                    thinking_budgets,
                    stream: stream_options,
                };
                let stream_options_with_headers = pir_ai::models::ModelsSimpleStreamOptions {
                    simple,
                    transform_headers: Some(Arc::new(move |headers| {
                        let runner = crate::core::extensions::read_runner(&runner_ref);
                        Box::pin(async move {
                            if runner.has_handlers("before_provider_headers") {
                                runner.emit_before_provider_headers(headers).await
                            } else {
                                headers
                            }
                        })
                            as BoxFuture<'static, pir_ai::types::ProviderHeaders>
                    })),
                };
                Box::pin(model_runtime.stream_simple(
                    &model,
                    &context,
                    Some(stream_options_with_headers),
                )) as pir_agent::BoxStream<'static, pir_ai::types::StreamEvent>
            },
        )
    };

    let session_id = session_manager
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_session_id()
        .to_owned();
    let (steering_mode, follow_up_mode, transport, thinking_budgets, max_retry_delay_ms) = {
        let loader = resource_loader.lock().unwrap_or_else(|e| e.into_inner());
        let settings = loader.settings_manager();
        (
            settings.get_steering_mode(),
            settings.get_follow_up_mode(),
            settings.get_transport(),
            settings.get_thinking_budgets(),
            settings.get_provider_retry_settings().max_retry_delay_ms,
        )
    };

    let runner_ref_for_hooks = extension_runner_ref.clone();
    let mut agent_options = AgentOptions::new(stream_fn);
    agent_options.initial_state = InitialAgentState {
        system_prompt: Some(String::new()),
        model: model.clone(),
        thinking_level: Some(thinking_level),
        tools: Some(Vec::new()),
        messages: None,
    };
    agent_options.convert_to_llm = Some(convert_to_llm);
    agent_options.transform_context = Some(Arc::new(move |messages, _signal| {
        let runner = crate::core::extensions::read_runner(&runner_ref_for_hooks);
        Box::pin(async move { runner.emit_context(messages).await })
            as BoxFuture<'static, Vec<AgentMessage>>
    }));
    agent_options.on_payload = Some(Arc::new({
        let runner_ref = extension_runner_ref.clone();
        move |payload, _model| {
            let runner = crate::core::extensions::read_runner(&runner_ref);
            Box::pin(async move {
                if runner.has_handlers("before_provider_request") {
                    Some(runner.emit_before_provider_request(payload).await)
                } else {
                    Some(payload)
                }
            })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = Option<serde_json::Value>> + Send>,
                >
        }
    }));
    agent_options.session_id = Some(session_id);
    agent_options.steering_mode = Some(steering_mode);
    agent_options.follow_up_mode = Some(follow_up_mode);
    agent_options.transport = Some(transport);
    agent_options.thinking_budgets = thinking_budgets_to_ai(thinking_budgets);
    agent_options.max_retry_delay_ms = Some(max_retry_delay_ms);
    // Tool interception + provider response hooks read the runner at
    // execution time (agent-session.ts:459-462 `_installAgentToolHooks`), so
    // a runner swap needs no reinstall.
    agent_options.before_tool_call = Some(
        crate::core::extensions::extension_before_tool_call_hook(extension_runner_ref.clone()),
    );
    agent_options.after_tool_call = Some(crate::core::extensions::extension_after_tool_call_hook(
        extension_runner_ref.clone(),
    ));
    agent_options.on_response = Some(crate::core::extensions::extension_on_response_callback(
        extension_runner_ref.clone(),
    ));

    let agent = Arc::new(Agent::new(agent_options));

    // Restore messages / save initial entries (sdk.ts:362-374).
    if has_existing_session {
        agent.set_messages(existing_messages);
        if !has_thinking_entry {
            let result = session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .append_thinking_level_change(thinking_level_str(thinking_level));
            if let Err(error) = result {
                tracing::warn!("session append failed: {error}");
            }
        }
    } else {
        if let Some(model) = &model {
            let result = session_manager
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .append_model_change(&model.provider, &model.id);
            if let Err(error) = result {
                tracing::warn!("session append failed: {error}");
            }
        }
        let result = session_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append_thinking_level_change(thinking_level_str(thinking_level));
        if let Err(error) = result {
            tracing::warn!("session append failed: {error}");
        }
    }

    let session = AgentSession::new(AgentSessionConfig {
        agent,
        session_manager,
        cwd: cwd.to_string_lossy().into_owned(),
        scoped_models: options.scoped_models,
        resource_loader,
        custom_tools: options.custom_tools,
        model_runtime,
        initial_active_tool_names: Some(initial_active_tool_names),
        allowed_tool_names,
        excluded_tool_names: options.exclude_tools,
        extension_runner_ref,
        session_start_event: options.session_start_event.unwrap_or(SessionStartEvent {
            reason: crate::core::extensions::SessionStartReason::Startup,
            previous_session_file: None,
        }),
    });

    Ok(CreateAgentSessionResult {
        session,
        model_fallback_message,
        services: provided_services,
    })
}

/// `Image reading is disabled.` placeholder (sdk.ts:271).
const IMAGE_PLACEHOLDER: &str = "Image reading is disabled.";

/// Filter image blocks out of user content, replacing them with a deduped
/// text placeholder (sdk.ts:262-289).
fn filter_user_image_blocks(blocks: &mut Vec<pir_ai::types::UserContentBlock>) {
    use pir_ai::types::UserContentBlock;
    let mut result: Vec<UserContentBlock> = Vec::with_capacity(blocks.len());
    for block in std::mem::take(blocks) {
        let block = match block {
            UserContentBlock::Image(_) => UserContentBlock::Text(TextContent {
                text: IMAGE_PLACEHOLDER.to_owned(),
                text_signature: None,
            }),
            other => other,
        };
        let is_placeholder =
            matches!(&block, UserContentBlock::Text(t) if t.text == IMAGE_PLACEHOLDER);
        let last_placeholder =
            matches!(result.last(), Some(UserContentBlock::Text(t)) if t.text == IMAGE_PLACEHOLDER);
        if !(is_placeholder && last_placeholder) {
            result.push(block);
        }
    }
    *blocks = result;
}

/// Tool-result variant of [`filter_user_image_blocks`].
fn filter_tool_result_image_blocks(blocks: &mut Vec<pir_ai::types::ToolResultContent>) {
    use pir_ai::types::ToolResultContent;
    let mut result: Vec<ToolResultContent> = Vec::with_capacity(blocks.len());
    for block in std::mem::take(blocks) {
        let block = match block {
            ToolResultContent::Image(_) => ToolResultContent::Text(TextContent {
                text: IMAGE_PLACEHOLDER.to_owned(),
                text_signature: None,
            }),
            other => other,
        };
        let is_placeholder =
            matches!(&block, ToolResultContent::Text(t) if t.text == IMAGE_PLACEHOLDER);
        let last_placeholder = matches!(result.last(), Some(ToolResultContent::Text(t)) if t.text == IMAGE_PLACEHOLDER);
        if !(is_placeholder && last_placeholder) {
            result.push(block);
        }
    }
    *blocks = result;
}

fn parse_thinking_level_str(level: &str) -> ThinkingLevel {
    crate::cli::args::parse_thinking_level(level).unwrap_or(DEFAULT_THINKING_LEVEL)
}

/// Settings `ThinkingBudgetsSettings` → pi-ai `ThinkingBudgets`. Upstream
/// passes the settings value through `Agent.thinkingBudgets` into
/// `SimpleStreamOptions.thinkingBudgets` (sdk.ts); the agent path here uses
/// the same source both for `AgentOptions` and for the per-call
/// `stream_simple` conversion.
fn thinking_budgets_to_ai(
    budgets: Option<crate::core::settings_manager::ThinkingBudgetsSettings>,
) -> Option<pir_ai::types::ThinkingBudgets> {
    budgets.map(|budgets| pir_ai::types::ThinkingBudgets {
        minimal: budgets.minimal.map(|v| v as u32),
        low: budgets.low.map(|v| v as u32),
        medium: budgets.medium.map(|v| v as u32),
        high: budgets.high.map(|v| v as u32),
    })
}

fn thinking_level_str(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::Xhigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}
