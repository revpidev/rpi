//! Port of `external/pi/packages/agent/test/harness/agent-harness-stream.test.ts`
//! @ pi 0.82.1 (2efa728) — `AgentHarness` stream/streamOptions behavior.
//!
//! Structural mapping notes:
//! - One intentional divergence, inherited from the type layer (types.rs
//!   `AgentHarnessStreamOptionsPatch` doc): upstream's explicit `undefined`
//!   for a *scalar* patch field (`timeoutMs: undefined` clearing the value,
//!   agent-harness.ts:101-105 via `Object.hasOwn`) is not expressible in the
//!   Rust patch type, so the second test skips the scalar-clear step and
//!   asserts the value survives unchanged. Map clearing (`metadata:
//!   undefined` → `PatchMap::Clear`) and per-key deletion are fully covered.
//! - The payload-chaining test registers a probe `Provider` that invokes the
//!   `onPayload` callback like a real provider would (the faux provider only
//!   invokes `onResponse`).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;
use rpi_agent::error::AgentError;
use rpi_agent::harness::session::memory_storage::{
    InMemorySessionStorage, InMemorySessionStorageOptions,
};
use rpi_agent::harness::session::Session as SessionFacade;
use rpi_agent::harness::types::{
    AgentHarnessOptions, AgentHarnessResources, AgentHarnessStreamOptions,
    AgentHarnessStreamOptionsPatch, BeforeProviderPayloadResult, BeforeProviderRequestResult,
    PatchMap, SessionContextBuildOptions, SessionMetadata,
};
use rpi_agent::harness::{
    AgentHarness, AgentHarnessError, AgentHarnessEvent, AgentHarnessHook, AgentHarnessListener,
    AgentHarnessOwnEvent, HarnessHookResult, Session,
};
use rpi_agent::types::{AgentEvent, AgentToolResult, AgentToolUpdateCallback};
use rpi_ai::auth::{
    ApiKeyAuth, ApiKeyCredential, AuthContext, AuthResult, ModelAuth, ModelsError, ProviderAuth,
};
use rpi_ai::models::{Models, Provider};
use rpi_ai::types::{
    CacheRetention, Context, Model, ProviderHeaders, SimpleStreamOptions, StreamOptions,
    TextContent, ToolResultContent,
};
use rpi_ai::utils::event_stream::AssistantMessageEventStream;
use rpi_test_support::faux::{
    faux_assistant_message, faux_tool_call, FauxAiProvider, FauxProvider, FauxProviderOptions,
    FauxResponseStep,
};
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn new_faux() -> (Models, Arc<FauxProvider>) {
    let faux = FauxProvider::new(FauxProviderOptions::default());
    let models = Models::new(None);
    models.set_provider(Arc::new(FauxAiProvider::new(Arc::clone(&faux))));
    (models, faux)
}

fn new_session() -> Arc<SessionFacade<SessionMetadata>> {
    Arc::new(SessionFacade::new(
        Arc::new(
            InMemorySessionStorage::new(InMemorySessionStorageOptions::default())
                .expect("in-memory storage"),
        ),
        SessionContextBuildOptions::default(),
    ))
}

fn as_dyn_session(
    session: &Arc<SessionFacade<SessionMetadata>>,
) -> Arc<dyn Session<Metadata = SessionMetadata>> {
    session.clone()
}

fn base_options(
    models: &Models,
    session: Arc<dyn Session<Metadata = SessionMetadata>>,
    model: Model,
) -> AgentHarnessOptions<()> {
    AgentHarnessOptions {
        session,
        models: models.clone(),
        tools: Vec::new(),
        resources: AgentHarnessResources::default(),
        system_prompt: None,
        stream_options: None,
        retry: None,
        model,
        thinking_level: None,
        active_tool_names: None,
        steering_mode: None,
        follow_up_mode: None,
        tool_context: None,
    }
}

fn listener<F, Fut>(f: F) -> AgentHarnessListener
where
    F: Fn(AgentHarnessEvent, CancellationToken) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<(), AgentHarnessError>> + Send + 'static,
{
    Arc::new(move |event, signal| Box::pin(f(event, signal)))
}

fn hook<F, Fut>(f: F) -> AgentHarnessHook
where
    F: Fn(AgentHarnessOwnEvent) -> Fut + Send + Sync + 'static,
    Fut:
        std::future::Future<Output = Result<HarnessHookResult, AgentHarnessError>> + Send + 'static,
{
    Arc::new(move |event| Box::pin(f(event)))
}

fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

fn metadata(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

/// `calculateTool` (test/utils/calculate.ts) — minimal port.
struct CalculateTool {
    parameters: Value,
}

#[async_trait]
impl rpi_agent::harness::AgentHarnessTool<()> for CalculateTool {
    fn name(&self) -> &str {
        "calculate"
    }

    fn label(&self) -> &str {
        "Calculator"
    }

    fn description(&self) -> &str {
        "Evaluate mathematical expressions"
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: Value,
        _signal: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
        _context: (),
    ) -> Result<AgentToolResult, AgentError> {
        Ok(AgentToolResult {
            content: vec![ToolResultContent::Text(TextContent {
                text: "2".to_owned(),
                text_signature: None,
            })],
            details: Value::Null,
            usage: None,
            added_tool_names: None,
            terminate: None,
        })
    }
}

fn calculate_tool() -> Arc<CalculateTool> {
    Arc::new(CalculateTool {
        parameters: json!({
            "type": "object",
            "properties": {
                "expression": {
                    "type": "string",
                    "description": "The mathematical expression to evaluate"
                }
            },
            "required": ["expression"]
        }),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// "snapshots stream options before provider request hooks" (:39-86).
#[tokio::test]
async fn test_stream_options_snapshot_before_provider_request_hooks() {
    let (models, faux) = new_faux();
    let captured: Arc<Mutex<Option<StreamOptions>>> = Arc::new(Mutex::new(None));
    faux.set_responses(vec![FauxResponseStep::Factory({
        let captured = Arc::clone(&captured);
        Box::new(move |_context, options, _state, _model| {
            *captured.lock().expect("captured") = options.cloned();
            faux_assistant_message("ok", Default::default())
        })
    })]);

    let session = Arc::new(SessionFacade::new(
        Arc::new(
            InMemorySessionStorage::new(InMemorySessionStorageOptions {
                entries: None,
                metadata: Some(SessionMetadata {
                    id: "session-1".to_owned(),
                    created_at: "now".to_owned(),
                }),
            })
            .expect("storage"),
        ),
        SessionContextBuildOptions::default(),
    ));
    let harness = Arc::new(
        AgentHarness::new(AgentHarnessOptions {
            stream_options: Some(AgentHarnessStreamOptions {
                transport: None,
                timeout_ms: Some(1000),
                max_retries: Some(2),
                max_retry_delay_ms: Some(3000),
                headers: Some(headers(&[("x-base", "base")])),
                metadata: Some(metadata(&[("base", json!(true))])),
                cache_retention: Some(CacheRetention::None),
            }),
            ..base_options(
                &models,
                as_dyn_session(&session),
                faux.get_model(None).expect("faux model"),
            )
        })
        .expect("harness"),
    );

    let _unsubscribe = harness.on(
        "before_provider_request",
        hook(|event| async move {
            let AgentHarnessOwnEvent::BeforeProviderRequest {
                session_id,
                stream_options,
                ..
            } = &event
            else {
                return Ok(HarnessHookResult::BeforeProviderRequest(None));
            };
            assert_eq!(session_id, "session-1");
            assert_eq!(stream_options.headers, Some(headers(&[("x-base", "base")])));
            Ok(HarnessHookResult::BeforeProviderRequest(Some(
                BeforeProviderRequestResult {
                    stream_options: Some(AgentHarnessStreamOptionsPatch {
                        headers: PatchMap::Merge(BTreeMap::from([(
                            "x-hook".to_owned(),
                            Some("hook".to_owned()),
                        )])),
                        metadata: PatchMap::Merge(BTreeMap::from([(
                            "hook".to_owned(),
                            Some(json!(true)),
                        )])),
                        ..Default::default()
                    }),
                },
            )))
        }),
    );

    harness.prompt("hello", None).await.expect("prompt");

    let captured = captured.lock().expect("captured").clone().expect("options");
    assert_eq!(captured.timeout_ms, Some(1000));
    assert_eq!(captured.max_retries, Some(2));
    assert_eq!(captured.max_retry_delay_ms, Some(3000));
    assert_eq!(captured.session_id.as_deref(), Some("session-1"));
    assert_eq!(captured.cache_retention, Some(CacheRetention::None));
    let captured_headers: ProviderHeaders = captured.request.headers.expect("headers");
    assert_eq!(
        captured_headers,
        HashMap::from([
            ("x-base".to_owned(), Some("base".to_owned())),
            ("x-hook".to_owned(), Some("hook".to_owned())),
        ])
    );
    assert_eq!(
        captured.metadata.expect("metadata"),
        Map::from_iter([
            ("base".to_owned(), json!(true)),
            ("hook".to_owned(), json!(true))
        ])
    );
}

/// "chains provider request patches and supports deletion semantics"
/// (:88-137). Divergence: the upstream second patch clears `timeoutMs` via an
/// explicit `undefined` — inexpressible in the Rust patch type (see the file
/// header), so the timeout is asserted to survive unchanged instead.
#[tokio::test]
async fn test_provider_request_patch_chaining_and_deletion() {
    let (models, faux) = new_faux();
    let captured: Arc<Mutex<Option<StreamOptions>>> = Arc::new(Mutex::new(None));
    faux.set_responses(vec![FauxResponseStep::Factory({
        let captured = Arc::clone(&captured);
        Box::new(move |_context, options, _state, _model| {
            *captured.lock().expect("captured") = options.cloned();
            faux_assistant_message("ok", Default::default())
        })
    })]);

    let harness = Arc::new(
        AgentHarness::new(AgentHarnessOptions {
            stream_options: Some(AgentHarnessStreamOptions {
                transport: None,
                timeout_ms: Some(1000),
                max_retries: Some(2),
                max_retry_delay_ms: None,
                headers: Some(headers(&[("keep", "base"), ("remove", "base")])),
                metadata: Some(metadata(&[
                    ("keep", json!("base")),
                    ("remove", json!("base")),
                ])),
                cache_retention: None,
            }),
            ..base_options(
                &models,
                as_dyn_session(&new_session()),
                faux.get_model(None).expect("faux model"),
            )
        })
        .expect("harness"),
    );

    let _first = harness.on(
        "before_provider_request",
        hook(|event| async move {
            let AgentHarnessOwnEvent::BeforeProviderRequest { stream_options, .. } = &event else {
                return Ok(HarnessHookResult::BeforeProviderRequest(None));
            };
            assert_eq!(
                stream_options.headers,
                Some(headers(&[("keep", "base"), ("remove", "base")]))
            );
            Ok(HarnessHookResult::BeforeProviderRequest(Some(
                BeforeProviderRequestResult {
                    stream_options: Some(AgentHarnessStreamOptionsPatch {
                        headers: PatchMap::Merge(BTreeMap::from([
                            ("first".to_owned(), Some("1".to_owned())),
                            ("remove".to_owned(), None),
                        ])),
                        metadata: PatchMap::Merge(BTreeMap::from([
                            ("first".to_owned(), Some(json!(1))),
                            ("remove".to_owned(), None),
                        ])),
                        ..Default::default()
                    }),
                },
            )))
        }),
    );
    let _second = harness.on(
        "before_provider_request",
        hook(|event| async move {
            let AgentHarnessOwnEvent::BeforeProviderRequest { stream_options, .. } = &event else {
                return Ok(HarnessHookResult::BeforeProviderRequest(None));
            };
            assert_eq!(
                stream_options.headers,
                Some(headers(&[("keep", "base"), ("first", "1")]))
            );
            assert_eq!(
                stream_options.metadata,
                Some(metadata(&[("keep", json!("base")), ("first", json!(1))]))
            );
            Ok(HarnessHookResult::BeforeProviderRequest(Some(
                BeforeProviderRequestResult {
                    stream_options: Some(AgentHarnessStreamOptionsPatch {
                        headers: PatchMap::Merge(BTreeMap::from([(
                            "second".to_owned(),
                            Some("2".to_owned()),
                        )])),
                        // Upstream `metadata: undefined` — clear the whole map.
                        metadata: PatchMap::Clear,
                        ..Default::default()
                    }),
                },
            )))
        }),
    );

    harness.prompt("hello", None).await.expect("prompt");

    let captured = captured.lock().expect("captured").clone().expect("options");
    // Upstream clears `timeoutMs` here; the Rust patch cannot express
    // scalar clears, so it stays at the base value (file header note).
    assert_eq!(captured.timeout_ms, Some(1000));
    assert_eq!(captured.max_retries, Some(2));
    let captured_headers: ProviderHeaders = captured.request.headers.expect("headers");
    assert_eq!(
        captured_headers,
        HashMap::from([
            ("keep".to_owned(), Some("base".to_owned())),
            ("first".to_owned(), Some("1".to_owned())),
            ("second".to_owned(), Some("2".to_owned())),
        ])
    );
    assert_eq!(captured.metadata, None);
}

/// "uses updated stream options for save-point snapshots without mutating the
/// active request" (:139-176).
#[tokio::test]
async fn test_save_point_stream_options_do_not_mutate_active_request() {
    let (models, faux) = new_faux();
    let captured: Arc<Mutex<Vec<StreamOptions>>> = Arc::new(Mutex::new(Vec::new()));
    faux.set_responses(vec![
        FauxResponseStep::Factory({
            let captured = Arc::clone(&captured);
            Box::new(move |_context, options, _state, _model| {
                captured
                    .lock()
                    .expect("captured")
                    .push(options.cloned().expect("options"));
                faux_assistant_message(
                    vec![faux_tool_call(
                        "calculate",
                        json!({ "expression": "1 + 1" })
                            .as_object()
                            .expect("object")
                            .clone(),
                        Some("call-1".to_owned()),
                    )],
                    rpi_test_support::faux::FauxAssistantOptions {
                        stop_reason: Some(rpi_ai::types::StopReason::ToolUse),
                        ..Default::default()
                    },
                )
            })
        }),
        FauxResponseStep::Factory({
            let captured = Arc::clone(&captured);
            Box::new(move |_context, options, _state, _model| {
                captured
                    .lock()
                    .expect("captured")
                    .push(options.cloned().expect("options"));
                faux_assistant_message("done", Default::default())
            })
        }),
    ]);

    let harness = Arc::new(
        AgentHarness::new(AgentHarnessOptions {
            tools: vec![calculate_tool()],
            stream_options: Some(AgentHarnessStreamOptions {
                timeout_ms: Some(1000),
                headers: Some(headers(&[("turn", "first")])),
                ..Default::default()
            }),
            ..base_options(
                &models,
                as_dyn_session(&new_session()),
                faux.get_model(None).expect("faux model"),
            )
        })
        .expect("harness"),
    );

    let _unsubscribe = harness.subscribe({
        let harness = Arc::clone(&harness);
        listener(move |event, _signal| {
            let harness = Arc::clone(&harness);
            async move {
                if matches!(
                    event,
                    AgentHarnessEvent::Agent(AgentEvent::ToolExecutionStart { .. })
                ) {
                    harness.set_stream_options(AgentHarnessStreamOptions {
                        timeout_ms: Some(2000),
                        headers: Some(headers(&[("turn", "second")])),
                        ..Default::default()
                    });
                }
                Ok(())
            }
        })
    });

    harness.prompt("hello", None).await.expect("prompt");

    let captured = captured.lock().expect("captured");
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[0].timeout_ms, Some(1000));
    assert_eq!(
        captured[0].headers,
        Some(HashMap::from([(
            "turn".to_owned(),
            Some("first".to_owned())
        )]))
    );
    assert_eq!(captured[1].timeout_ms, Some(2000));
    assert_eq!(
        captured[1].headers,
        Some(HashMap::from([(
            "turn".to_owned(),
            Some("second".to_owned())
        )]))
    );
}

// ---------------------------------------------------------------------------
// Payload hook chaining (:178-208)
// ---------------------------------------------------------------------------

/// Dummy static-key auth so `Models` accepts the probe provider.
struct ProbeAuth;

#[async_trait]
impl ApiKeyAuth for ProbeAuth {
    fn name(&self) -> &str {
        "probe API key"
    }

    async fn resolve(
        &self,
        _ctx: &dyn AuthContext,
        _credential: Option<&ApiKeyCredential>,
    ) -> Result<Option<AuthResult>, ModelsError> {
        Ok(Some(AuthResult {
            auth: ModelAuth {
                api_key: Some("probe-key".to_owned()),
                headers: None,
                base_url: None,
            },
            env: None,
            source: Some("PROBE_API_KEY".to_owned()),
        }))
    }
}

/// A `Provider` that invokes the `onPayload` callback (like a real provider)
/// and otherwise streams the faux provider's scripted responses.
struct PayloadProbeProvider {
    faux: Arc<FauxProvider>,
    auth: ProviderAuth,
    captured_payload: Arc<Mutex<Option<Option<Value>>>>,
}

impl Provider for PayloadProbeProvider {
    fn id(&self) -> &str {
        self.faux.provider()
    }

    fn name(&self) -> &str {
        self.faux.provider()
    }

    fn base_url(&self) -> Option<&str> {
        None
    }

    fn headers(&self) -> Option<&ProviderHeaders> {
        None
    }

    fn auth(&self) -> &ProviderAuth {
        &self.auth
    }

    fn get_models(&self) -> Vec<Model> {
        self.faux.models().to_vec()
    }

    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<StreamOptions>,
    ) -> AssistantMessageEventStream {
        let options = options.unwrap_or_default();
        let stream = AssistantMessageEventStream::new();
        let producer = stream.clone();
        let faux = Arc::clone(&self.faux);
        let captured_payload = Arc::clone(&self.captured_payload);
        let model = model.clone();
        let context = context.clone();
        tokio::spawn(async move {
            if let Some(on_payload) = &options.on_payload {
                let result = on_payload(json!({ "steps": ["provider"] }), &model).await;
                *captured_payload.lock().expect("payload") = Some(result);
            }
            let mut events = (faux.stream_fn())(model, context, options);
            while let Some(event) = events.next().await {
                producer.push(event);
            }
            producer.end(None);
        });
        stream
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        self.stream(model, context, options.map(|simple| simple.stream))
    }
}

/// "chains provider payload hooks" (:178-208).
#[tokio::test]
async fn test_provider_payload_hook_chaining() {
    let faux = FauxProvider::new(FauxProviderOptions::default());
    faux.set_responses(vec![faux_assistant_message("ok", Default::default()).into()]);
    let captured_payload: Arc<Mutex<Option<Option<Value>>>> = Arc::new(Mutex::new(None));
    let models = Models::new(None);
    models.set_provider(Arc::new(PayloadProbeProvider {
        faux: Arc::clone(&faux),
        auth: ProviderAuth {
            api_key: Some(Arc::new(ProbeAuth)),
            oauth: None,
        },
        captured_payload: Arc::clone(&captured_payload),
    }));

    let harness = Arc::new(
        AgentHarness::new(base_options(
            &models,
            as_dyn_session(&new_session()),
            faux.get_model(None).expect("faux model"),
        ))
        .expect("harness"),
    );
    let seen_payloads: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));

    let _first = harness.on("before_provider_payload", {
        let seen_payloads = Arc::clone(&seen_payloads);
        hook(move |event| {
            let seen_payloads = Arc::clone(&seen_payloads);
            async move {
                let AgentHarnessOwnEvent::BeforeProviderPayload { payload, .. } = &event else {
                    return Ok(HarnessHookResult::BeforeProviderPayload(None));
                };
                seen_payloads
                    .lock()
                    .expect("payloads")
                    .push(payload.clone());
                Ok(HarnessHookResult::BeforeProviderPayload(Some(
                    BeforeProviderPayloadResult {
                        payload: json!({ "steps": ["provider", "first"] }),
                    },
                )))
            }
        })
    });
    let _second = harness.on("before_provider_payload", {
        let seen_payloads = Arc::clone(&seen_payloads);
        hook(move |event| {
            let seen_payloads = Arc::clone(&seen_payloads);
            async move {
                let AgentHarnessOwnEvent::BeforeProviderPayload { payload, .. } = &event else {
                    return Ok(HarnessHookResult::BeforeProviderPayload(None));
                };
                seen_payloads
                    .lock()
                    .expect("payloads")
                    .push(payload.clone());
                Ok(HarnessHookResult::BeforeProviderPayload(Some(
                    BeforeProviderPayloadResult {
                        payload: json!({ "steps": ["provider", "first", "second"] }),
                    },
                )))
            }
        })
    });

    harness.prompt("hello", None).await.expect("prompt");

    assert_eq!(
        *seen_payloads.lock().expect("payloads"),
        vec![
            json!({ "steps": ["provider"] }),
            json!({ "steps": ["provider", "first"] }),
        ]
    );
    assert_eq!(
        *captured_payload.lock().expect("payload"),
        Some(Some(json!({ "steps": ["provider", "first", "second"] })))
    );
}
