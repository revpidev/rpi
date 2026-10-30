//! Durable sleeping, host timers and wakeups (V18-06 FR-C/FR-D; port of
//! `packages/durable/test/harness-sleep.test.ts` @ `4a080252f`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rpi_ai::models::Models;
use rpi_durable::chord::context::{BACKGROUND_CONTEXT, Context};
use rpi_durable::chord::json::JsonValue;
use rpi_durable::harness::{
    AnyTask, Extension, Harness, HarnessImpl, HarnessOptions, NextTaskState, Registry,
    RegistryImpl, RunningTask, SetTimer, TaskDefinition, TaskInspectionState, TaskRuntime,
    TimerOptions, Wake, WakeFn, open_harness,
};
use rpi_durable::ids::TaskId;
use rpi_durable::session::{SessionResult, TaskCreate, TaskOwnership, commit_fn};
use rpi_durable::storage::MemoryStorage;
use rpi_durable::types::{Storage, TaskStatus};

struct FakeTask(&'static str);

#[async_trait]
impl TaskDefinition for FakeTask {
    fn name(&self) -> &str {
        self.0
    }
    fn version(&self) -> u64 {
        1
    }
    fn initial(&self) -> JsonValue {
        serde_json::json!({ "phase": "prepare" })
    }
}

/// Sleeps once, then completes, counting its phases.
struct SleepTask {
    invocations: Arc<AtomicU64>,
    delay: f64,
    name: &'static str,
}

#[async_trait]
impl TaskDefinition for SleepTask {
    fn name(&self) -> &str {
        self.name
    }
    fn version(&self) -> u64 {
        1
    }
    fn initial(&self) -> JsonValue {
        serde_json::json!({ "phase": "sleep" })
    }
    async fn run_phase(
        &self,
        phase: &str,
        _task: RunningTask,
        runtime: &dyn TaskRuntime,
        context: &Context,
    ) -> SessionResult<()> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        match phase {
            "sleep" => {
                let until = runtime.now()? + self.delay;
                runtime
                    .sleep_until(until, serde_json::json!({ "phase": "done" }), context)
                    .await
            }
            _ => {
                runtime
                    .commit(
                        Arc::new(|_tx, _current| {
                            Box::pin(async move {
                                Ok(Some(NextTaskState::Terminal {
                                    outcome: rpi_durable::types::TaskOutcome::Completed {
                                        result: serde_json::json!({ "woke": true }),
                                    },
                                }))
                            })
                        }),
                        context,
                    )
                    .await
            }
        }
    }
}

/// One armed callback and its delay.
type ArmedTimer = (u64, Arc<dyn Fn() + Send + Sync>);

#[derive(Default)]
struct FakeTimer {
    armed: Mutex<Vec<ArmedTimer>>,
    decline: bool,
}

impl FakeTimer {
    fn set_timer(self: &Arc<Self>) -> SetTimer {
        let timer = Arc::clone(self);
        Arc::new(move |callback, delay_ms, _options: TimerOptions| {
            if timer.decline {
                return None;
            }
            timer
                .armed
                .lock()
                .expect("armed")
                .push((delay_ms, Arc::clone(&callback)));
            Some(Arc::new(|| {}))
        })
    }

    fn fire(&self) {
        let armed: Vec<_> = self.armed.lock().expect("armed").drain(..).collect();
        for (_, callback) in armed {
            callback();
        }
    }
}

struct Opened {
    harness: Arc<HarnessImpl>,
}

async fn open(
    task: AnyTask,
    timer: Arc<FakeTimer>,
    on_wake: Option<WakeFn>,
    clock: Arc<AtomicU64>,
) -> Opened {
    let registry = Arc::new(RegistryImpl::with_builtins(
        ["pi.generation", "pi.tool", "pi.compaction"]
            .into_iter()
            .map(|name| Arc::new(FakeTask(name)) as AnyTask)
            .collect(),
    ));
    registry
        .install(Extension {
            name: "test".into(),
            tasks: vec![task],
            ..Extension::default()
        })
        .expect("install");
    let clock_for_now = Arc::clone(&clock);
    let options = HarnessOptions {
        telemetry: None,
        models: Models::new(None).into(),
        identity: None,
        registry: Arc::clone(&registry) as Arc<dyn rpi_durable::harness::RegistryReader>,
        settings: None,
        env: None,
        conversation_created: None,
        now: Some(Arc::new(move || {
            clock_for_now.load(Ordering::SeqCst) as f64
        })),
        set_timer: Some(timer.set_timer()),
        on_wake,
        on_report: Some(Arc::new(|message| eprintln!("report: {message}"))),
        on_report_info: None,
    };
    let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
    let harness = open_harness(storage, options, &BACKGROUND_CONTEXT.clone())
        .await
        .expect("open");
    Opened { harness }
}

async fn create_task(harness: &Arc<HarnessImpl>, kind: &str) -> TaskId {
    harness
        .root(&BACKGROUND_CONTEXT.clone(), Default::default())
        .await
        .expect("root");
    let kind = kind.to_string();
    harness
        .session()
        .commit(
            commit_fn(move |tx| {
                let kind = kind.clone();
                Box::pin(async move {
                    tx.create_task(TaskCreate {
                        kind,
                        version: 1,
                        input: JsonValue::Null,
                        checkpoint: serde_json::json!({ "phase": "sleep" }),
                        ownership: TaskOwnership::Conversation(
                            rpi_durable::ids::ROOT_CONVERSATION_ID,
                        ),
                        conversation_id: Some(rpi_durable::ids::ROOT_CONVERSATION_ID),
                        background: false,
                        abandon_on_restart: false,
                        trace: Default::default(),
                        links: Vec::new(),
                    })
                    .await
                })
            }),
            &BACKGROUND_CONTEXT.clone(),
        )
        .await
        .expect("create task")
}

async fn wait_for_status(harness: &Arc<HarnessImpl>, id: TaskId, status: TaskStatus) {
    for _ in 0..200 {
        let record = harness
            .get_task(id, &BACKGROUND_CONTEXT.clone())
            .await
            .expect("task")
            .expect("present");
        if record.status() == status {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let record = harness
        .get_task(id, &BACKGROUND_CONTEXT.clone())
        .await
        .expect("task")
        .expect("present");
    panic!("task stayed {:?}, wanted {status:?}", record.status());
}

#[tokio::test]
async fn sleeping_task_holds_no_invocation_and_resumes_at_its_checkpoint() {
    let invocations = Arc::new(AtomicU64::new(0));
    let timer = Arc::new(FakeTimer::default());
    let clock = Arc::new(AtomicU64::new(1_000));
    let opened = open(
        Arc::new(SleepTask {
            invocations: Arc::clone(&invocations),
            delay: 1_000.0,
            name: "test.sleep",
        }),
        Arc::clone(&timer),
        None,
        Arc::clone(&clock),
    )
    .await;
    let id = create_task(&opened.harness, "test.sleep").await;
    let _ = opened.harness.resume();

    wait_for_status(&opened.harness, id, TaskStatus::Sleeping).await;
    assert_eq!(invocations.load(Ordering::SeqCst), 1);
    let wake = opened.harness.wake();
    assert!(!wake.active, "a sleeping task needs no process yet");
    assert_eq!(wake.at, Some(2_000.0));
    let inspection = opened
        .harness
        .inspect(&BACKGROUND_CONTEXT.clone())
        .await
        .expect("inspect");
    assert!(matches!(
        inspection
            .tasks
            .iter()
            .find(|task| task.record.id == id)
            .map(|task| &task.state),
        Some(TaskInspectionState::Sleeping { until }) if *until == 2_000.0
    ));
    assert!(!timer.armed.lock().expect("armed").is_empty());

    clock.store(2_000, Ordering::SeqCst);
    timer.fire();
    let settled = opened
        .harness
        .wait_for_task(id, &BACKGROUND_CONTEXT.clone())
        .await
        .expect("settle");
    assert_eq!(settled.result, serde_json::json!({ "woke": true }));
    assert_eq!(invocations.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn on_wake_replaces_the_timer_and_host_resume_wakes_the_task() {
    let invocations = Arc::new(AtomicU64::new(0));
    let wakes: Arc<Mutex<Vec<Wake>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&wakes);
    let on_wake: WakeFn = Arc::new(move |wake| sink.lock().expect("wakes").push(wake));
    let timer = Arc::new(FakeTimer::default());
    let clock = Arc::new(AtomicU64::new(5_000));
    let opened = open(
        Arc::new(SleepTask {
            invocations: Arc::clone(&invocations),
            delay: 1_000.0,
            name: "test.sleep",
        }),
        Arc::clone(&timer),
        Some(on_wake),
        Arc::clone(&clock),
    )
    .await;
    let id = create_task(&opened.harness, "test.sleep").await;
    let _ = opened.harness.resume();

    wait_for_status(&opened.harness, id, TaskStatus::Sleeping).await;
    assert!(
        timer.armed.lock().expect("armed").is_empty(),
        "onWake owns the alarm"
    );
    assert!(
        wakes
            .lock()
            .expect("wakes")
            .iter()
            .any(|wake| wake.at == Some(6_000.0)),
        "got {:?}",
        wakes.lock().expect("wakes")
    );

    clock.store(6_000, Ordering::SeqCst);
    let _ = opened.harness.resume();
    let settled = opened
        .harness
        .wait_for_task(id, &BACKGROUND_CONTEXT.clone())
        .await
        .expect("settle");
    assert_eq!(settled.result, serde_json::json!({ "woke": true }));
}

#[tokio::test]
async fn an_early_timer_wake_arms_again_until_due() {
    let invocations = Arc::new(AtomicU64::new(0));
    let timer = Arc::new(FakeTimer::default());
    let clock = Arc::new(AtomicU64::new(10_000));
    let opened = open(
        Arc::new(SleepTask {
            invocations: Arc::clone(&invocations),
            delay: 1_000.0,
            name: "test.sleep",
        }),
        Arc::clone(&timer),
        None,
        Arc::clone(&clock),
    )
    .await;
    let id = create_task(&opened.harness, "test.sleep").await;
    let _ = opened.harness.resume();
    wait_for_status(&opened.harness, id, TaskStatus::Sleeping).await;

    // Fire without advancing the clock: the pass sees it is not due and arms
    // the timer again instead of running the task.
    timer.fire();
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let record = opened
        .harness
        .get_task(id, &BACKGROUND_CONTEXT.clone())
        .await
        .expect("task")
        .expect("present");
    assert_eq!(record.status(), TaskStatus::Sleeping);
    assert!(!timer.armed.lock().expect("armed").is_empty());
}

/// FR-D R3: a host that declines a foreground timer falls back to the runtime
/// timer, so the sleep still wakes.
#[tokio::test]
async fn a_declined_host_timer_falls_back_and_still_wakes() {
    let invocations = Arc::new(AtomicU64::new(0));
    let timer = Arc::new(FakeTimer {
        armed: Mutex::new(Vec::new()),
        decline: true,
    });
    let clock = Arc::new(AtomicU64::new(1_000));
    let opened = open(
        Arc::new(SleepTask {
            invocations: Arc::clone(&invocations),
            delay: 1_000.0,
            name: "test.sleep",
        }),
        Arc::clone(&timer),
        None,
        Arc::clone(&clock),
    )
    .await;
    let id = create_task(&opened.harness, "test.sleep").await;
    let _ = opened.harness.resume();
    // Real time advances: the sleep is 1s, so shorten it by moving the clock.
    wait_for_status(&opened.harness, id, TaskStatus::Sleeping).await;
    clock.store(2_000, Ordering::SeqCst);
    let settled = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        opened
            .harness
            .wait_for_task(id, &BACKGROUND_CONTEXT.clone()),
    )
    .await
    .expect("fallback timer fires")
    .expect("settle");
    assert_eq!(settled.result, serde_json::json!({ "woke": true }));
}

/// FR-D R4: the wake value reports the earliest of several sleeps.
#[tokio::test]
async fn the_earliest_of_several_sleeps_wakes_the_harness() {
    let timer = Arc::new(FakeTimer::default());
    let clock = Arc::new(AtomicU64::new(10_000));
    let first = Arc::new(AtomicU64::new(0));
    let second = Arc::new(AtomicU64::new(0));
    let registry = Arc::new(rpi_durable::harness::RegistryImpl::with_builtins(
        rpi_durable::harness::builtin_tasks(),
    ));
    registry
        .install(Extension {
            name: "sleeps".into(),
            tasks: vec![
                Arc::new(SleepTask {
                    invocations: Arc::clone(&first),
                    delay: 500.0,
                    name: "test.sleep.a",
                }),
                Arc::new(SleepTask {
                    invocations: Arc::clone(&second),
                    delay: 2_000.0,
                    name: "test.sleep.b",
                }),
            ],
            ..Extension::default()
        })
        .expect("install");
    let options = HarnessOptions {
        telemetry: None,
        models: Models::new(None).into(),
        identity: None,
        registry: Arc::clone(&registry) as Arc<dyn rpi_durable::harness::RegistryReader>,
        settings: None,
        env: None,
        conversation_created: None,
        now: Some({
            let clock = Arc::clone(&clock);
            Arc::new(move || clock.load(Ordering::SeqCst) as f64)
        }),
        set_timer: Some(timer.set_timer()),
        on_wake: None,
        on_report: None,
        on_report_info: None,
    };
    let storage: Arc<dyn rpi_durable::types::Storage> = Arc::new(MemoryStorage::new());
    let harness = open_harness(storage, options, &BACKGROUND_CONTEXT.clone())
        .await
        .expect("open");
    let _ = harness
        .root(&BACKGROUND_CONTEXT.clone(), Default::default())
        .await
        .expect("root");
    let a = create_task(&harness, "test.sleep.a").await;
    let b = create_task(&harness, "test.sleep.b").await;
    harness.resume().expect("resume");
    wait_for_status(&harness, a, TaskStatus::Sleeping).await;
    wait_for_status(&harness, b, TaskStatus::Sleeping).await;
    assert_eq!(harness.wake().at, Some(10_500.0));
    clock.store(10_500, Ordering::SeqCst);
    timer.fire();
    let first = harness
        .wait_for_task(a, &BACKGROUND_CONTEXT.clone())
        .await
        .expect("first");
    assert!(matches!(
        first.record.state,
        rpi_durable::types::TaskState::Terminal { .. }
    ));
    assert_eq!(harness.wake().at, Some(12_000.0));
    clock.store(12_000, Ordering::SeqCst);
    timer.fire();
    let second = harness
        .wait_for_task(b, &BACKGROUND_CONTEXT.clone())
        .await
        .expect("second");
    assert!(matches!(
        second.record.state,
        rpi_durable::types::TaskState::Terminal { .. }
    ));
}

/// R-D6: `wake()` is a pure snapshot, and a clock that moves backwards does not
/// wake a task early; the timer re-arms until the deadline.
#[tokio::test]
async fn repeated_wake_is_stable_and_a_backwards_clock_rearms() {
    let invocations = Arc::new(AtomicU64::new(0));
    let timer = Arc::new(FakeTimer::default());
    let clock = Arc::new(AtomicU64::new(1_000));
    let opened = open(
        Arc::new(SleepTask {
            invocations: Arc::clone(&invocations),
            delay: 1_000.0,
            name: "test.sleep",
        }),
        Arc::clone(&timer),
        None,
        Arc::clone(&clock),
    )
    .await;
    let id = create_task(&opened.harness, "test.sleep").await;
    opened.harness.resume().expect("resume");
    wait_for_status(&opened.harness, id, TaskStatus::Sleeping).await;

    let first = opened.harness.wake();
    let second = opened.harness.wake();
    assert_eq!(first, second);
    assert!(!first.active);
    assert_eq!(first.at, Some(2_000.0));

    // A clock wind-back makes the early wake not due; the task sleeps again.
    clock.store(500, Ordering::SeqCst);
    timer.fire();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let record = opened
        .harness
        .get_task(id, &BACKGROUND_CONTEXT.clone())
        .await
        .expect("task")
        .expect("record");
    assert_eq!(record.status(), TaskStatus::Sleeping);
    assert_eq!(invocations.load(Ordering::SeqCst), 1);

    clock.store(2_000, Ordering::SeqCst);
    timer.fire();
    for _ in 0..200 {
        if invocations.load(Ordering::SeqCst) == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(invocations.load(Ordering::SeqCst), 2);
    opened
        .harness
        .close(&BACKGROUND_CONTEXT.clone())
        .await
        .expect("close");
}
