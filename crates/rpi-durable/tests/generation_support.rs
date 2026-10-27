#![allow(dead_code)]

//! Shared support for the generation/tool integration tests (V17-14, worker 3b).
//!
//! rpi-ai ships no faux provider, so this file builds a scripted [`Provider`]
//! test double registered on a [`Models`] handle through
//! [`Models::set_provider`]. Each `stream_simple` call pops the next scripted
//! reply. The Harness is opened over memory storage with the **real** built-in
//! task definitions ([`builtin_tasks`]).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rpi_ai::auth::{
    ApiKeyAuth, ApiKeyCredential, AuthContext, AuthResult, ModelAuth, ProviderAuth,
};
use rpi_ai::models::{Models, Provider};
use rpi_ai::types::{
    ApiKind, AssistantMessage, DoneReason, ErrorReason, InputModality, Model, ModelCost,
    SimpleStreamOptions, StopReason, StreamEvent, StreamOptions, TranscriptContext,
};
use rpi_ai::utils::event_stream::AssistantMessageEventStream;
use rpi_durable::chord::context::{BACKGROUND_CONTEXT, Context};
use rpi_durable::harness::{
    AgentChange, AnyTask, Conversation, ConversationAbortOptions, Extension, HarnessImpl,
    HarnessOptions, HarnessSettings, InputSubmissionDraft, ModelRef, Registry, RegistryImpl,
    Submission, SubmissionDraft, builtin_tasks, open_harness,
};
use rpi_durable::storage::MemoryStorage;
use rpi_durable::types::Storage;

/// One scripted provider response.
#[allow(clippy::large_enum_variant)]
pub enum Script {
    /// A terminal assistant message pushed as `start` + `done`/`error`.
    Message(AssistantMessage),
    /// A partial that never terminates (drives abort).
    Pending(String),
}

/// An api-key auth that always resolves, so `Models::stream_simple` reaches the
/// provider without credentials.
struct AlwaysAuth;

#[async_trait]
impl ApiKeyAuth for AlwaysAuth {
    fn name(&self) -> &str {
        "always"
    }

    async fn resolve(
        &self,
        _context: &dyn AuthContext,
        _credential: Option<&ApiKeyCredential>,
    ) -> Result<Option<AuthResult>, rpi_ai::auth::ModelsError> {
        Ok(Some(AuthResult {
            auth: ModelAuth {
                api_key: None,
                headers: None,
                base_url: None,
            },
            env: None,
            source: None,
        }))
    }
}

/// Identity values observed at the provider boundary.
pub type IdentityLog = Arc<Mutex<Vec<Option<rpi_ai::utils::identity::ClientIdentity>>>>;

/// A provider that returns scripted responses in order.
pub struct ScriptedProvider {
    model: Model,
    auth: ProviderAuth,
    scripts: Mutex<VecDeque<Script>>,
    identity_log: Option<IdentityLog>,
}

impl ScriptedProvider {
    fn next(&self) -> Script {
        self.scripts
            .lock()
            .ok()
            .and_then(|mut scripts| scripts.pop_front())
            .unwrap_or_else(|| {
                Script::Message(error_message("no scripted reply", "no scripted reply"))
            })
    }

    fn record_identity(&self, identity: Option<rpi_ai::utils::identity::ClientIdentity>) {
        if let Some(log) = &self.identity_log {
            log.lock().expect("identity log").push(identity);
        }
    }

    fn stream_of(&self, script: Script) -> AssistantMessageEventStream {
        let stream = AssistantMessageEventStream::new();
        match script {
            Script::Message(message) => {
                stream.push(StreamEvent::Start {
                    partial: Arc::new(partial(&message)),
                });
                match message.stop_reason {
                    StopReason::Error => stream.push(StreamEvent::Error {
                        reason: ErrorReason::Error,
                        error: message,
                    }),
                    StopReason::Deferred => stream.push(StreamEvent::Done {
                        reason: DoneReason::Deferred,
                        message,
                    }),
                    StopReason::Length => stream.push(StreamEvent::Done {
                        reason: DoneReason::Length,
                        message,
                    }),
                    StopReason::ToolUse => stream.push(StreamEvent::Done {
                        reason: DoneReason::ToolUse,
                        message,
                    }),
                    _ => stream.push(StreamEvent::Done {
                        reason: DoneReason::Stop,
                        message,
                    }),
                }
            }
            Script::Pending(text) => {
                let message = text_message(&text, StopReason::Pending);
                stream.push(StreamEvent::Start {
                    partial: Arc::new(message.clone()),
                });
                stream.push(StreamEvent::TextDelta {
                    content_index: 0,
                    delta: text,
                    partial: Arc::new(message),
                });
                // Never finish.
            }
        }
        stream
    }
}

impl Provider for ScriptedProvider {
    fn id(&self) -> &str {
        "test"
    }

    fn name(&self) -> &str {
        "Test"
    }

    fn base_url(&self) -> Option<&str> {
        Some("http://localhost")
    }

    fn headers(&self) -> Option<&rpi_ai::types::ProviderHeaders> {
        None
    }

    fn auth(&self) -> &ProviderAuth {
        &self.auth
    }

    fn get_models(&self) -> Vec<Model> {
        vec![self.model.clone()]
    }

    fn stream(
        &self,
        _model: &Model,
        _context: &TranscriptContext,
        options: Option<StreamOptions>,
    ) -> AssistantMessageEventStream {
        self.record_identity(options.and_then(|options| options.identity.clone()));
        self.stream_of(self.next())
    }

    fn stream_simple(
        &self,
        _model: &Model,
        _context: &TranscriptContext,
        options: Option<SimpleStreamOptions>,
    ) -> Result<AssistantMessageEventStream, String> {
        self.record_identity(options.and_then(|options| options.stream.identity.clone()));
        Ok(self.stream_of(self.next()))
    }
}

/// The scripted test model.
pub fn model() -> Model {
    Model {
        id: "test-model".to_string(),
        name: "Test Model".to_string(),
        api: ApiKind::from("openai"),
        provider: "test".to_string(),
        base_url: "http://localhost".to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![InputModality::Text],
        input_limits: None,
        cost: ModelCost::default(),
        prompt_cache: None,
        context_window: 200_000,
        max_tokens: 4_096,
        sampling_params: None,
        sampling_params_by_thinking_level: None,
        headers: None,
        compat: None,
    }
}

/// Build a `Models` handle with one scripted provider.
pub fn test_models(scripts: Vec<Script>) -> Models {
    test_models_with_identity_log(scripts).0
}

/// Build a `Models` handle and capture identity at each provider call.
pub fn test_models_with_identity_log(scripts: Vec<Script>) -> (Models, IdentityLog) {
    let identity_log = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(ScriptedProvider {
        model: model(),
        auth: ProviderAuth {
            api_key: Some(Arc::new(AlwaysAuth)),
            oauth: None,
        },
        scripts: Mutex::new(scripts.into()),
        identity_log: Some(Arc::clone(&identity_log)),
    });
    let models = Models::new(None);
    models.set_provider(provider);
    (models, identity_log)
}

/// The shared background context.
pub fn context() -> Context {
    BACKGROUND_CONTEXT.clone()
}

/// A captured harness report log.
#[derive(Clone, Default)]
pub struct Reports(Arc<Mutex<Vec<String>>>);

impl Reports {
    /// Every reported message so far.
    pub fn entries(&self) -> Vec<String> {
        self.0.lock().map(|log| log.clone()).unwrap_or_default()
    }
}

/// An opened Harness with its registry and report log.
pub struct Opened {
    pub harness: Arc<HarnessImpl>,
    pub registry: Arc<RegistryImpl>,
    pub storage: Arc<dyn Storage>,
    pub reports: Reports,
}

/// Open a Harness with the real built-ins, optional extra extensions, and settings.
pub async fn open(
    extensions: Vec<Extension>,
    settings: Option<HarnessSettings>,
    models: Models,
) -> Opened {
    open_with_storage(Arc::new(MemoryStorage::new()), extensions, settings, models).await
}

/// Open a Harness with explicit request identity.
pub async fn open_with_identity(
    extensions: Vec<Extension>,
    settings: Option<HarnessSettings>,
    models: Models,
    identity: Option<rpi_ai::utils::identity::ClientIdentity>,
) -> Opened {
    open_with_storage_and_identity(
        Arc::new(MemoryStorage::new()),
        extensions,
        settings,
        models,
        identity,
    )
    .await
}

/// Open a Harness over `storage`.
pub async fn open_with_storage(
    storage: Arc<dyn Storage>,
    extensions: Vec<Extension>,
    settings: Option<HarnessSettings>,
    models: Models,
) -> Opened {
    open_with_storage_and_identity(storage, extensions, settings, models, None).await
}

/// Open a Harness over `storage` with explicit request identity.
pub async fn open_with_storage_and_identity(
    storage: Arc<dyn Storage>,
    extensions: Vec<Extension>,
    settings: Option<HarnessSettings>,
    models: Models,
    identity: Option<rpi_ai::utils::identity::ClientIdentity>,
) -> Opened {
    let registry = Arc::new(RegistryImpl::with_builtins(builtin_tasks()));
    for extension in extensions {
        registry.install(extension).expect("install extension");
    }
    let reports = Reports::default();
    let captured = reports.clone();
    let options = HarnessOptions {
        models,
        identity,
        registry: Arc::clone(&registry) as Arc<dyn rpi_durable::harness::RegistryReader>,
        settings,
        env: None,
        conversation_created: None,
        now: None,
        on_report: Some(Arc::new(move |message| {
            if let Ok(mut log) = captured.0.lock() {
                log.push(message);
            }
        })),
    };
    let harness = open_harness(Arc::clone(&storage), options, &context())
        .await
        .expect("open harness");
    Opened {
        harness,
        registry,
        storage,
        reports,
    }
}

/// Create the root conversation without selecting a model.
pub async fn root_only(harness: &Arc<HarnessImpl>) -> Arc<dyn Conversation> {
    let conversation = harness
        .root(&context(), Default::default())
        .await
        .expect("root");
    let conversation: Arc<dyn Conversation> = conversation;
    conversation
}

/// Configure the conversation's agent to use the scripted model.
pub async fn select_model(conversation: &Arc<dyn Conversation>) {
    conversation
        .configure(
            AgentChange {
                model: Some(Some(ModelRef {
                    provider: "test".to_string(),
                    model_id: "test-model".to_string(),
                })),
                ..AgentChange::default()
            },
            &context(),
        )
        .await
        .expect("configure model");
}

/// Create the root conversation and select the scripted model.
pub async fn root_ready(harness: &Arc<HarnessImpl>) -> Arc<dyn Conversation> {
    let conversation = root_only(harness).await;
    select_model(&conversation).await;
    conversation
}

/// Submit one user input and return its submission handle.
pub async fn submit_input(
    conversation: &Arc<dyn Conversation>,
    content: &str,
) -> Arc<dyn Submission> {
    conversation
        .submit(
            SubmissionDraft::Input(InputSubmissionDraft {
                request_id: None,
                content: rpi_ai::types::UserContent::Text(content.to_string()),
                when_busy: None,
            }),
            &context(),
        )
        .await
        .expect("submit")
}

/// Read a conversation's committed entries.
pub async fn entries(conversation: &Arc<dyn Conversation>) -> Vec<rpi_durable::types::EntryRecord> {
    conversation
        .entries(
            rpi_durable::types::EntryQuery {
                conversation_id: conversation.id(),
                min_entry_id: None,
                max_entry_id: None,
                order: Some(rpi_durable::types::ScanOrder::Ascending),
            },
            200,
            None,
            &context(),
        )
        .await
        .expect("entries")
        .items
}

// ---------------------------------------------------------------------------
// Message builders
// ---------------------------------------------------------------------------

fn base_message(stop_reason: &str) -> serde_json::Value {
    serde_json::json!({
        "role": "assistant",
        "content": [],
        "api": "openai",
        "provider": "test",
        "model": "test-model",
        "usage": {
            "input": 3, "output": 2, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 5,
            "cost": { "input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": 0.0 }
        },
        "stopReason": stop_reason,
        "timestamp": 1
    })
}

/// An assistant message with `text` and a `stop` reason.
pub fn text_message(text: &str, stop: StopReason) -> AssistantMessage {
    let mut value = base_message(stop_reason_name(stop));
    value["content"] = serde_json::json!([{ "type": "text", "text": text }]);
    serde_json::from_value(value).expect("assistant message")
}

/// An assistant message that failed with `message`.
pub fn error_message(message: &str, error: &str) -> AssistantMessage {
    let mut value = base_message("error");
    value["errorMessage"] = serde_json::Value::String(error.to_string());
    let _ = message;
    serde_json::from_value(value).expect("assistant message")
}

/// An assistant message that calls `name` with `arguments`.
pub fn tool_use_message(name: &str, id: &str, arguments: serde_json::Value) -> AssistantMessage {
    let mut value = base_message("toolUse");
    value["content"] =
        serde_json::json!([{ "type": "toolCall", "id": id, "name": name, "arguments": arguments }]);
    serde_json::from_value(value).expect("assistant message")
}

fn stop_reason_name(stop: StopReason) -> &'static str {
    match stop {
        StopReason::Stop => "stop",
        StopReason::Length => "length",
        StopReason::ToolUse => "toolUse",
        StopReason::Error => "error",
        StopReason::Aborted => "aborted",
        StopReason::Deferred => "deferred",
        StopReason::Pending => "pending",
    }
}

/// A partial as it appears in an opening `start` event: the terminal content of
/// `message` with its `stop` reason.
fn partial(message: &AssistantMessage) -> AssistantMessage {
    let mut value = serde_json::to_value(message).unwrap_or(serde_json::Value::Null);
    value["stopReason"] = serde_json::Value::String("pending".to_string());
    serde_json::from_value(value).unwrap_or_else(|_| message.clone())
}

/// The abort-conversation options that cross background boundaries.
pub fn abort_options() -> ConversationAbortOptions {
    ConversationAbortOptions { background: false }
}

/// A boxed built-in task list (for callers that need the type).
pub fn all_builtins() -> Vec<AnyTask> {
    builtin_tasks()
}
