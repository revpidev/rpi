//! Port of `external/pi/packages/agent/test/harness/agent-harness.test.ts` @
//! pi 0.82.1 (2efa728) — the `AgentHarness` behavior contract.
//!
//! Structural mapping notes:
//! - The upstream shared `models` collection with per-test unique faux
//!   provider ids becomes a per-test `Models` instance (each test registers
//!   its own `FauxAiProvider`).
//! - Upstream floating promises (`void harness.setModel(...)` inside
//!   listeners) become awaited calls: listeners are invoked through the
//!   harness barrier, and re-entrant emissions only invoke the listener with
//!   unrelated event types, so awaiting is deadlock-free.
//! - The abort test's blocking first response (an upstream async factory
//!   awaiting a release promise) becomes `tokens_per_second` pacing: the
//!   abort lands while the first response is still streaming.
//! - App-specific tool/resource subtypes (`AppTool.source`, `AppSkill.source`)
//!   have no Rust trait-object equivalent; the tests assert names/fields
//!   instead.
//! - `AgentHarness` is generic over the tool context: context-free tests use
//!   `AgentHarness<()>`; the tool-context tests use a small `TestContext`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rpi_agent::error::AgentError;
use rpi_agent::harness::session::memory_storage::{
    InMemorySessionStorage, InMemorySessionStorageOptions,
};
use rpi_agent::harness::session::Session as SessionFacade;
use rpi_agent::harness::types::{
    AgentHarnessOptions, AgentHarnessResources, AgentHarnessTool, AgentHarnessToolContextSource,
    BeforeAgentStartResult, CompactResult, SessionContextBuildOptions, SessionEntryCursorOptions,
    SessionMetadata, SessionStats, SessionStorage, ToolResultPatch, UpdateSource,
};
use rpi_agent::harness::{
    AgentHarness, AgentHarnessError, AgentHarnessErrorCode, AgentHarnessEvent, AgentHarnessHook,
    AgentHarnessListener, AgentHarnessOwnEvent, Session, SessionError, SessionErrorCode,
};
use rpi_agent::messages::{AgentMessage, CustomMessage, CustomRole};
use rpi_agent::session::{MessageEntry, SessionEntry};
use rpi_agent::types::{AgentEvent, AgentToolResult, AgentToolUpdateCallback, QueueMode};
use rpi_ai::models::Models;
use rpi_ai::types::{
    ApiKind, AssistantContent, AssistantMessage, AssistantRole, Context, Model, ModelThinkingLevel,
    StopReason, TextContent, ToolResultContent, Usage, UsageCost, UserContent, UserContentBlock,
    UserMessage, UserRole,
};
use rpi_ai::utils::retry::RetryPolicy;
use rpi_test_support::faux::{
    faux_assistant_message, faux_tool_call, FauxAiProvider, FauxAssistantOptions,
    FauxModelDefinition, FauxProvider, FauxProviderOptions, FauxResponseStep,
};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

/// Wall-clock ceiling for every async test in this file: a deadlock
/// regression fails the run with a clear panic instead of hanging CI (the
/// `TEST_TIMEOUT` pattern of agent_test.rs).
const TEST_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Unique faux provider + `Models` registration (upstream `newFaux`).
fn new_faux(options: FauxProviderOptions) -> (Models, Arc<FauxProvider>) {
    let faux = FauxProvider::new(options);
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

/// `Arc<dyn Session>` view of a facade (the harness options field type).
fn as_dyn_session(
    session: &Arc<SessionFacade<SessionMetadata>>,
) -> Arc<dyn Session<Metadata = SessionMetadata>> {
    session.clone()
}

#[allow(clippy::too_many_arguments)]
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

fn create_usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Usage {
    Usage {
        input,
        output,
        cache_read,
        cache_write,
        cache_write1h: None,
        reasoning: None,
        total_tokens: input + output + cache_read + cache_write,
        cost: UsageCost::default(),
    }
}

fn create_user_message(text: &str) -> AgentMessage {
    AgentMessage::User(UserMessage {
        role: UserRole::User,
        content: UserContent::Blocks(vec![UserContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })]),
        timestamp: 1,
    })
}

fn create_assistant_message(text: &str) -> AgentMessage {
    AgentMessage::Assistant(AssistantMessage {
        role: AssistantRole::Assistant,
        content: vec![AssistantContent::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        api: ApiKind::from("faux"),
        provider: "faux".to_owned(),
        model: "faux-1".to_owned(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: create_usage(100, 50, 0, 0),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: 2,
    })
}

fn user_message_text(user: &UserMessage) -> String {
    match &user.content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => blocks
            .iter()
            .map(|block| match block {
                UserContentBlock::Text(text) => text.text.as_str(),
                _ => "",
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

/// `textFromUserMessages` (agent-harness.test.ts:39-49) over the LLM context.
fn text_from_user_messages(context: &Context) -> Vec<String> {
    context
        .messages
        .iter()
        .filter_map(|message| match message {
            rpi_ai::types::Message::User(user) => Some(user_message_text(user)),
            _ => None,
        })
        .collect()
}

/// The upstream `type` tag of a harness event (via the serde wire shape).
fn event_type(event: &AgentHarnessEvent) -> String {
    serde_json::to_value(event)
        .expect("event serialization")
        .get("type")
        .and_then(Value::as_str)
        .expect("event type tag")
        .to_owned()
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
    Fut: std::future::Future<
            Output = Result<rpi_agent::harness::HarnessHookResult, AgentHarnessError>,
        > + Send
        + 'static,
{
    Arc::new(move |event| Box::pin(f(event)))
}

// ---------------------------------------------------------------------------
// Test tools (test/utils/calculate.ts, test/utils/get-current-time.ts)
// ---------------------------------------------------------------------------

/// Tiny evaluator for the expressions the fixtures use (`a + b` style);
/// upstream delegates to `new Function` (calculate.ts:9-16).
fn calculate(expression: &str) -> Result<String, AgentError> {
    for op in ['+', '-', '*', '/'] {
        if let Some((left, right)) = expression.split_once(op) {
            let left: f64 = left
                .trim()
                .parse()
                .map_err(|_| AgentError::Message(format!("invalid expression: {expression}")))?;
            let right: f64 = right
                .trim()
                .parse()
                .map_err(|_| AgentError::Message(format!("invalid expression: {expression}")))?;
            let result = match op {
                '+' => left + right,
                '-' => left - right,
                '*' => left * right,
                _ => left / right,
            };
            return Ok(format!("{expression} = {result}"));
        }
    }
    Err(AgentError::Message(format!(
        "invalid expression: {expression}"
    )))
}

fn calculate_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "expression": {
                "type": "string",
                "description": "The mathematical expression to evaluate"
            }
        },
        "required": ["expression"]
    })
}

/// `calculateTool` / `createCalculateToolWithUsage` (calculate.ts:28-49).
struct CalculateTool {
    usage: Option<Usage>,
    parameters: Value,
}

impl CalculateTool {
    fn new() -> Self {
        Self {
            usage: None,
            parameters: calculate_parameters(),
        }
    }

    fn with_usage(usage: Usage) -> Self {
        Self {
            usage: Some(usage),
            parameters: calculate_parameters(),
        }
    }
}

#[async_trait]
impl AgentHarnessTool<()> for CalculateTool {
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
        params: Value,
        _signal: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
        _context: (),
    ) -> Result<AgentToolResult, AgentError> {
        let expression = params
            .get("expression")
            .and_then(Value::as_str)
            .unwrap_or_default();
        Ok(AgentToolResult {
            content: vec![ToolResultContent::Text(TextContent {
                text: calculate(expression)?,
                text_signature: None,
            })],
            details: Value::Null,
            usage: self.usage.clone(),
            added_tool_names: None,
            terminate: None,
        })
    }
}

/// `getCurrentTimeTool` (get-current-time.ts:42-49) — fixed text suffices;
/// the tests only route it through tool lists.
struct GetCurrentTimeTool {
    parameters: Value,
}

impl GetCurrentTimeTool {
    fn new() -> Self {
        Self {
            parameters: json!({
                "type": "object",
                "properties": {
                    "timezone": {
                        "type": "string",
                        "description": "Optional timezone (e.g., 'America/New_York', 'Europe/London')"
                    }
                }
            }),
        }
    }
}

#[async_trait]
impl AgentHarnessTool<()> for GetCurrentTimeTool {
    fn name(&self) -> &str {
        "get_current_time"
    }

    fn label(&self) -> &str {
        "Current Time"
    }

    fn description(&self) -> &str {
        "Get the current date and time"
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
                text: "now".to_owned(),
                text_signature: None,
            })],
            details: json!({ "utcTimestamp": 0 }),
            usage: None,
            added_tool_names: None,
            terminate: None,
        })
    }
}

/// A named clone of the calculate tool (upstream spreads `calculateTool`).
struct NamedTool {
    name: &'static str,
    parameters: Value,
}

impl NamedTool {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            parameters: calculate_parameters(),
        }
    }
}

#[async_trait]
impl AgentHarnessTool<()> for NamedTool {
    fn name(&self) -> &str {
        self.name
    }

    fn label(&self) -> &str {
        self.name
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
        params: Value,
        _signal: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
        _context: (),
    ) -> Result<AgentToolResult, AgentError> {
        let expression = params
            .get("expression")
            .and_then(Value::as_str)
            .unwrap_or_default();
        Ok(AgentToolResult {
            content: vec![ToolResultContent::Text(TextContent {
                text: calculate(expression)?,
                text_signature: None,
            })],
            details: Value::Null,
            usage: None,
            added_tool_names: None,
            terminate: None,
        })
    }
}

fn tool_names(tools: &[Arc<dyn AgentHarnessTool<()>>]) -> Vec<String> {
    tools.iter().map(|tool| tool.name().to_owned()).collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// "constructs directly and exposes queue modes" (:93-113).
#[tokio::test]
async fn test_construct_exposes_queue_modes() {
    tokio::time::timeout(TEST_TIMEOUT, test_construct_exposes_queue_modes_body())
        .await
        .expect("test_construct_exposes_queue_modes timed out");
}

async fn test_construct_exposes_queue_modes_body() {
    let (models, faux) = new_faux(FauxProviderOptions::default());
    let model = faux.get_model(None).expect("faux model");
    let harness = AgentHarness::new(AgentHarnessOptions {
        thinking_level: Some(ModelThinkingLevel::High),
        system_prompt: Some(rpi_agent::harness::AgentHarnessSystemPrompt::Static(
            "You are helpful.".to_owned(),
        )),
        steering_mode: Some(QueueMode::All),
        follow_up_mode: Some(QueueMode::All),
        ..base_options(&models, as_dyn_session(&new_session()), model.clone())
    })
    .expect("harness");

    assert_eq!(harness.get_model(), model);
    assert_eq!(harness.get_thinking_level(), ModelThinkingLevel::High);
    assert_eq!(harness.get_steering_mode(), QueueMode::All);
    assert_eq!(harness.get_follow_up_mode(), QueueMode::All);
    harness.set_steering_mode(QueueMode::OneAtATime);
    harness.set_follow_up_mode(QueueMode::OneAtATime);
    assert_eq!(harness.get_steering_mode(), QueueMode::OneAtATime);
    assert_eq!(harness.get_follow_up_mode(), QueueMode::OneAtATime);
}

/// "drains one queued steering message at a time and emits queue updates"
/// (:115-155).
#[tokio::test]
async fn test_steer_drains_one_at_a_time_and_emits_queue_updates() {
    tokio::time::timeout(
        TEST_TIMEOUT,
        test_steer_drains_one_at_a_time_and_emits_queue_updates_body(),
    )
    .await
    .expect("test_steer_drains_one_at_a_time_and_emits_queue_updates timed out");
}

async fn test_steer_drains_one_at_a_time_and_emits_queue_updates_body() {
    let (models, faux) = new_faux(FauxProviderOptions::default());
    let user_counts: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
    faux.set_responses(
        ["first", "second", "third"]
            .iter()
            .map(|text| {
                let user_counts = Arc::clone(&user_counts);
                FauxResponseStep::Factory(Box::new(move |context, _options, _state, _model| {
                    let count = context
                        .messages
                        .iter()
                        .filter(|m| matches!(m, rpi_ai::types::Message::User(_)))
                        .count();
                    user_counts.lock().expect("counts").push(count);
                    faux_assistant_message(*text, FauxAssistantOptions::default())
                }))
            })
            .collect(),
    );
    let harness = Arc::new(
        AgentHarness::new(AgentHarnessOptions {
            steering_mode: Some(QueueMode::OneAtATime),
            ..base_options(
                &models,
                as_dyn_session(&new_session()),
                faux.get_model(None).expect("faux model"),
            )
        })
        .expect("harness"),
    );
    let steer_queue_lengths: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
    let queued = Arc::new(AtomicBool::new(false));
    let _unsubscribe = harness.subscribe({
        let harness = Arc::clone(&harness);
        let lengths = Arc::clone(&steer_queue_lengths);
        listener(move |event, _signal| {
            let harness = Arc::clone(&harness);
            let lengths = Arc::clone(&lengths);
            let queued = Arc::clone(&queued);
            async move {
                match &event {
                    AgentHarnessEvent::Harness(AgentHarnessOwnEvent::QueueUpdate {
                        steer, ..
                    }) => lengths.lock().expect("lengths").push(steer.len()),
                    AgentHarnessEvent::Agent(AgentEvent::MessageStart { message })
                        if matches!(message, AgentMessage::Assistant(_))
                            && !queued.swap(true, Ordering::SeqCst) =>
                    {
                        harness.steer("one", None).await?;
                        harness.steer("two", None).await?;
                    }
                    _ => {}
                }
                Ok(())
            }
        })
    });

    harness.prompt("hello", None).await.expect("prompt");

    assert_eq!(*user_counts.lock().expect("counts"), vec![1, 2, 3]);
    assert_eq!(
        *steer_queue_lengths.lock().expect("lengths"),
        vec![1, 2, 1, 0]
    );
}

/// "appends before_agent_start messages and persists them" (:157-186).
#[tokio::test]
async fn test_before_agent_start_appends_and_persists_messages() {
    tokio::time::timeout(
        TEST_TIMEOUT,
        test_before_agent_start_appends_and_persists_messages_body(),
    )
    .await
    .expect("test_before_agent_start_appends_and_persists_messages timed out");
}

async fn test_before_agent_start_appends_and_persists_messages_body() {
    let (models, faux) = new_faux(FauxProviderOptions::default());
    let request_text: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    faux.set_responses(vec![FauxResponseStep::Factory({
        let request_text = Arc::clone(&request_text);
        Box::new(move |context, _options, _state, _model| {
            *request_text.lock().expect("request text") = text_from_user_messages(context);
            faux_assistant_message("ok", FauxAssistantOptions::default())
        })
    })]);
    let session = new_session();
    let harness = Arc::new(
        AgentHarness::new(base_options(
            &models,
            as_dyn_session(&session),
            faux.get_model(None).expect("faux model"),
        ))
        .expect("harness"),
    );
    let _unsubscribe = harness.on(
        "before_agent_start",
        hook(|_event| async {
            Ok(rpi_agent::harness::HarnessHookResult::BeforeAgentStart(
                Some(BeforeAgentStartResult {
                    messages: Some(vec![create_user_message("hook")]),
                    system_prompt: None,
                }),
            ))
        }),
    );

    harness.prompt("hello", None).await.expect("prompt");

    let entries = session
        .get_entries(Default::default())
        .await
        .expect("entries");
    let persisted_text: Vec<String> = entries
        .iter()
        .filter_map(|entry| match entry {
            SessionEntry::Message(message) => match &message.message {
                AgentMessage::User(user) => Some(user_message_text(user)),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        *request_text.lock().expect("request text"),
        vec!["hello", "hook"]
    );
    assert_eq!(persisted_text, vec!["hello", "hook"]);
}

/// "abort clears steer and follow-up queues but preserves next-turn messages"
/// (:188-240). The upstream blocking first response becomes paced streaming;
/// the abort lands mid-stream.
#[tokio::test]
async fn test_abort_clears_steer_followup_preserves_next_turn() {
    tokio::time::timeout(
        TEST_TIMEOUT,
        test_abort_clears_steer_followup_preserves_next_turn_body(),
    )
    .await
    .expect("test_abort_clears_steer_followup_preserves_next_turn timed out");
}

async fn test_abort_clears_steer_followup_preserves_next_turn_body() {
    let (models, faux) = new_faux(FauxProviderOptions {
        tokens_per_second: Some(50.0),
        ..Default::default()
    });
    let aborted_signal: Arc<Mutex<Option<CancellationToken>>> = Arc::new(Mutex::new(None));
    let second_request_text: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    faux.set_responses(vec![
        FauxResponseStep::Factory({
            let aborted_signal = Arc::clone(&aborted_signal);
            Box::new(move |_context, options, _state, _model| {
                *aborted_signal.lock().expect("signal") =
                    options.and_then(|options| options.signal.clone());
                faux_assistant_message("aborted-ish", FauxAssistantOptions::default())
            })
        }),
        FauxResponseStep::Factory({
            let second_request_text = Arc::clone(&second_request_text);
            Box::new(move |context, _options, _state, _model| {
                *second_request_text.lock().expect("text") = text_from_user_messages(context);
                faux_assistant_message("second", FauxAssistantOptions::default())
            })
        }),
    ]);
    let harness = Arc::new(
        AgentHarness::new(base_options(
            &models,
            as_dyn_session(&new_session()),
            faux.get_model(None).expect("faux model"),
        ))
        .expect("harness"),
    );
    let queue_updates: Arc<Mutex<Vec<(usize, usize, usize)>>> = Arc::new(Mutex::new(Vec::new()));
    let triggered = Arc::new(AtomicBool::new(false));
    let abort_handle_slot: Arc<Mutex<Option<tokio::task::JoinHandle<_>>>> =
        Arc::new(Mutex::new(None));
    let _unsubscribe = harness.subscribe({
        let harness = Arc::clone(&harness);
        let updates = Arc::clone(&queue_updates);
        let slot = Arc::clone(&abort_handle_slot);
        listener(move |event, _signal| {
            let harness = Arc::clone(&harness);
            let queue_updates = Arc::clone(&updates);
            let triggered = Arc::clone(&triggered);
            let abort_handle_slot = Arc::clone(&slot);
            async move {
                match &event {
                    AgentHarnessEvent::Harness(AgentHarnessOwnEvent::QueueUpdate {
                        steer,
                        follow_up,
                        next_turn,
                    }) => queue_updates.lock().expect("queue updates").push((
                        steer.len(),
                        follow_up.len(),
                        next_turn.len(),
                    )),
                    AgentHarnessEvent::Agent(AgentEvent::MessageStart { message })
                        if matches!(message, AgentMessage::Assistant(_))
                            && !triggered.swap(true, Ordering::SeqCst) =>
                    {
                        harness.steer("steer", None).await?;
                        harness.follow_up("follow", None).await?;
                        harness.next_turn("next", None).await?;
                        // abort() awaits run settlement — it must run off the
                        // listener barrier.
                        let abort_harness = Arc::clone(&harness);
                        *abort_handle_slot.lock().expect("abort slot") =
                            Some(tokio::spawn(async move { abort_harness.abort().await }));
                    }
                    _ => {}
                }
                Ok(())
            }
        })
    });

    let prompt_harness = Arc::clone(&harness);
    let first_prompt = tokio::spawn(async move { prompt_harness.prompt("first", None).await });
    // The first prompt settles (aborted); by then the listener has spawned
    // the abort task.
    let first = first_prompt
        .await
        .expect("prompt task")
        .expect("first prompt");
    let abort_result = abort_handle_slot
        .lock()
        .expect("abort slot")
        .take()
        .expect("abort spawned by the time prompt settles");
    let abort_result = abort_result.await.expect("abort task").expect("abort");
    assert_eq!(first.stop_reason, StopReason::Aborted);
    assert!(aborted_signal
        .lock()
        .expect("signal")
        .as_ref()
        .is_some_and(CancellationToken::is_cancelled));

    harness.prompt("second", None).await.expect("second prompt");

    assert_eq!(abort_result.cleared_steer.len(), 1);
    assert_eq!(abort_result.cleared_follow_up.len(), 1);
    assert!(queue_updates
        .lock()
        .expect("queue updates")
        .contains(&(0, 0, 1)));
    assert_eq!(
        *second_request_text.lock().expect("text"),
        vec!["first", "next", "second"]
    );
}

/// "drains follow-up messages one at a time after the agent would otherwise
/// stop" (:242-282).
#[tokio::test]
async fn test_follow_up_drains_one_at_a_time_after_stop() {
    tokio::time::timeout(
        TEST_TIMEOUT,
        test_follow_up_drains_one_at_a_time_after_stop_body(),
    )
    .await
    .expect("test_follow_up_drains_one_at_a_time_after_stop timed out");
}

async fn test_follow_up_drains_one_at_a_time_after_stop_body() {
    let (models, faux) = new_faux(FauxProviderOptions::default());
    let user_counts: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
    faux.set_responses(
        ["first", "second", "third"]
            .iter()
            .map(|text| {
                let user_counts = Arc::clone(&user_counts);
                FauxResponseStep::Factory(Box::new(move |context, _options, _state, _model| {
                    let count = context
                        .messages
                        .iter()
                        .filter(|m| matches!(m, rpi_ai::types::Message::User(_)))
                        .count();
                    user_counts.lock().expect("counts").push(count);
                    faux_assistant_message(*text, FauxAssistantOptions::default())
                }))
            })
            .collect(),
    );
    let harness = Arc::new(
        AgentHarness::new(AgentHarnessOptions {
            follow_up_mode: Some(QueueMode::OneAtATime),
            ..base_options(
                &models,
                as_dyn_session(&new_session()),
                faux.get_model(None).expect("faux model"),
            )
        })
        .expect("harness"),
    );
    let follow_up_lengths: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
    let queued = Arc::new(AtomicBool::new(false));
    let _unsubscribe = harness.subscribe({
        let harness = Arc::clone(&harness);
        let lengths = Arc::clone(&follow_up_lengths);
        listener(move |event, _signal| {
            let harness = Arc::clone(&harness);
            let lengths = Arc::clone(&lengths);
            let queued = Arc::clone(&queued);
            async move {
                match &event {
                    AgentHarnessEvent::Harness(AgentHarnessOwnEvent::QueueUpdate {
                        follow_up,
                        ..
                    }) => lengths.lock().expect("lengths").push(follow_up.len()),
                    AgentHarnessEvent::Agent(AgentEvent::MessageStart { message })
                        if matches!(message, AgentMessage::Assistant(_))
                            && !queued.swap(true, Ordering::SeqCst) =>
                    {
                        harness.follow_up("one", None).await?;
                        harness.follow_up("two", None).await?;
                    }
                    _ => {}
                }
                Ok(())
            }
        })
    });

    harness.prompt("hello", None).await.expect("prompt");

    assert_eq!(*user_counts.lock().expect("counts"), vec![1, 2, 3]);
    assert_eq!(
        *follow_up_lengths.lock().expect("lengths"),
        vec![1, 2, 1, 0]
    );
}

/// "settles thrown hook failures with persisted assistant error messages"
/// (:284-312).
#[tokio::test]
async fn test_hook_failure_settles_with_persisted_error_message() {
    tokio::time::timeout(
        TEST_TIMEOUT,
        test_hook_failure_settles_with_persisted_error_message_body(),
    )
    .await
    .expect("test_hook_failure_settles_with_persisted_error_message timed out");
}

async fn test_hook_failure_settles_with_persisted_error_message_body() {
    let (models, faux) = new_faux(FauxProviderOptions::default());
    faux.set_responses(vec![faux_assistant_message(
        "should not be used",
        Default::default(),
    )
    .into()]);
    let session = new_session();
    let harness = Arc::new(
        AgentHarness::new(base_options(
            &models,
            as_dyn_session(&session),
            faux.get_model(None).expect("faux model"),
        ))
        .expect("harness"),
    );
    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let _unsubscribe = harness.subscribe({
        let events = Arc::clone(&events);
        listener(move |event, _signal| {
            let events = Arc::clone(&events);
            async move {
                events.lock().expect("events").push(event_type(&event));
                Ok(())
            }
        })
    });
    let _unsubscribe = harness.on(
        "context",
        hook(|_event| async {
            Err(AgentHarnessError::new(
                AgentHarnessErrorCode::Hook,
                "context exploded",
            ))
        }),
    );

    let response = harness.prompt("hello", None).await.expect("prompt settles");
    let second = harness
        .prompt("after failure", None)
        .await
        .expect("second prompt");
    assert!(matches!(second, AssistantMessage { .. }));

    let entries = session
        .get_entries(Default::default())
        .await
        .expect("entries");
    let messages: Vec<AgentMessage> = entries
        .iter()
        .filter_map(|entry| match entry {
            SessionEntry::Message(message) => Some(message.message.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(response.stop_reason, StopReason::Error);
    assert_eq!(response.error_message.as_deref(), Some("context exploded"));
    assert!(matches!(messages.first(), Some(AgentMessage::User(_))));
    match messages.get(1) {
        Some(AgentMessage::Assistant(assistant)) => {
            assert_eq!(assistant.stop_reason, StopReason::Error);
            assert_eq!(assistant.error_message.as_deref(), Some("context exploded"));
        }
        other => panic!("expected persisted assistant failure, got {other:?}"),
    }
    let events = events.lock().expect("events");
    assert!(events.iter().any(|t| t == "agent_end"));
    assert!(events.iter().any(|t| t == "settled"));
}

/// "refreshes model, thinking level, resources, system prompt, and active
/// tools at save points" (:314-376).
#[tokio::test]
async fn test_save_point_refreshes_config() {
    tokio::time::timeout(TEST_TIMEOUT, test_save_point_refreshes_config_body())
        .await
        .expect("test_save_point_refreshes_config timed out");
}

async fn test_save_point_refreshes_config_body() {
    let (models, faux) = new_faux(FauxProviderOptions {
        models: Some(vec![
            FauxModelDefinition {
                id: "first".to_owned(),
                name: None,
                reasoning: Some(true),
                input: None,
                cost: None,
                context_window: None,
                max_tokens: None,
            },
            FauxModelDefinition {
                id: "second".to_owned(),
                name: None,
                reasoning: Some(true),
                input: None,
                cost: None,
                context_window: None,
                max_tokens: None,
            },
        ]),
        ..Default::default()
    });
    let second_model = faux.get_model(Some("second")).expect("second model");
    type Capture = (String, Option<ModelThinkingLevel>, String, Vec<String>);
    let captured: Arc<Mutex<Vec<Capture>>> = Arc::new(Mutex::new(Vec::new()));
    faux.set_responses(vec![
        FauxResponseStep::Factory({
            let captured = Arc::clone(&captured);
            Box::new(move |context, options, _state, model| {
                captured.lock().expect("captured").push((
                    model.id.clone(),
                    options.and_then(|options| options.reasoning),
                    context.system_prompt.clone().unwrap_or_default(),
                    context
                        .tools
                        .as_ref()
                        .map(|tools| tools.iter().map(|tool| tool.name.clone()).collect())
                        .unwrap_or_default(),
                ));
                faux_assistant_message(
                    vec![faux_tool_call(
                        "calculate",
                        json!({ "expression": "1 + 1" })
                            .as_object()
                            .expect("object")
                            .clone(),
                        Some("call-1".to_owned()),
                    )],
                    FauxAssistantOptions {
                        stop_reason: Some(StopReason::ToolUse),
                        ..Default::default()
                    },
                )
            })
        }),
        FauxResponseStep::Factory({
            let captured = Arc::clone(&captured);
            Box::new(move |context, options, _state, model| {
                captured.lock().expect("captured").push((
                    model.id.clone(),
                    options.and_then(|options| options.reasoning),
                    context.system_prompt.clone().unwrap_or_default(),
                    context
                        .tools
                        .as_ref()
                        .map(|tools| tools.iter().map(|tool| tool.name.clone()).collect())
                        .unwrap_or_default(),
                ));
                faux_assistant_message("done", FauxAssistantOptions::default())
            })
        }),
    ]);
    let harness = Arc::new(
        AgentHarness::new(AgentHarnessOptions {
            thinking_level: Some(ModelThinkingLevel::Off),
            resources: AgentHarnessResources {
                skills: Some(vec![rpi_agent::harness::Skill {
                    name: "prompt".to_owned(),
                    description: "prompt".to_owned(),
                    content: "first prompt".to_owned(),
                    file_path: "/skills/prompt".to_owned(),
                    disable_model_invocation: None,
                }]),
                prompt_templates: None,
            },
            system_prompt: Some(rpi_agent::harness::AgentHarnessSystemPrompt::Dynamic(
                Arc::new(|context| {
                    let prompt = context
                        .resources
                        .skills
                        .as_ref()
                        .and_then(|skills| skills.first().map(|skill| skill.content.clone()))
                        .unwrap_or_else(|| "missing prompt".to_owned());
                    Box::pin(async move { prompt })
                }),
            )),
            tools: vec![Arc::new(CalculateTool::new())],
            ..base_options(
                &models,
                as_dyn_session(&new_session()),
                faux.get_model(None).expect("first model"),
            )
        })
        .expect("harness"),
    );
    let _unsubscribe = harness.subscribe({
        let harness = Arc::clone(&harness);
        let second_model = second_model.clone();
        listener(move |event, _signal| {
            let harness = Arc::clone(&harness);
            let second_model = second_model.clone();
            async move {
                if matches!(
                    event,
                    AgentHarnessEvent::Agent(AgentEvent::ToolExecutionStart { .. })
                ) {
                    harness.set_model(second_model).await?;
                    harness.set_thinking_level(ModelThinkingLevel::High).await?;
                    harness
                        .set_resources(AgentHarnessResources {
                            skills: Some(vec![rpi_agent::harness::Skill {
                                name: "prompt".to_owned(),
                                description: "prompt".to_owned(),
                                content: "second prompt".to_owned(),
                                file_path: "/skills/prompt".to_owned(),
                                disable_model_invocation: None,
                            }]),
                            prompt_templates: None,
                        })
                        .await?;
                    harness
                        .set_tools(
                            vec![
                                Arc::new(CalculateTool::new()) as Arc<dyn AgentHarnessTool<()>>,
                                Arc::new(GetCurrentTimeTool::new()),
                            ],
                            Some(vec!["get_current_time".to_owned()]),
                        )
                        .await?;
                }
                Ok(())
            }
        })
    });

    harness.prompt("hello", None).await.expect("prompt");

    assert_eq!(
        *captured.lock().expect("captured"),
        vec![
            (
                "first".to_owned(),
                None,
                "first prompt".to_owned(),
                vec!["calculate".to_owned()]
            ),
            (
                "second".to_owned(),
                Some(ModelThinkingLevel::High),
                "second prompt".to_owned(),
                vec!["get_current_time".to_owned()]
            ),
        ]
    );
}

/// "orders pending listener session writes after agent-emitted messages"
/// (:378-406).
#[tokio::test]
async fn test_pending_writes_ordered_after_agent_messages() {
    tokio::time::timeout(
        TEST_TIMEOUT,
        test_pending_writes_ordered_after_agent_messages_body(),
    )
    .await
    .expect("test_pending_writes_ordered_after_agent_messages timed out");
}

async fn test_pending_writes_ordered_after_agent_messages_body() {
    let (models, faux) = new_faux(FauxProviderOptions::default());
    faux.set_responses(vec![faux_assistant_message("ok", Default::default()).into()]);
    let session = new_session();
    let harness = Arc::new(
        AgentHarness::new(base_options(
            &models,
            as_dyn_session(&session),
            faux.get_model(None).expect("faux model"),
        ))
        .expect("harness"),
    );
    let wrote_pending = Arc::new(AtomicBool::new(false));
    let _unsubscribe = harness.subscribe({
        let harness = Arc::clone(&harness);
        listener(move |event, _signal| {
            let harness = Arc::clone(&harness);
            let wrote_pending = Arc::clone(&wrote_pending);
            async move {
                if let AgentHarnessEvent::Agent(AgentEvent::MessageEnd { message }) = &event {
                    if matches!(message, AgentMessage::Assistant(_))
                        && !wrote_pending.swap(true, Ordering::SeqCst)
                    {
                        harness
                            .append_message(AgentMessage::Custom(CustomMessage {
                                role: CustomRole::Custom,
                                custom_type: "listener".to_owned(),
                                content: UserContent::Text("listener write".to_owned()),
                                display: true,
                                details: None,
                                timestamp: 3,
                            }))
                            .await?;
                    }
                }
                Ok(())
            }
        })
    });

    harness.prompt("hello", None).await.expect("prompt");

    let entries = session
        .get_entries(Default::default())
        .await
        .expect("entries");
    let roles: Vec<&str> = entries
        .iter()
        .filter_map(|entry| match entry {
            SessionEntry::Message(message) => Some(match &message.message {
                AgentMessage::User(_) => "user",
                AgentMessage::Assistant(_) => "assistant",
                AgentMessage::ToolResult(_) => "toolResult",
                AgentMessage::Custom(_) => "custom",
                _ => "other",
            }),
            _ => None,
        })
        .collect();
    assert_eq!(roles, vec!["user", "assistant", "custom"]);
}

/// "waitForIdle waits for external run settlement and awaited listeners"
/// (:408-437).
#[tokio::test]
async fn test_wait_for_idle_waits_for_settlement_and_listeners() {
    tokio::time::timeout(
        TEST_TIMEOUT,
        test_wait_for_idle_waits_for_settlement_and_listeners_body(),
    )
    .await
    .expect("test_wait_for_idle_waits_for_settlement_and_listeners timed out");
}

async fn test_wait_for_idle_waits_for_settlement_and_listeners_body() {
    let (models, faux) = new_faux(FauxProviderOptions::default());
    faux.set_responses(vec![faux_assistant_message("ok", Default::default()).into()]);
    let harness = Arc::new(
        AgentHarness::new(base_options(
            &models,
            as_dyn_session(&new_session()),
            faux.get_model(None).expect("faux model"),
        ))
        .expect("harness"),
    );
    let barrier = Arc::new(tokio::sync::Notify::new());
    let listener_finished = Arc::new(AtomicBool::new(false));
    let _unsubscribe = harness.subscribe({
        let barrier = Arc::clone(&barrier);
        let listener_finished = Arc::clone(&listener_finished);
        listener(move |event, _signal| {
            let barrier = Arc::clone(&barrier);
            let listener_finished = Arc::clone(&listener_finished);
            async move {
                if matches!(event, AgentHarnessEvent::Agent(AgentEvent::AgentEnd { .. })) {
                    barrier.notified().await;
                    listener_finished.store(true, Ordering::SeqCst);
                }
                Ok(())
            }
        })
    });

    let prompt_harness = Arc::clone(&harness);
    let prompt_task = tokio::spawn(async move { prompt_harness.prompt("hello", None).await });
    // Let the prompt task install its run handle before awaiting idle.
    tokio::task::yield_now().await;
    let idle_resolved = Arc::new(AtomicBool::new(false));
    let idle_task = {
        let harness = Arc::clone(&harness);
        let idle_resolved = Arc::clone(&idle_resolved);
        tokio::spawn(async move {
            harness.wait_for_idle().await;
            idle_resolved.store(true, Ordering::SeqCst);
        })
    };
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(!idle_resolved.load(Ordering::SeqCst));
    assert!(!listener_finished.load(Ordering::SeqCst));
    barrier.notify_waiters();
    prompt_task.await.expect("prompt task").expect("prompt");
    idle_task.await.expect("idle task");
    assert!(idle_resolved.load(Ordering::SeqCst));
    assert!(listener_finished.load(Ordering::SeqCst));
}

/// "runs tool_call and tool_result hooks through the direct loop" (:439-491).
#[tokio::test]
async fn test_tool_call_and_tool_result_hooks() {
    tokio::time::timeout(TEST_TIMEOUT, test_tool_call_and_tool_result_hooks_body())
        .await
        .expect("test_tool_call_and_tool_result_hooks timed out");
}

async fn test_tool_call_and_tool_result_hooks_body() {
    let (models, faux) = new_faux(FauxProviderOptions::default());
    faux.set_responses(vec![FauxResponseStep::Factory(Box::new(
        |_c, _o, _s, _m| {
            faux_assistant_message(
                vec![faux_tool_call(
                    "calculate",
                    json!({ "expression": "2 + 2" })
                        .as_object()
                        .expect("object")
                        .clone(),
                    Some("call-1".to_owned()),
                )],
                FauxAssistantOptions {
                    stop_reason: Some(StopReason::ToolUse),
                    ..Default::default()
                },
            )
        },
    ))]);
    let session = new_session();
    let tool_usage = create_usage(1, 2, 3, 4);
    let patched_tool_usage = create_usage(5, 6, 7, 8);
    let harness = Arc::new(
        AgentHarness::new(AgentHarnessOptions {
            tools: vec![Arc::new(CalculateTool::with_usage(tool_usage.clone()))],
            ..base_options(
                &models,
                as_dyn_session(&session),
                faux.get_model(None).expect("faux model"),
            )
        })
        .expect("harness"),
    );
    let seen_tool_calls: Arc<Mutex<Vec<(String, String, Value)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let seen_tool_usage: Arc<Mutex<Option<Usage>>> = Arc::new(Mutex::new(None));
    let _unsubscribe = harness.on("tool_call", {
        let seen_tool_calls = Arc::clone(&seen_tool_calls);
        hook(move |event| {
            let seen_tool_calls = Arc::clone(&seen_tool_calls);
            async move {
                if let AgentHarnessOwnEvent::ToolCall {
                    tool_call_id,
                    tool_name,
                    input,
                } = event
                {
                    seen_tool_calls.lock().expect("tool calls").push((
                        tool_call_id,
                        tool_name,
                        input.get("expression").cloned().unwrap_or(Value::Null),
                    ));
                }
                Ok(rpi_agent::harness::HarnessHookResult::ToolCall(None))
            }
        })
    });
    let _unsubscribe2 = harness.on("tool_result", {
        let seen_tool_usage = Arc::clone(&seen_tool_usage);
        let patched_tool_usage = patched_tool_usage.clone();
        hook(move |event| {
            let seen_tool_usage = Arc::clone(&seen_tool_usage);
            let patched_tool_usage = patched_tool_usage.clone();
            async move {
                if let AgentHarnessOwnEvent::ToolResult {
                    tool_call_id,
                    tool_name,
                    usage,
                    ..
                } = event
                {
                    assert_eq!(tool_call_id, "call-1");
                    assert_eq!(tool_name, "calculate");
                    *seen_tool_usage.lock().expect("usage") = usage;
                }
                Ok(rpi_agent::harness::HarnessHookResult::ToolResult(Some(
                    ToolResultPatch {
                        content: Some(vec![ToolResultContent::Text(TextContent {
                            text: "patched result".to_owned(),
                            text_signature: None,
                        })]),
                        details: Some(json!({ "patched": true })),
                        is_error: None,
                        usage: Some(patched_tool_usage),
                        terminate: Some(true),
                    },
                )))
            }
        })
    });

    harness.prompt("hello", None).await.expect("prompt");

    let entries = session
        .get_entries(Default::default())
        .await
        .expect("entries");
    let tool_result = entries.iter().find_map(|entry| match entry {
        SessionEntry::Message(message) => match &message.message {
            AgentMessage::ToolResult(result) => Some(result),
            _ => None,
        },
        _ => None,
    });
    assert_eq!(
        *seen_tool_calls.lock().expect("tool calls"),
        vec![("call-1".to_owned(), "calculate".to_owned(), json!("2 + 2"))]
    );
    assert_eq!(*seen_tool_usage.lock().expect("usage"), Some(tool_usage));
    let tool_result = tool_result.expect("persisted tool result");
    assert_eq!(
        tool_result.content,
        vec![ToolResultContent::Text(TextContent {
            text: "patched result".to_owned(),
            text_signature: None,
        })]
    );
    assert_eq!(tool_result.details, Some(json!({ "patched": true })));
    assert_eq!(tool_result.usage, Some(patched_tool_usage));
}

// ---------------------------------------------------------------------------
// Tool context tests (:493-560)
// ---------------------------------------------------------------------------

/// Application tool context (`{ env }` upstream; an `Arc` marker so identity
/// is observable, matching upstream `toBe`).
#[derive(Clone, Default)]
struct TestContext {
    marker: Option<Arc<u64>>,
    generation: u64,
}

struct ContextTool {
    received: Arc<Mutex<Vec<TestContext>>>,
    parameters: Value,
}

#[async_trait]
impl AgentHarnessTool<TestContext> for ContextTool {
    fn name(&self) -> &str {
        "context"
    }

    fn label(&self) -> &str {
        "Context"
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
        params: Value,
        _signal: CancellationToken,
        _on_update: Option<AgentToolUpdateCallback>,
        context: TestContext,
    ) -> Result<AgentToolResult, AgentError> {
        self.received.lock().expect("received").push(context);
        let expression = params
            .get("expression")
            .and_then(Value::as_str)
            .unwrap_or_default();
        Ok(AgentToolResult {
            content: vec![ToolResultContent::Text(TextContent {
                text: calculate(expression)?,
                text_signature: None,
            })],
            details: Value::Null,
            usage: None,
            added_tool_names: None,
            terminate: None,
        })
    }
}

fn context_tool_call(id: &str, expression: &str) -> FauxResponseStep {
    FauxResponseStep::Factory(Box::new({
        let id = id.to_owned();
        let expression = expression.to_owned();
        move |_c, _o, _s, _m| {
            faux_assistant_message(
                vec![faux_tool_call(
                    "context",
                    json!({ "expression": expression })
                        .as_object()
                        .expect("object")
                        .clone(),
                    Some(id.clone()),
                )],
                FauxAssistantOptions {
                    stop_reason: Some(StopReason::ToolUse),
                    ..Default::default()
                },
            )
        }
    }))
}

/// "passes a static application context to harness tools" (:493-523).
#[tokio::test]
async fn test_static_tool_context_passed_to_tools() {
    tokio::time::timeout(
        TEST_TIMEOUT,
        test_static_tool_context_passed_to_tools_body(),
    )
    .await
    .expect("test_static_tool_context_passed_to_tools timed out");
}

async fn test_static_tool_context_passed_to_tools_body() {
    let (models, faux) = new_faux(FauxProviderOptions::default());
    faux.set_responses(vec![context_tool_call("call-1", "2 + 2")]);
    let tool_context = TestContext {
        marker: Some(Arc::new(42)),
        generation: 0,
    };
    let received: Arc<Mutex<Vec<TestContext>>> = Arc::new(Mutex::new(Vec::new()));
    let harness = Arc::new(
        AgentHarness::new(AgentHarnessOptions {
            tools: vec![Arc::new(ContextTool {
                received: Arc::clone(&received),
                parameters: calculate_parameters(),
            })],
            tool_context: Some(AgentHarnessToolContextSource::Static(tool_context.clone())),
            session: as_dyn_session(&new_session()),
            models: models.clone(),
            resources: AgentHarnessResources::default(),
            system_prompt: None,
            stream_options: None,
            retry: None,
            model: faux.get_model(None).expect("faux model"),
            thinking_level: None,
            active_tool_names: None,
            steering_mode: None,
            follow_up_mode: None,
        })
        .expect("harness"),
    );

    harness.prompt("hello", None).await.expect("prompt");

    let received = received.lock().expect("received");
    assert_eq!(received.len(), 1);
    assert!(received[0]
        .marker
        .as_ref()
        .zip(tool_context.marker.as_ref())
        .is_some_and(|(a, b)| Arc::ptr_eq(a, b)));
}

/// "resolves async tool context providers for each turn snapshot" (:525-560).
#[tokio::test]
async fn test_async_tool_context_provider_resolved_per_turn() {
    tokio::time::timeout(
        TEST_TIMEOUT,
        test_async_tool_context_provider_resolved_per_turn_body(),
    )
    .await
    .expect("test_async_tool_context_provider_resolved_per_turn timed out");
}

async fn test_async_tool_context_provider_resolved_per_turn_body() {
    let (models, faux) = new_faux(FauxProviderOptions::default());
    faux.set_responses(vec![
        context_tool_call("call-1", "1 + 1"),
        context_tool_call("call-2", "2 + 2"),
        faux_assistant_message("done", Default::default()).into(),
    ]);
    let received: Arc<Mutex<Vec<TestContext>>> = Arc::new(Mutex::new(Vec::new()));
    let generation = Arc::new(AtomicU64::new(0));
    let harness = Arc::new(
        AgentHarness::new(AgentHarnessOptions {
            tools: vec![Arc::new(ContextTool {
                received: Arc::clone(&received),
                parameters: calculate_parameters(),
            })],
            tool_context: Some(AgentHarnessToolContextSource::Provider({
                let generation = Arc::clone(&generation);
                Arc::new(move || {
                    let generation = Arc::clone(&generation);
                    Box::pin(async move {
                        TestContext {
                            marker: None,
                            generation: generation.fetch_add(1, Ordering::SeqCst) + 1,
                        }
                    })
                })
            })),
            session: as_dyn_session(&new_session()),
            models: models.clone(),
            resources: AgentHarnessResources::default(),
            system_prompt: None,
            stream_options: None,
            retry: None,
            model: faux.get_model(None).expect("faux model"),
            thinking_level: None,
            active_tool_names: None,
            steering_mode: None,
            follow_up_mode: None,
        })
        .expect("harness"),
    );

    harness.prompt("hello", None).await.expect("prompt");

    let generations: Vec<u64> = received
        .lock()
        .expect("received")
        .iter()
        .map(|context| context.generation)
        .collect();
    assert_eq!(generations, vec![1, 2]);
}

// ---------------------------------------------------------------------------
// Compaction tests (:562-606)
// ---------------------------------------------------------------------------

async fn seed_two_message_session() -> Arc<SessionFacade<SessionMetadata>> {
    let session = new_session();
    session
        .append_message(create_user_message("one"))
        .await
        .expect("append user");
    session
        .append_message(create_assistant_message("two"))
        .await
        .expect("append assistant");
    session
}

fn find_compaction_usage(entries: &[SessionEntry]) -> Option<Usage> {
    entries.iter().find_map(|entry| match entry {
        SessionEntry::Compaction(compaction) => compaction.usage.clone(),
        _ => None,
    })
}

/// "persists generated compaction usage" (:562-579).
#[tokio::test]
async fn test_compaction_persists_generated_usage() {
    tokio::time::timeout(
        TEST_TIMEOUT,
        test_compaction_persists_generated_usage_body(),
    )
    .await
    .expect("test_compaction_persists_generated_usage timed out");
}

async fn test_compaction_persists_generated_usage_body() {
    let (models, faux) = new_faux(FauxProviderOptions::default());
    faux.set_responses(vec![faux_assistant_message(
        "## Goal\nTest summary",
        Default::default(),
    )
    .into()]);
    let session = seed_two_message_session().await;
    let harness = Arc::new(
        AgentHarness::new(base_options(
            &models,
            as_dyn_session(&session),
            faux.get_model(None).expect("faux model"),
        ))
        .expect("harness"),
    );

    let result = harness.compact(None).await.expect("compact");
    let entries = session
        .get_entries(Default::default())
        .await
        .expect("entries");

    let usage = result.usage.clone().expect("compaction usage");
    assert!(usage.total_tokens > 0);
    assert_eq!(find_compaction_usage(&entries), result.usage);

    // Harness variant anchor (D-020): unlike the coding-agent variant, a
    // two-message session still compacts (no early bail) and the entry
    // carries `retainedTail` (harness compaction.ts:690-694 — verified
    // against the pinned sources: toSummarize=0, retainedTail=2).
    let compaction = entries
        .iter()
        .find_map(|entry| match entry {
            SessionEntry::Compaction(compaction) => Some(compaction),
            _ => None,
        })
        .expect("compaction entry persisted");
    let retained_tail = compaction
        .retained_tail
        .as_ref()
        .expect("harness compaction carries retainedTail");
    assert_eq!(retained_tail.len(), 2);
}

/// "persists hook-provided compaction usage" (:581-606).
#[tokio::test]
async fn test_compaction_persists_hook_provided_usage() {
    tokio::time::timeout(
        TEST_TIMEOUT,
        test_compaction_persists_hook_provided_usage_body(),
    )
    .await
    .expect("test_compaction_persists_hook_provided_usage timed out");
}

async fn test_compaction_persists_hook_provided_usage_body() {
    let (models, faux) = new_faux(FauxProviderOptions::default());
    let usage = create_usage(5, 6, 7, 8);
    let session = seed_two_message_session().await;
    let harness = Arc::new(
        AgentHarness::new(base_options(
            &models,
            as_dyn_session(&session),
            faux.get_model(None).expect("faux model"),
        ))
        .expect("harness"),
    );
    let _unsubscribe = harness.on("session_before_compact", {
        let usage = usage.clone();
        hook(move |event| {
            let usage = usage.clone();
            async move {
                let AgentHarnessOwnEvent::SessionBeforeCompact { preparation, .. } = event else {
                    return Ok(rpi_agent::harness::HarnessHookResult::SessionBeforeCompact(
                        None,
                    ));
                };
                Ok(rpi_agent::harness::HarnessHookResult::SessionBeforeCompact(
                    Some(rpi_agent::harness::types::SessionBeforeCompactResult {
                        cancel: None,
                        compaction: Some(CompactResult {
                            summary: "hook summary".to_owned(),
                            first_kept_entry_id: Some(preparation.first_kept_entry_id),
                            tokens_before: preparation.tokens_before,
                            usage: Some(usage),
                            retained_tail: None,
                            details: None,
                        }),
                    }),
                ))
            }
        })
    });

    let result = harness.compact(None).await.expect("compact");
    let entries = session
        .get_entries(Default::default())
        .await
        .expect("entries");

    assert_eq!(result.usage, Some(usage.clone()));
    assert_eq!(find_compaction_usage(&entries), Some(usage));
}

// ---------------------------------------------------------------------------
// Summarization retry tests (:608-776)
// ---------------------------------------------------------------------------

fn retry_event_collector() -> (AgentHarnessListener, Arc<Mutex<Vec<String>>>) {
    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = {
        let events = Arc::clone(&events);
        listener(move |event, _signal| {
            let events = Arc::clone(&events);
            async move {
                if let AgentHarnessEvent::Harness(owned) = &event {
                    let label = match owned {
                        AgentHarnessOwnEvent::RetryScheduled { operation, .. } => {
                            Some(format!("retry_scheduled:{}", operation_label(*operation)))
                        }
                        AgentHarnessOwnEvent::RetryAttemptStart { operation } => Some(format!(
                            "retry_attempt_start:{}",
                            operation_label(*operation)
                        )),
                        AgentHarnessOwnEvent::RetryFinished { operation } => {
                            Some(format!("retry_finished:{}", operation_label(*operation)))
                        }
                        _ => None,
                    };
                    if let Some(label) = label {
                        events.lock().expect("retry events").push(label);
                    }
                }
                Ok(())
            }
        })
    };
    (listener, events)
}

fn operation_label(operation: rpi_agent::harness::RetryOperation) -> &'static str {
    match operation {
        rpi_agent::harness::RetryOperation::Compaction => "compaction",
        rpi_agent::harness::RetryOperation::BranchSummary => "branch_summary",
    }
}

fn error_response(error_message: &'static str) -> FauxResponseStep {
    FauxResponseStep::Factory(Box::new(move |_c, _o, _s, _m| {
        faux_assistant_message(
            "",
            FauxAssistantOptions {
                stop_reason: Some(StopReason::Error),
                error_message: Some(error_message.to_owned()),
                ..Default::default()
            },
        )
    }))
}

/// "retries transient compaction errors and emits retry events" (:609-651).
#[tokio::test]
async fn test_compaction_retries_transient_errors_and_emits_events() {
    tokio::time::timeout(
        TEST_TIMEOUT,
        test_compaction_retries_transient_errors_and_emits_events_body(),
    )
    .await
    .expect("test_compaction_retries_transient_errors_and_emits_events timed out");
}

async fn test_compaction_retries_transient_errors_and_emits_events_body() {
    let (models, faux) = new_faux(FauxProviderOptions::default());
    faux.set_responses(vec![
        error_response("terminated"),
        faux_assistant_message("## Goal\nRecovered summary", Default::default()).into(),
    ]);
    let session = seed_two_message_session().await;
    let harness = Arc::new(
        AgentHarness::new(AgentHarnessOptions {
            retry: Some(RetryPolicy {
                enabled: true,
                max_retries: 1,
                base_delay_ms: 0,
            }),
            ..base_options(
                &models,
                as_dyn_session(&session),
                faux.get_model(None).expect("faux model"),
            )
        })
        .expect("harness"),
    );
    let (retry_listener, retry_events) = retry_event_collector();
    let _unsubscribe = harness.subscribe(retry_listener);

    let result = harness.compact(None).await.expect("compact");

    assert!(result.summary.contains("Recovered summary"));
    assert_eq!(faux.call_count(), 2);
    assert_eq!(
        *retry_events.lock().expect("retry events"),
        vec![
            "retry_scheduled:compaction",
            "retry_attempt_start:compaction",
            "retry_finished:compaction",
        ]
    );
}

/// "does not retry non-retryable compaction errors" (:653-686).
#[tokio::test]
async fn test_compaction_does_not_retry_non_retryable_errors() {
    tokio::time::timeout(
        TEST_TIMEOUT,
        test_compaction_does_not_retry_non_retryable_errors_body(),
    )
    .await
    .expect("test_compaction_does_not_retry_non_retryable_errors timed out");
}

async fn test_compaction_does_not_retry_non_retryable_errors_body() {
    let (models, faux) = new_faux(FauxProviderOptions::default());
    faux.set_responses(vec![error_response("insufficient_quota")]);
    let session = seed_two_message_session().await;
    let harness = Arc::new(
        AgentHarness::new(AgentHarnessOptions {
            retry: Some(RetryPolicy {
                enabled: true,
                max_retries: 1,
                base_delay_ms: 0,
            }),
            ..base_options(
                &models,
                as_dyn_session(&session),
                faux.get_model(None).expect("faux model"),
            )
        })
        .expect("harness"),
    );
    let (retry_listener, retry_events) = retry_event_collector();
    let _unsubscribe = harness.subscribe(retry_listener);

    let error = harness.compact(None).await.expect_err("compact fails");
    assert!(error.message.contains("insufficient_quota"));
    assert_eq!(faux.call_count(), 1);
    assert!(retry_events.lock().expect("retry events").is_empty());
}

/// "exhausts transient compaction retries after maxRetries failures"
/// (:688-729).
#[tokio::test]
async fn test_compaction_exhausts_transient_retries() {
    tokio::time::timeout(
        TEST_TIMEOUT,
        test_compaction_exhausts_transient_retries_body(),
    )
    .await
    .expect("test_compaction_exhausts_transient_retries timed out");
}

async fn test_compaction_exhausts_transient_retries_body() {
    let (models, faux) = new_faux(FauxProviderOptions::default());
    faux.set_responses((0..4).map(|_| error_response("terminated")).collect());
    let session = seed_two_message_session().await;
    let harness = Arc::new(
        AgentHarness::new(AgentHarnessOptions {
            retry: Some(RetryPolicy {
                enabled: true,
                max_retries: 3,
                base_delay_ms: 0,
            }),
            ..base_options(
                &models,
                as_dyn_session(&session),
                faux.get_model(None).expect("faux model"),
            )
        })
        .expect("harness"),
    );
    let (retry_listener, retry_events) = retry_event_collector();
    let _unsubscribe = harness.subscribe(retry_listener);

    let error = harness.compact(None).await.expect_err("compact fails");
    assert!(error.message.contains("terminated"));
    assert_eq!(faux.call_count(), 4);
    assert_eq!(
        *retry_events.lock().expect("retry events"),
        vec![
            "retry_scheduled:compaction",
            "retry_attempt_start:compaction",
            "retry_scheduled:compaction",
            "retry_attempt_start:compaction",
            "retry_scheduled:compaction",
            "retry_attempt_start:compaction",
            "retry_finished:compaction",
        ]
    );
}

/// Branch fixture (:744-748): target user message + reply, then an abandoned
/// pair. Returns (session, target_id).
async fn seed_branch_session() -> (Arc<SessionFacade<SessionMetadata>>, String) {
    let session = new_session();
    let target_id = session
        .append_message(create_user_message("first branch"))
        .await
        .expect("append");
    session
        .append_message(create_assistant_message("first reply"))
        .await
        .expect("append");
    session
        .append_message(create_user_message("abandoned work"))
        .await
        .expect("append");
    session
        .append_message(create_assistant_message("abandoned reply"))
        .await
        .expect("append");
    (session, target_id)
}

/// "retries transient branch summary errors and emits retry events"
/// (:731-775).
#[tokio::test]
async fn test_branch_summary_retries_transient_errors_and_emits_events() {
    tokio::time::timeout(
        TEST_TIMEOUT,
        test_branch_summary_retries_transient_errors_and_emits_events_body(),
    )
    .await
    .expect("test_branch_summary_retries_transient_errors_and_emits_events timed out");
}

async fn test_branch_summary_retries_transient_errors_and_emits_events_body() {
    let (models, faux) = new_faux(FauxProviderOptions::default());
    faux.set_responses(vec![
        error_response("terminated"),
        faux_assistant_message("## Goal\nRecovered branch summary", Default::default()).into(),
    ]);
    let (session, target_id) = seed_branch_session().await;
    let harness = Arc::new(
        AgentHarness::new(AgentHarnessOptions {
            retry: Some(RetryPolicy {
                enabled: true,
                max_retries: 1,
                base_delay_ms: 0,
            }),
            ..base_options(
                &models,
                as_dyn_session(&session),
                faux.get_model(None).expect("faux model"),
            )
        })
        .expect("harness"),
    );
    let (retry_listener, retry_events) = retry_event_collector();
    let _unsubscribe = harness.subscribe(retry_listener);

    let result = harness
        .navigate_tree(
            &target_id,
            rpi_agent::harness::NavigateTreeOptions {
                summarize: true,
                ..Default::default()
            },
        )
        .await
        .expect("navigate");

    let summary_entry = result.summary_entry.expect("summary entry");
    assert!(summary_entry.summary.contains("Recovered branch summary"));
    assert_eq!(faux.call_count(), 2);
    assert_eq!(
        *retry_events.lock().expect("retry events"),
        vec![
            "retry_scheduled:branch_summary",
            "retry_attempt_start:branch_summary",
            "retry_finished:branch_summary",
        ]
    );
}

/// "persists generated branch summary usage" (:778-795).
#[tokio::test]
async fn test_branch_summary_persists_generated_usage() {
    tokio::time::timeout(
        TEST_TIMEOUT,
        test_branch_summary_persists_generated_usage_body(),
    )
    .await
    .expect("test_branch_summary_persists_generated_usage timed out");
}

async fn test_branch_summary_persists_generated_usage_body() {
    let (models, faux) = new_faux(FauxProviderOptions::default());
    faux.set_responses(vec![faux_assistant_message(
        "## Goal\nBranch summary",
        Default::default(),
    )
    .into()]);
    let (session, target_id) = seed_branch_session().await;
    let harness = Arc::new(
        AgentHarness::new(base_options(
            &models,
            as_dyn_session(&session),
            faux.get_model(None).expect("faux model"),
        ))
        .expect("harness"),
    );

    let result = harness
        .navigate_tree(
            &target_id,
            rpi_agent::harness::NavigateTreeOptions {
                summarize: true,
                ..Default::default()
            },
        )
        .await
        .expect("navigate");

    let usage = result
        .summary_entry
        .and_then(|entry| entry.usage)
        .expect("summary usage");
    assert!(usage.total_tokens > 0);
}

/// "persists hook-provided branch summary usage" (:797-817).
#[tokio::test]
async fn test_branch_summary_persists_hook_provided_usage() {
    tokio::time::timeout(
        TEST_TIMEOUT,
        test_branch_summary_persists_hook_provided_usage_body(),
    )
    .await
    .expect("test_branch_summary_persists_hook_provided_usage timed out");
}

async fn test_branch_summary_persists_hook_provided_usage_body() {
    let (models, faux) = new_faux(FauxProviderOptions::default());
    let usage = create_usage(13, 14, 15, 16);
    let (session, target_id) = seed_branch_session().await;
    let harness = Arc::new(
        AgentHarness::new(base_options(
            &models,
            as_dyn_session(&session),
            faux.get_model(None).expect("faux model"),
        ))
        .expect("harness"),
    );
    let _unsubscribe = harness.on("session_before_tree", {
        let usage = usage.clone();
        hook(move |_event| {
            let usage = usage.clone();
            async move {
                Ok(rpi_agent::harness::HarnessHookResult::SessionBeforeTree(
                    Some(rpi_agent::harness::types::SessionBeforeTreeResult {
                        summary: Some(rpi_agent::harness::TreeSummary {
                            summary: "hook branch summary".to_owned(),
                            details: None,
                            usage: Some(usage),
                        }),
                        ..Default::default()
                    }),
                ))
            }
        })
    });

    let result = harness
        .navigate_tree(
            &target_id,
            rpi_agent::harness::NavigateTreeOptions {
                summarize: true,
                ..Default::default()
            },
        )
        .await
        .expect("navigate");

    let entry_usage = result.summary_entry.and_then(|entry| entry.usage);
    assert_eq!(entry_usage, Some(usage));
}

// ---------------------------------------------------------------------------
// Tools/resources getters and update events (:819-956)
// ---------------------------------------------------------------------------

/// "preserves app tool types for getters and update events" (:819-887).
/// Rust trait objects carry no app subtype; names stand in for `source`.
#[tokio::test]
async fn test_tools_getters_and_update_events() {
    tokio::time::timeout(TEST_TIMEOUT, test_tools_getters_and_update_events_body())
        .await
        .expect("test_tools_getters_and_update_events timed out");
}

async fn test_tools_getters_and_update_events_body() {
    let (models, faux) = new_faux(FauxProviderOptions::default());
    let session = new_session();
    let model = faux.get_model(None).expect("faux model");
    let harness = Arc::new(
        AgentHarness::new(AgentHarnessOptions {
            tools: vec![
                Arc::new(NamedTool::new("inspect")),
                Arc::new(NamedTool::new("search")),
            ],
            active_tool_names: Some(vec!["inspect".to_owned()]),
            ..base_options(&models, as_dyn_session(&session), model)
        })
        .expect("harness"),
    );
    #[derive(Debug, PartialEq)]
    struct ToolsUpdate {
        tool_names: Vec<String>,
        previous_tool_names: Vec<String>,
        active_tool_names: Vec<String>,
        previous_active_tool_names: Vec<String>,
        source: UpdateSource,
    }
    let updates: Arc<Mutex<Vec<ToolsUpdate>>> = Arc::new(Mutex::new(Vec::new()));
    let _unsubscribe = harness.subscribe({
        let harness = Arc::clone(&harness);
        let updates = Arc::clone(&updates);
        listener(move |event, _signal| {
            let harness = Arc::clone(&harness);
            let updates = Arc::clone(&updates);
            async move {
                if let AgentHarnessEvent::Harness(AgentHarnessOwnEvent::ToolsUpdate {
                    tool_names,
                    previous_tool_names,
                    active_tool_names,
                    previous_active_tool_names,
                    source,
                }) = &event
                {
                    updates.lock().expect("updates").push(ToolsUpdate {
                        tool_names: tool_names.clone(),
                        previous_tool_names: previous_tool_names.clone(),
                        active_tool_names: active_tool_names.clone(),
                        previous_active_tool_names: previous_active_tool_names.clone(),
                        source: *source,
                    });
                    assert_eq!(
                        harness
                            .get_active_tools()
                            .iter()
                            .map(|tool| tool.name().to_owned())
                            .collect::<Vec<_>>(),
                        *active_tool_names
                    );
                }
                Ok(())
            }
        })
    });

    // Getters return copies; mutating them must not affect the harness.
    let mut tools = harness.get_tools();
    let mut active_tools = harness.get_active_tools();
    tools.pop();
    active_tools.pop();
    assert_eq!(tool_names(&harness.get_tools()), vec!["inspect", "search"]);
    assert_eq!(tool_names(&harness.get_active_tools()), vec!["inspect"]);

    harness
        .set_active_tools(vec!["search".to_owned()])
        .await
        .expect("set active");
    harness
        .set_tools(
            vec![Arc::new(NamedTool::new("search"))],
            Some(vec!["search".to_owned()]),
        )
        .await
        .expect("set tools");
    let missing = harness.set_active_tools(vec!["missing".to_owned()]).await;
    assert_eq!(
        missing.expect_err("unknown tool").code,
        AgentHarnessErrorCode::InvalidArgument
    );
    let duplicate = harness
        .set_active_tools(vec!["search".to_owned(), "search".to_owned()])
        .await;
    assert_eq!(
        duplicate.expect_err("duplicate").code,
        AgentHarnessErrorCode::InvalidArgument
    );
    let missing_active = harness
        .set_tools(vec![Arc::new(NamedTool::new("inspect"))], None)
        .await;
    assert_eq!(
        missing_active.expect_err("active not in tools").code,
        AgentHarnessErrorCode::InvalidArgument
    );
    let duplicate_tools = harness
        .set_tools(
            vec![
                Arc::new(NamedTool::new("inspect")),
                Arc::new(NamedTool::new("inspect")),
            ],
            Some(vec!["inspect".to_owned()]),
        )
        .await;
    assert_eq!(
        duplicate_tools.expect_err("duplicate tools").code,
        AgentHarnessErrorCode::InvalidArgument
    );

    assert_eq!(
        *updates.lock().expect("updates"),
        vec![
            ToolsUpdate {
                tool_names: vec!["inspect".to_owned(), "search".to_owned()],
                previous_tool_names: vec!["inspect".to_owned(), "search".to_owned()],
                active_tool_names: vec!["search".to_owned()],
                previous_active_tool_names: vec!["inspect".to_owned()],
                source: UpdateSource::Set,
            },
            ToolsUpdate {
                tool_names: vec!["search".to_owned()],
                previous_tool_names: vec!["inspect".to_owned(), "search".to_owned()],
                active_tool_names: vec!["search".to_owned()],
                previous_active_tool_names: vec!["search".to_owned()],
                source: UpdateSource::Set,
            },
        ]
    );
    assert_eq!(tool_names(&harness.get_tools()), vec!["search"]);
    assert_eq!(tool_names(&harness.get_active_tools()), vec!["search"]);
    let context = session
        .build_context(SessionContextBuildOptions::default())
        .await
        .expect("context");
    assert_eq!(context.active_tool_names, Some(vec!["search".to_owned()]));
}

/// "validates constructor tool names" (:889-915).
#[tokio::test]
async fn test_constructor_validates_tool_names() {
    tokio::time::timeout(TEST_TIMEOUT, test_constructor_validates_tool_names_body())
        .await
        .expect("test_constructor_validates_tool_names timed out");
}

async fn test_constructor_validates_tool_names_body() {
    let (models, faux) = new_faux(FauxProviderOptions::default());
    let model = faux.get_model(None).expect("faux model");

    let unknown = AgentHarness::new(AgentHarnessOptions {
        tools: vec![Arc::new(CalculateTool::new())],
        active_tool_names: Some(vec!["missing".to_owned()]),
        ..base_options(&models, as_dyn_session(&new_session()), model.clone())
    });
    let error = unknown.map(|_| ()).expect_err("unknown tool");
    assert!(error.message.contains("Unknown tool"));

    let duplicate_tool = AgentHarness::new(AgentHarnessOptions {
        tools: vec![
            Arc::new(CalculateTool::new()),
            Arc::new(CalculateTool::new()),
        ],
        active_tool_names: Some(vec!["calculate".to_owned()]),
        ..base_options(&models, as_dyn_session(&new_session()), model.clone())
    });
    let error = duplicate_tool.map(|_| ()).expect_err("duplicate tool");
    assert!(error.message.contains("Duplicate tool"));

    let duplicate_active = AgentHarness::new(AgentHarnessOptions {
        tools: vec![Arc::new(CalculateTool::new())],
        active_tool_names: Some(vec!["calculate".to_owned(), "calculate".to_owned()]),
        ..base_options(&models, as_dyn_session(&new_session()), model)
    });
    let error = duplicate_active.map(|_| ()).expect_err("duplicate active");
    assert!(error.message.contains("Duplicate active tool"));
}

/// "preserves app resource types for getters and update events" (:917-956).
/// Names/fields stand in for the app-specific `source` discriminator.
#[tokio::test]
async fn test_resources_getters_and_update_events() {
    tokio::time::timeout(
        TEST_TIMEOUT,
        test_resources_getters_and_update_events_body(),
    )
    .await
    .expect("test_resources_getters_and_update_events timed out");
}

async fn test_resources_getters_and_update_events_body() {
    let (models, faux) = new_faux(FauxProviderOptions::default());
    let harness = Arc::new(
        AgentHarness::new(base_options(
            &models,
            as_dyn_session(&new_session()),
            faux.get_model(None).expect("faux model"),
        ))
        .expect("harness"),
    );
    let skill = rpi_agent::harness::Skill {
        name: "inspect".to_owned(),
        description: "Inspect things".to_owned(),
        content: "Use inspection tools.".to_owned(),
        file_path: "/skills/inspect/SKILL.md".to_owned(),
        disable_model_invocation: None,
    };
    let prompt_template = rpi_agent::harness::PromptTemplate {
        name: "review".to_owned(),
        description: None,
        content: "Review $1".to_owned(),
    };
    let resources = AgentHarnessResources {
        skills: Some(vec![skill.clone()]),
        prompt_templates: Some(vec![prompt_template.clone()]),
    };
    #[derive(Debug, PartialEq)]
    struct ResourcesUpdate {
        resources_skill: Option<String>,
        previous_skill: Option<String>,
    }
    let updates: Arc<Mutex<Vec<ResourcesUpdate>>> = Arc::new(Mutex::new(Vec::new()));
    let _unsubscribe = harness.subscribe({
        let updates = Arc::clone(&updates);
        listener(move |event, _signal| {
            let updates = Arc::clone(&updates);
            async move {
                if let AgentHarnessEvent::Harness(AgentHarnessOwnEvent::ResourcesUpdate {
                    resources,
                    previous_resources,
                }) = &event
                {
                    updates.lock().expect("updates").push(ResourcesUpdate {
                        resources_skill: resources
                            .skills
                            .as_ref()
                            .and_then(|skills| skills.first().map(|skill| skill.name.clone())),
                        previous_skill: previous_resources
                            .skills
                            .as_ref()
                            .and_then(|skills| skills.first().map(|skill| skill.name.clone())),
                    });
                }
                Ok(())
            }
        })
    });

    harness.set_resources(resources.clone()).await.expect("set");
    harness
        .set_resources(resources.clone())
        .await
        .expect("set again");
    let mut resolved = harness.get_resources();
    // Cloned vectors: mutation must not leak into the harness.
    if let Some(skills) = resolved.skills.as_mut() {
        skills.pop();
    }

    assert_eq!(
        *updates.lock().expect("updates"),
        vec![
            ResourcesUpdate {
                resources_skill: Some("inspect".to_owned()),
                previous_skill: None,
            },
            ResourcesUpdate {
                resources_skill: Some("inspect".to_owned()),
                previous_skill: Some("inspect".to_owned()),
            },
        ]
    );
    let resolved = harness.get_resources();
    assert_eq!(resolved.skills.as_ref().map(|skills| skills.len()), Some(1));
    assert_eq!(
        resolved
            .skills
            .as_ref()
            .and_then(|skills| skills.first())
            .map(|skill| skill.file_path.as_str()),
        Some("/skills/inspect/SKILL.md")
    );
    assert_eq!(
        resolved
            .prompt_templates
            .as_ref()
            .and_then(|templates| templates.first())
            .map(|template| template.content.as_str()),
        Some("Review $1")
    );
}

// ---------------------------------------------------------------------------
// Failure-path anchors (not in the upstream suite — the pinned upstream tests
// cover no failure paths; these follow the pinned *implementation* semantics
// of agent-harness.ts:430-440, :544-556, :623-655, :707-722).
// ---------------------------------------------------------------------------

/// `SessionStorage` wrapper injecting persistence failures:
/// `FailingMode::All` fails every `append_entry` (the run's own persistence
/// fails), `FailingMode::CustomMessages` fails only custom-message entries
/// (the staged writes of harness listeners).
enum FailingMode {
    All,
    CustomMessages,
}

struct FailingAppendStorage {
    inner: InMemorySessionStorage,
    mode: FailingMode,
}

#[async_trait]
impl SessionStorage for FailingAppendStorage {
    type Metadata = SessionMetadata;

    async fn get_metadata(&self) -> Result<Self::Metadata, SessionError> {
        self.inner.get_metadata().await
    }

    async fn get_leaf_id(&self) -> Result<Option<String>, SessionError> {
        self.inner.get_leaf_id().await
    }

    async fn set_leaf_id(&self, leaf_id: Option<String>) -> Result<(), SessionError> {
        self.inner.set_leaf_id(leaf_id).await
    }

    async fn create_entry_id(&self) -> Result<String, SessionError> {
        self.inner.create_entry_id().await
    }

    async fn append_entry(&self, entry: SessionEntry) -> Result<(), SessionError> {
        let fails = match self.mode {
            FailingMode::All => true,
            FailingMode::CustomMessages => matches!(
                &entry,
                SessionEntry::Message(MessageEntry {
                    message: AgentMessage::Custom(_),
                    ..
                })
            ),
        };
        if fails {
            return Err(SessionError::new(
                SessionErrorCode::Storage,
                "append failed",
            ));
        }
        self.inner.append_entry(entry).await
    }

    async fn get_entry(&self, id: &str) -> Result<Option<SessionEntry>, SessionError> {
        self.inner.get_entry(id).await
    }

    async fn find_entries(&self, entry_type: &str) -> Result<Vec<SessionEntry>, SessionError> {
        self.inner.find_entries(entry_type).await
    }

    async fn get_label(&self, id: &str) -> Result<Option<String>, SessionError> {
        self.inner.get_label(id).await
    }

    async fn get_session_name(&self) -> Result<Option<String>, SessionError> {
        self.inner.get_session_name().await
    }

    async fn get_session_stats(&self) -> Result<SessionStats, SessionError> {
        self.inner.get_session_stats().await
    }

    async fn get_path_to_root_or_compaction(
        &self,
        leaf_id: Option<&str>,
    ) -> Result<Vec<SessionEntry>, SessionError> {
        self.inner.get_path_to_root_or_compaction(leaf_id).await
    }

    async fn get_entries(
        &self,
        options: SessionEntryCursorOptions,
    ) -> Result<Vec<SessionEntry>, SessionError> {
        self.inner.get_entries(options).await
    }
}

/// A failed `queue_update` notification rolls the dequeued steer message back
/// into the queue head and fails the run with the hook error
/// (agent-harness.ts:430-440: `drainQueuedMessages` catches the emit error,
/// `queue.unshift(...messages)` and rethrows `normalizeHookError(error)`).
#[tokio::test]
async fn test_drain_failure_requeues_steer_message_and_fails_run() {
    tokio::time::timeout(
        TEST_TIMEOUT,
        test_drain_failure_requeues_steer_message_and_fails_run_body(),
    )
    .await
    .expect("test_drain_failure_requeues_steer_message_and_fails_run timed out");
}

async fn test_drain_failure_requeues_steer_message_and_fails_run_body() {
    let (models, faux) = new_faux(FauxProviderOptions::default());
    faux.set_responses(vec![faux_assistant_message("ok", Default::default()).into()]);
    let harness = Arc::new(
        AgentHarness::new(base_options(
            &models,
            as_dyn_session(&new_session()),
            faux.get_model(None).expect("faux model"),
        ))
        .expect("harness"),
    );
    let queue_lengths: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
    let fail_queue_update = Arc::new(AtomicBool::new(false));
    let _unsubscribe = harness.subscribe({
        let queue_lengths = Arc::clone(&queue_lengths);
        let fail_queue_update = Arc::clone(&fail_queue_update);
        listener(move |event, _signal| {
            let queue_lengths = Arc::clone(&queue_lengths);
            let fail_queue_update = Arc::clone(&fail_queue_update);
            async move {
                if let AgentHarnessEvent::Harness(AgentHarnessOwnEvent::QueueUpdate {
                    steer, ..
                }) = &event
                {
                    queue_lengths
                        .lock()
                        .expect("queue lengths")
                        .push(steer.len());
                    if fail_queue_update.swap(false, Ordering::SeqCst) {
                        return Err(AgentHarnessError::new(
                            AgentHarnessErrorCode::Hook,
                            "queue update exploded",
                        ));
                    }
                }
                Ok(())
            }
        })
    });
    let steer_result: Arc<Mutex<Option<Result<(), AgentHarnessError>>>> =
        Arc::new(Mutex::new(None));
    let steered = Arc::new(AtomicBool::new(false));
    let _unsubscribe = harness.subscribe({
        let harness = Arc::clone(&harness);
        let steer_result = Arc::clone(&steer_result);
        let steered = Arc::clone(&steered);
        let fail_queue_update = Arc::clone(&fail_queue_update);
        listener(move |event, _signal| {
            let harness = Arc::clone(&harness);
            let steer_result = Arc::clone(&steer_result);
            let steered = Arc::clone(&steered);
            let fail_queue_update = Arc::clone(&fail_queue_update);
            async move {
                if let AgentHarnessEvent::Agent(AgentEvent::MessageStart { message }) = &event {
                    if matches!(message, AgentMessage::Assistant(_))
                        && !steered.swap(true, Ordering::SeqCst)
                    {
                        // The push's own notification must succeed; arm the
                        // failure only for the drain that follows.
                        *steer_result.lock().expect("steer result") =
                            Some(harness.steer("one", None).await);
                        fail_queue_update.store(true, Ordering::SeqCst);
                    }
                }
                Ok(())
            }
        })
    });

    let response = harness.prompt("hello", None).await.expect("prompt settles");

    // The failed drain leaves no steering message for a further turn, so the
    // loop ends without another provider call; the recorded hook failure is
    // trailing and `emitRunFailure` replays it as the assistant error message
    // (agent_harness.rs:1622-1637, file header).
    assert_eq!(response.stop_reason, StopReason::Error);
    assert_eq!(
        response.error_message.as_deref(),
        Some("queue update exploded")
    );
    assert!(steer_result
        .lock()
        .expect("steer result")
        .as_ref()
        .expect("steered during the run")
        .is_ok());
    // The drain removed the message (its notification carried the empty
    // queue) and then rolled it back into the queue head: an idle `abort`
    // returns the restored message.
    assert_eq!(*queue_lengths.lock().expect("queue lengths"), vec![1, 0]);
    let abort = harness.abort().await.expect("abort");
    assert_eq!(abort.cleared_steer.len(), 1);
    match &abort.cleared_steer[0] {
        AgentMessage::User(user) => assert_eq!(user_message_text(user), "one"),
        other => panic!("expected the restored steer message, got {other:?}"),
    }
}

/// `steer` / `followUp` reject idle phases with `invalid_state` while
/// `nextTurn` is queueable in any phase and its message splices in front of
/// the next prompt (agent-harness.ts:707-722, :588-597).
#[tokio::test]
async fn test_steer_and_follow_up_rejected_while_idle_next_turn_queueable() {
    tokio::time::timeout(
        TEST_TIMEOUT,
        test_steer_and_follow_up_rejected_while_idle_next_turn_queueable_body(),
    )
    .await
    .expect("test_steer_and_follow_up_rejected_while_idle_next_turn_queueable timed out");
}

async fn test_steer_and_follow_up_rejected_while_idle_next_turn_queueable_body() {
    let (models, faux) = new_faux(FauxProviderOptions::default());
    let request_text: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    faux.set_responses(vec![FauxResponseStep::Factory({
        let request_text = Arc::clone(&request_text);
        Box::new(move |context, _options, _state, _model| {
            *request_text.lock().expect("request text") = text_from_user_messages(context);
            faux_assistant_message("ok", FauxAssistantOptions::default())
        })
    })]);
    let harness = Arc::new(
        AgentHarness::new(base_options(
            &models,
            as_dyn_session(&new_session()),
            faux.get_model(None).expect("faux model"),
        ))
        .expect("harness"),
    );

    let steer_error = harness
        .steer("steer", None)
        .await
        .expect_err("steer while idle");
    assert_eq!(steer_error.code, AgentHarnessErrorCode::InvalidState);
    assert_eq!(steer_error.message, "Cannot steer while idle");
    let follow_up_error = harness
        .follow_up("follow", None)
        .await
        .expect_err("follow up while idle");
    assert_eq!(follow_up_error.code, AgentHarnessErrorCode::InvalidState);
    assert_eq!(follow_up_error.message, "Cannot follow up while idle");

    // nextTurn has no phase gate; queueing does not leave the idle phase.
    harness
        .next_turn("next", None)
        .await
        .expect("next turn while idle");
    assert!(harness.steer("steer", None).await.is_err());
    assert!(harness.follow_up("follow", None).await.is_err());

    harness.prompt("hello", None).await.expect("prompt");
    assert_eq!(
        *request_text.lock().expect("request text"),
        vec!["next", "hello"]
    );
}

/// A run failure whose `emitRunFailure` replay also fails aggregates into one
/// `unknown` error carrying the AggregateError message (agent-harness.ts:631-637;
/// agent_harness.rs:1628-1635). The failing storage makes the run's first
/// persisted append fail (the primary failure) and the replay's own
/// `message_end` append fail again (the reporting failure).
#[tokio::test]
async fn test_failed_failure_reporting_aggregates_unknown_error() {
    tokio::time::timeout(
        TEST_TIMEOUT,
        test_failed_failure_reporting_aggregates_unknown_error_body(),
    )
    .await
    .expect("test_failed_failure_reporting_aggregates_unknown_error timed out");
}

async fn test_failed_failure_reporting_aggregates_unknown_error_body() {
    let (models, faux) = new_faux(FauxProviderOptions::default());
    faux.set_responses(vec![faux_assistant_message(
        "should not be used",
        Default::default(),
    )
    .into()]);
    let storage = FailingAppendStorage {
        inner: InMemorySessionStorage::new(InMemorySessionStorageOptions::default())
            .expect("in-memory storage"),
        mode: FailingMode::All,
    };
    let session = Arc::new(SessionFacade::new(
        Arc::new(storage) as Arc<dyn SessionStorage<Metadata = SessionMetadata>>,
        SessionContextBuildOptions::default(),
    ));
    let harness = Arc::new(
        AgentHarness::new(base_options(
            &models,
            as_dyn_session(&session),
            faux.get_model(None).expect("faux model"),
        ))
        .expect("harness"),
    );

    let error = harness
        .prompt("hello", None)
        .await
        .expect_err("aggregate error");
    assert_eq!(error.code, AgentHarnessErrorCode::Unknown);
    assert_eq!(
        error.message,
        "Agent run failed and failure reporting failed"
    );
}

/// A failing `finally` flush propagates from the turn directly, overriding
/// the run failure (agent-harness.ts:649-655; agent_harness.rs:1559-1567 —
/// `(Err(_), Err(error)) => return Err(error)`). The run fails when the
/// staged listener write cannot flush at `turn_end`; the trailing
/// `emitRunFailure` replay fails for the same reason, but the finally flush
/// error wins — the surface error is the `session` code, not the aggregated
/// `unknown`. (The `(Ok(_), Err(error))` branch is structurally unreachable:
/// every write staged during a run is flushed by the `turn_end`/`agent_end`
/// handlers, and `agent_end` sets the phase to idle before its emissions, so
/// the finally flush only ever fails on top of an already-failed run.)
#[tokio::test]
async fn test_finally_flush_failure_overrides_run_failure() {
    tokio::time::timeout(
        TEST_TIMEOUT,
        test_finally_flush_failure_overrides_run_failure_body(),
    )
    .await
    .expect("test_finally_flush_failure_overrides_run_failure timed out");
}

async fn test_finally_flush_failure_overrides_run_failure_body() {
    let (models, faux) = new_faux(FauxProviderOptions::default());
    faux.set_responses(vec![faux_assistant_message("ok", Default::default()).into()]);
    let storage = FailingAppendStorage {
        inner: InMemorySessionStorage::new(InMemorySessionStorageOptions::default())
            .expect("in-memory storage"),
        mode: FailingMode::CustomMessages,
    };
    let session = Arc::new(SessionFacade::new(
        Arc::new(storage) as Arc<dyn SessionStorage<Metadata = SessionMetadata>>,
        SessionContextBuildOptions::default(),
    ));
    let harness = Arc::new(
        AgentHarness::new(base_options(
            &models,
            as_dyn_session(&session),
            faux.get_model(None).expect("faux model"),
        ))
        .expect("harness"),
    );
    let staged = Arc::new(AtomicBool::new(false));
    let _unsubscribe = harness.subscribe({
        let harness = Arc::clone(&harness);
        let staged = Arc::clone(&staged);
        listener(move |event, _signal| {
            let harness = Arc::clone(&harness);
            let staged = Arc::clone(&staged);
            async move {
                // `turn_end` emits before the harness flushes pending writes
                // (agent-harness.ts:544-556), so the staged write is pending
                // for that flush.
                if matches!(event, AgentHarnessEvent::Agent(AgentEvent::TurnEnd { .. }))
                    && !staged.swap(true, Ordering::SeqCst)
                {
                    harness
                        .append_message(AgentMessage::Custom(CustomMessage {
                            role: CustomRole::Custom,
                            custom_type: "listener".to_owned(),
                            content: UserContent::Text("listener write".to_owned()),
                            display: true,
                            details: None,
                            timestamp: 3,
                        }))
                        .await?;
                }
                Ok(())
            }
        })
    });

    let error = harness
        .prompt("hello", None)
        .await
        .expect_err("flush error wins");
    assert_eq!(error.code, AgentHarnessErrorCode::Session);
    assert_eq!(error.message, "append failed");
    // The failed turn still restored the idle phase (agent-harness.ts:665-667).
    let steer_error = harness.steer("steer", None).await.expect_err("idle again");
    assert_eq!(steer_error.code, AgentHarnessErrorCode::InvalidState);
}
