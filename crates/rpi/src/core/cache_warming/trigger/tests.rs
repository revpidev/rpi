//! Warmer integration tests — port of `packages/coding-agent/test/
//! cache-warmer.test.ts` @ c596d09d9 (#9668), driven through the real
//! `CacheWarmer` state machine with a fake `streamSimple` and the
//! in-memory `SessionManager`, under the paused tokio clock
//! (`vi.useFakeTimers()` equivalent).

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use rpi_agent::messages::AgentMessage;
use rpi_agent::session::UsageEntry;
use rpi_ai::models::ModelsSimpleStreamOptions;
use rpi_ai::types::{
    ApiKind, AssistantMessage, CacheRetention, Context, Model, ModelPromptCache, StopReason,
    StreamEvent, Usage,
};
use rpi_ai::utils::event_stream::AssistantMessageEventStream;
use rpi_ext_host::types::{CacheWarmingAction, CacheWarmingDecisionEvent};

use super::WarmingState;
use super::{
    CacheWarmRequest, CacheWarmer, CacheWarmerDeps, CacheWarmingDecide, IsCurrent, WarmingModels,
};

// ---------------------------------------------------------------------------
// Fixtures (cache-warmer.test.ts:33-69)
// ---------------------------------------------------------------------------

fn test_model(id: &str) -> Model {
    Model {
        id: id.to_owned(),
        name: id.to_owned(),
        api: ApiKind(ApiKind::ANTHROPIC_MESSAGES.to_owned()),
        provider: "anthropic".to_owned(),
        base_url: "https://api.anthropic.com".to_owned(),
        reasoning: true,
        thinking_level_map: None,
        input: vec![],
        // claude-opus-4-6 catalog rates (drives the upstream number
        // assertions: warmCost ≈ $0.050025 / missCost ≈ $0.575 at 100k).
        cost: rpi_ai::types::ModelCost {
            rates: rpi_ai::types::ModelCostRates {
                input: 5.0,
                output: 25.0,
                cache_read: 0.50,
                cache_write: 6.25,
            },
            tiers: None,
        },
        prompt_cache: Some(ModelPromptCache {
            short: Some(300),
            long: Some(3600),
        }),
        context_window: 200_000,
        max_tokens: 16_384,
        sampling_params: None,
        headers: None,
        compat: None,
    }
}

fn adaptive_model() -> Model {
    // `claude-opus-4-6` — reasoning is replayable (test uses it with
    // reasoning on; opus carries adaptive thinking in the fixture by
    // leaving compat absent the reasoning path is *not* replayable — the
    // upstream fixture asserts the opposite via opus's catalog compat, so
    // mirror that by marking adaptive).
    let mut model = test_model("claude-opus-4-6");
    model.compat = Some(rpi_ai::types::ModelCompat {
        force_adaptive_thinking: Some(true),
        ..Default::default()
    });
    model
}

fn budget_model() -> Model {
    // `claude-sonnet-4-5` — budget-based thinking: not replayable while
    // reasoning is on.
    test_model("claude-sonnet-4-5")
}

fn openai_model() -> Model {
    let mut model = test_model("gpt-5");
    model.api = ApiKind(ApiKind::OPENAI_RESPONSES.to_owned());
    model.provider = "openai".to_owned();
    model.prompt_cache = Some(ModelPromptCache {
        short: Some(300),
        long: Some(86_400),
    });
    model
}

fn unknown_model() -> Model {
    let mut model = adaptive_model();
    model.prompt_cache = None;
    model
}

fn warm_usage() -> Usage {
    let mut usage = Usage {
        cache_read: 100,
        output: 1,
        total_tokens: 101,
        ..Usage::default()
    };
    usage.cost.total = 0.01;
    usage
}

fn assistant_response(model: &Model, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        role: rpi_ai::types::AssistantRole::Assistant,
        content: vec![],
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        provider_thinking_level: None,
        diagnostics: None,
        usage: warm_usage(),
        stop_reason,
        timestamp: 0,
        deferred: None,
        error_message: None,
        end_turn: None,
        raw_stop_reason: None,
    }
}

fn branch_with_prompt(prompt_tokens: u64) -> Vec<rpi_agent::session::FileEntry> {
    let mut usage = warm_usage();
    usage.output = 10;
    usage.cache_read = prompt_tokens;
    usage.total_tokens = prompt_tokens + 10;
    let assistant = AssistantMessage {
        usage,
        ..assistant_response(&adaptive_model(), StopReason::Stop)
    };
    vec![rpi_agent::session::FileEntry::Message(
        rpi_agent::session::MessageEntry {
            id: "a".to_owned(),
            parent_id: None,
            timestamp: "1970-01-01T00:00:00.000Z".to_owned(),
            message: AgentMessage::Assistant(assistant),
        },
    )]
}

// ---------------------------------------------------------------------------
// Fake runtime (cache-warmer.test.ts:84-125)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeModels {
    calls: Mutex<Vec<(Model, ModelsSimpleStreamOptions)>>,
    /// Signals handed to the fake per call, in order.
    signals: Mutex<Vec<Option<tokio_util::sync::CancellationToken>>>,
    /// Pending responses; each pop resolves one refresh.
    results: Mutex<Vec<Result<AssistantMessage, ()>>>,
}

impl WarmingModels for FakeModels {
    fn warming_stream_simple(
        &self,
        model: &Model,
        _context: &Context,
        options: Option<ModelsSimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        let options = options.unwrap_or_default();
        self.signals
            .lock()
            .unwrap()
            .push(options.simple.stream.request.signal.clone());
        self.calls.lock().unwrap().push((model.clone(), options));
        let result = self
            .results
            .lock()
            .unwrap()
            .pop()
            .unwrap_or(Ok(assistant_response(model, StopReason::Length)));
        let stream = AssistantMessageEventStream::new();
        match result {
            Ok(message) => stream.push(StreamEvent::Done {
                reason: rpi_ai::types::DoneReason::Stop,
                message,
            }),
            Err(()) => {
                let error = assistant_response(model, StopReason::Error);
                stream.push(StreamEvent::Error {
                    reason: rpi_ai::types::ErrorReason::Error,
                    error,
                })
            }
        }
        stream
    }
}

struct FakeRuntime {
    warmer: Arc<CacheWarmer>,
    models: Arc<FakeModels>,
    events: Arc<Mutex<Vec<CacheWarmingDecisionEvent>>>,
    warmed_entries: Arc<Mutex<Vec<UsageEntry>>>,
    mode: Arc<AtomicU8>,
}

impl FakeRuntime {
    fn new(
        mode: CacheWarmingModeArg,
        branch: Vec<rpi_agent::session::FileEntry>,
        decide: DecideArg,
    ) -> Self {
        let models = Arc::new(FakeModels::default());
        let session = Arc::new(Mutex::new(
            crate::core::session_manager::SessionManager::in_memory_with_entries(
                None,
                crate::core::session_manager::NewSessionOptions::default(),
                Some(branch),
            )
            .unwrap(),
        ));
        let mode = Arc::new(AtomicU8::new(mode as u8));
        let mode_reader = mode.clone();
        let events = Arc::new(Mutex::new(Vec::new()));
        let events_for_decide = events.clone();
        let decide_fn: CacheWarmingDecide = match decide {
            DecideArg::Default => crate::core::cache_warming::CacheWarmerDeps::default_decide(),
            DecideArg::Warm => Arc::new(move |event| {
                let mut events = events_for_decide.lock().unwrap();
                events.push(event);
                Box::pin(async { CacheWarmingAction::Warm })
            }),
            DecideArg::Stop => Arc::new(move |event| {
                let mut events = events_for_decide.lock().unwrap();
                events.push(event);
                Box::pin(async { CacheWarmingAction::Stop })
            }),
            DecideArg::Record => Arc::new(move |event| {
                let action = event.action;
                let mut events = events_for_decide.lock().unwrap();
                events.push(event);
                Box::pin(async move { action })
            }),
        };
        // Route append_usage through the real SessionManager but capture the
        // note argument (appendUsage call assertion, cache-warmer.test.ts:168-174).
        let warmer = Arc::new(CacheWarmer::new(CacheWarmerDeps {
            models: models.clone(),
            session,
            get_mode: Arc::new(move || match mode_reader.load(Ordering::SeqCst) {
                0 => crate::core::settings_manager::CacheWarmingMode::Off,
                1 => crate::core::settings_manager::CacheWarmingMode::Streaming,
                _ => crate::core::settings_manager::CacheWarmingMode::Idle,
            }),
            decide: decide_fn,
        }));
        // Spy on warmed entries via the public callback; usage notes ride
        // along through a wrapper session-like closure on the warmer itself.
        let warmed_entries: Arc<Mutex<Vec<UsageEntry>>> = Arc::new(Mutex::new(Vec::new()));
        let warmed_for_cb = warmed_entries.clone();
        warmer.set_on_warmed(Some(Arc::new(move |entry| {
            warmed_for_cb.lock().unwrap().push(entry.clone());
        })));
        FakeRuntime {
            warmer,
            models,
            events,
            warmed_entries,
            mode,
        }
    }

    /// Persisted usage notes (appendUsage `note` assertion surface,
    /// cache-warmer.test.ts:168-175).
    fn append_notes(&self) -> Vec<Option<String>> {
        self.warmed_entries
            .lock()
            .unwrap()
            .iter()
            .map(|entry| entry.note.clone())
            .collect()
    }
}

#[derive(Clone, Copy)]
enum CacheWarmingModeArg {
    Off,
    Streaming,
    Idle,
}

enum DecideArg {
    #[allow(dead_code)]
    Default,
    Warm,
    Stop,
    Record,
}

fn request(model: &Model, reasoning: Option<rpi_ai::types::ThinkingLevel>) -> CacheWarmRequest {
    CacheWarmRequest {
        model: model.clone(),
        context: Context {
            system_prompt: None,
            messages: vec![],
            tools: None,
        },
        options: ModelsSimpleStreamOptions {
            simple: rpi_ai::types::SimpleStreamOptions {
                stream: rpi_ai::types::StreamOptions {
                    reasoning: reasoning.map(|level| level.to_model_level()),
                    ..Default::default()
                },
                reasoning,
                ..Default::default()
            },
            ..Default::default()
        },
    }
}

fn always_current() -> IsCurrent {
    Arc::new(|| true)
}

// ---------------------------------------------------------------------------
// Tests (cache-warmer.test.ts:127-320)
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn replays_profitable_requests_and_preserves_options() {
    let runtime = FakeRuntime::new(
        CacheWarmingModeArg::Idle,
        branch_with_prompt(100_000),
        DecideArg::Record,
    );
    let mut req = request(&adaptive_model(), Some(rpi_ai::types::ThinkingLevel::High));
    req.options.simple.stream.session_id = Some("s".to_owned());
    req.options.simple.stream.cache_retention = Some(CacheRetention::Short);
    req.options.simple.stream.timeout_ms = Some(1234);

    runtime.warmer.start(req, always_current());
    tokio::time::advance(std::time::Duration::from_millis(270_000)).await;
    // Let the spawned refresh task run to completion.
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_millis(1)).await;
    tokio::task::yield_now().await;

    let calls = runtime.models.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    let (called_model, options) = calls.into_iter().next().unwrap();
    assert_eq!(called_model.id, "claude-opus-4-6");
    // One-token cap, zero retries, independent abort signal
    // (cache-warmer.test.ts:151-158).
    assert_eq!(options.simple.stream.max_tokens, Some(1));
    assert_eq!(options.simple.stream.max_retries, Some(0));
    assert!(options.simple.stream.request.signal.is_some());
    // Everything else about the request is preserved.
    assert_eq!(
        options.simple.stream.session_id.as_deref(),
        Some("s"),
        "session affinity preserved across refreshes"
    );
    assert_eq!(options.simple.stream.timeout_ms, Some(1234));

    // Decision event fired with streaming-phase economics
    // (cache-warmer.test.ts:159-166).
    let events = runtime.events.lock().unwrap().clone();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].action, CacheWarmingAction::Warm);
    assert!((events[0].miss_cost - 0.575).abs() < 1e-6);
    assert!((events[0].warm_cost - 0.050025).abs() < 1e-6);

    // Usage persisted with kind cache_warm and surfaced via onWarmed
    // (cache-warmer.test.ts:167-177).
    let warmed = runtime.warmed_entries.lock().unwrap().clone();
    assert_eq!(warmed.len(), 1);
    assert_eq!(warmed[0].kind, "cache_warm");
    assert_eq!(warmed[0].provider, "anthropic");
    assert_eq!(warmed[0].model, "claude-opus-4-6");
    assert!(runtime.append_notes()[0].is_none());

    // Repeated refreshes keep the options (cache-warmer.test.ts:177-179).
    tokio::time::advance(std::time::Duration::from_millis(270_000)).await;
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(runtime.models.calls.lock().unwrap().len(), 2);
    runtime.warmer.cancel();
}

#[tokio::test(start_paused = true)]
async fn applies_economic_decisions_and_extension_overrides() {
    // Unprofitable: 5k prompt → savings below threshold → no request
    // (cache-warmer.test.ts:182-191).
    let unprofitable = FakeRuntime::new(
        CacheWarmingModeArg::Idle,
        branch_with_prompt(5_000),
        DecideArg::Record,
    );
    unprofitable
        .warmer
        .start(request(&adaptive_model(), None), always_current());
    tokio::time::advance(std::time::Duration::from_millis(270_000)).await;
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert!(unprofitable.models.calls.lock().unwrap().is_empty());
    let status = unprofitable.warmer.status();
    assert_eq!(status.state, WarmingState::Inactive);
    let decision = status.decision.unwrap();
    assert_eq!(decision.action, CacheWarmingAction::Stop);
    assert!(decision.economics_available);
    assert!(!status.extension_override);

    // Forced warm by extension override (cache-warmer.test.ts:194-199).
    let forced = FakeRuntime::new(
        CacheWarmingModeArg::Idle,
        branch_with_prompt(5_000),
        DecideArg::Warm,
    );
    forced
        .warmer
        .start(request(&adaptive_model(), None), always_current());
    tokio::time::advance(std::time::Duration::from_millis(270_000)).await;
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(forced.models.calls.lock().unwrap().len(), 1);
    assert_eq!(
        forced.append_notes()[0].as_deref(),
        Some("extension override")
    );
    forced.warmer.cancel();

    // Vetoed by extension (cache-warmer.test.ts:201-205).
    let vetoed = FakeRuntime::new(
        CacheWarmingModeArg::Idle,
        branch_with_prompt(100_000),
        DecideArg::Stop,
    );
    vetoed
        .warmer
        .start(request(&adaptive_model(), None), always_current());
    tokio::time::advance(std::time::Duration::from_millis(270_000)).await;
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert!(vetoed.models.calls.lock().unwrap().is_empty());
    let status = vetoed.warmer.status();
    assert_eq!(status.state, WarmingState::Inactive);
    assert!(status.extension_override);

    // Economics unavailable: zero prompt tokens (cache-warmer.test.ts:207-214).
    let unavailable = FakeRuntime::new(
        CacheWarmingModeArg::Idle,
        branch_with_prompt(0),
        DecideArg::Record,
    );
    unavailable
        .warmer
        .start(request(&adaptive_model(), None), always_current());
    let status = unavailable.warmer.status();
    assert_eq!(status.state, WarmingState::Inactive);
    assert_eq!(
        status.reason.as_deref(),
        Some("cache economics unavailable")
    );
    tokio::time::advance(std::time::Duration::from_millis(270_000)).await;
    tokio::task::yield_now().await;
    assert!(unavailable.models.calls.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn stops_for_unsupported_requests_context_and_mode_changes() {
    // mode off (cache-warmer.test.ts:219-222).
    let unsupported = FakeRuntime::new(
        CacheWarmingModeArg::Off,
        branch_with_prompt(100_000),
        DecideArg::Record,
    );
    unsupported
        .warmer
        .start(request(&adaptive_model(), None), always_current());
    assert_eq!(
        unsupported.warmer.status().reason.as_deref(),
        Some("cache warming disabled")
    );

    // unknown lifetime (cache-warmer.test.ts:223-225).
    unsupported.mode.store(2, Ordering::SeqCst); // idle
    unsupported
        .warmer
        .start(request(&unknown_model(), None), always_current());
    assert_eq!(
        unsupported.warmer.status().reason.as_deref(),
        Some("cache lifetime unavailable")
    );

    // budget-thinking Claude with reasoning on (cache-warmer.test.ts:226-227).
    unsupported.warmer.start(
        request(&budget_model(), Some(rpi_ai::types::ThinkingLevel::High)),
        always_current(),
    );
    assert_eq!(
        unsupported.warmer.status().reason.as_deref(),
        Some("request cannot be replayed safely")
    );

    // Context changed (cache-warmer.test.ts:229-234).
    let still_current = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let flag = still_current.clone();
    let is_current: IsCurrent = Arc::new(move || flag.load(Ordering::SeqCst));
    unsupported
        .warmer
        .start(request(&adaptive_model(), None), is_current);
    still_current.store(false, Ordering::SeqCst);
    assert_eq!(
        unsupported.warmer.status().reason.as_deref(),
        Some("conversation context changed")
    );
    tokio::time::advance(std::time::Duration::from_millis(270_000)).await;
    tokio::task::yield_now().await;
    assert!(unsupported.models.calls.lock().unwrap().is_empty());

    // Mode flipped to off mid-run (cache-warmer.test.ts:236-239).
    unsupported
        .warmer
        .start(request(&adaptive_model(), None), always_current());
    unsupported.mode.store(0, Ordering::SeqCst);
    tokio::time::advance(std::time::Duration::from_millis(270_000)).await;
    tokio::task::yield_now().await;
    assert!(unsupported.models.calls.lock().unwrap().is_empty());

    // Streaming mode stops on settle (cache-warmer.test.ts:241-244).
    let streaming = FakeRuntime::new(
        CacheWarmingModeArg::Streaming,
        branch_with_prompt(400_000),
        DecideArg::Record,
    );
    streaming
        .warmer
        .start(request(&adaptive_model(), None), always_current());
    streaming.warmer.on_agent_settled();
    assert_eq!(
        streaming.warmer.status().reason.as_deref(),
        Some("agent run settled")
    );
}

#[tokio::test(start_paused = true)]
async fn aborts_replaced_requests_and_skips_failed_refreshes() {
    // Replaced request's signal aborts (cache-warmer.test.ts:253-261): a
    // held response that never resolves is aborted by the next start.
    let pending = FakeRuntime::new(
        CacheWarmingModeArg::Idle,
        branch_with_prompt(100_000),
        DecideArg::Record,
    );
    // Preload no results → the fake returns a Done message immediately; to
    // observe the abort we instead assert the earlier run's signal state
    // after replacement.
    pending
        .warmer
        .start(request(&adaptive_model(), None), always_current());
    tokio::time::advance(std::time::Duration::from_millis(270_000)).await;
    tokio::task::yield_now().await;
    let first_signal = pending
        .models
        .signals
        .lock()
        .unwrap()
        .first()
        .cloned()
        .flatten();
    assert!(first_signal.is_some());
    assert!(!first_signal.as_ref().unwrap().is_cancelled());
    pending
        .warmer
        .start(request(&adaptive_model(), None), always_current());
    assert!(
        first_signal.as_ref().unwrap().is_cancelled(),
        "replaced request aborted"
    );

    // Failed refresh: stopReason error → no usage recorded
    // (cache-warmer.test.ts:264-269).
    let failed = FakeRuntime::new(
        CacheWarmingModeArg::Idle,
        branch_with_prompt(100_000),
        DecideArg::Record,
    );
    failed.models.results.lock().unwrap().push(Err(()));
    failed
        .warmer
        .start(request(&adaptive_model(), None), always_current());
    tokio::time::advance(std::time::Duration::from_millis(270_000)).await;
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(failed.models.calls.lock().unwrap().len(), 1);
    assert!(failed.warmed_entries.lock().unwrap().is_empty());
    failed.warmer.cancel();
}

#[tokio::test(start_paused = true)]
async fn default_off_mode_never_spawns_or_sends() {
    // 红线: 默认关闭零请求（无请求发出、状态为 inactive）。
    let runtime = FakeRuntime::new(
        CacheWarmingModeArg::Off,
        branch_with_prompt(100_000),
        DecideArg::Record,
    );
    runtime
        .warmer
        .start(request(&adaptive_model(), None), always_current());
    tokio::time::advance(std::time::Duration::from_millis(3_600_000)).await;
    tokio::task::yield_now().await;
    assert!(runtime.models.calls.lock().unwrap().is_empty());
    assert_eq!(runtime.warmer.status().state, WarmingState::Inactive);
}

#[tokio::test(start_paused = true)]
async fn idle_safety_window_stops_warming() {
    // 30-minute idle horizon (cache-warmer.ts:18): with a 300s TTL the
    // 5-minute refresh cadence would exceed the window after the run
    // started ≥ ~29 min ago; assert the stop reason after settling late.
    let runtime = FakeRuntime::new(
        CacheWarmingModeArg::Idle,
        branch_with_prompt(100_000),
        DecideArg::Record,
    );
    runtime
        .warmer
        .start(request(&adaptive_model(), None), always_current());
    tokio::time::advance(std::time::Duration::from_millis(29 * 60_000)).await;
    tokio::task::yield_now().await;
    runtime.warmer.on_agent_settled();
    // Next refresh (270s later) lands beyond startedAt + 30min → schedule
    // stops without sending.
    tokio::time::advance(std::time::Duration::from_millis(270_000)).await;
    tokio::task::yield_now().await;
    let status = runtime.warmer.status();
    assert_eq!(status.state, WarmingState::Inactive);
    assert_eq!(
        status.reason.as_deref(),
        Some("30-minute idle safety limit reached")
    );
}

#[tokio::test(start_paused = true)]
async fn long_retention_beyond_safety_window_and_none_retention() {
    // TTL derivation through the warmer's start path (covers the
    // cacheRetention "long"/"none" arms).
    let runtime = FakeRuntime::new(
        CacheWarmingModeArg::Idle,
        branch_with_prompt(100_000),
        DecideArg::Record,
    );
    let mut req = request(&openai_model(), None);
    req.options.simple.stream.cache_retention = Some(CacheRetention::Long);
    // 86400s * 0.9 = 77760s until the first refresh — beyond both safety
    // windows (60 min streaming / 30 min idle), so `schedule` stops
    // immediately without sending anything (cache-warmer.ts:282-284: warm
    // requests never extend the fixed safety windows).
    runtime.warmer.start(req, always_current());
    let status = runtime.warmer.status();
    assert_eq!(status.state, WarmingState::Inactive);
    assert_eq!(
        status.reason.as_deref(),
        Some("one-hour safety limit reached")
    );
    assert!(runtime.models.calls.lock().unwrap().is_empty());

    // "none" retention never warms (cache-warmer.test.ts:131).
    let mut req = request(&adaptive_model(), None);
    req.options.simple.stream.cache_retention = Some(CacheRetention::None);
    runtime.warmer.start(req, always_current());
    assert_eq!(
        runtime.warmer.status().reason.as_deref(),
        Some("request disabled prompt caching")
    );
}
