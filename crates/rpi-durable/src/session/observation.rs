//! Session-to-Chord observation bridge (port of
//! `packages/durable/src/session/observation.ts` @ `b4afdddf`).
//!
//! Upstream attaches one `CommittedStateSource` (for a read-only replicated
//! state) or one `CommittedWatch` (for a serialized async frame watch) to a
//! committed document incarnation. This module carries both.
//!
//! Shape differences (V17-14 §7.3): the bounded pending queue collapses to one
//! root replacement exactly like upstream; async listener delivery is performed
//! by a single spawned drain task per watch instead of microtasks, so frame
//! ordering is preserved through the queue but microtask turn boundaries are not
//! observable; `queueMicrotask` maps to `tokio::sync::Notify`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::chord::context::{Context, without_abort_signal};
use crate::chord::delta::{Op, PendingOps, PendingTake};
use crate::chord::json::{JsonObject, JsonValue};
use crate::chord::state::{
    AttachedReplicatedState, FrameListener, ReplicatedStateSource, ReplicatedStateSourceAttachment,
    ReplicatedStateSourceFrame, ReplicatedStateSourceOptions, ReplicatedStateSourceSnapshot,
    attach_replicated_state_source,
};

/// Canonical terminal update for a retired document incarnation (upstream
/// `RETIREMENT_OPERATIONS`).
pub const RETIREMENT_OPERATIONS: &[Op] = &[Op::Replace(JsonValue::Null)];

/// Observed document value: the committed root object, or `null` once retired.
pub type ObservedDocumentValue = Option<JsonObject>;

/// One value or recoverable-failure call delivered to a watch listener.
#[derive(Debug, Clone, PartialEq)]
pub enum WatchCall<T = ObservedDocumentValue> {
    Value {
        value: T,
        ops: Vec<Op>,
        retired: bool,
    },
    Failed {
        error: String,
    },
}

/// A boxed watch-listener future; `Err` settles the watch with `listener_error`.
pub type WatchFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'static>>;

/// One serialized asynchronous watch listener (upstream `WatchHandle.start`).
pub type WatchListener = Arc<dyn Fn(WatchCall, Context) -> WatchFuture + Send + Sync>;

/// Reason a watch terminated (upstream `WatchEnd`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEnd {
    Stopped,
    Aborted,
    Ended,
    ListenerError { error: String },
    Failed { error: String },
}

fn to_observed(value: &JsonValue) -> ObservedDocumentValue {
    match value {
        JsonValue::Null => None,
        JsonValue::Object(object) => Some(object.clone()),
        // A document root is always an object; any other shape is treated as absent.
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// CommittedWatch
// ---------------------------------------------------------------------------

struct WatchFrame {
    value: JsonValue,
    ops: Vec<Op>,
    retired: bool,
    failed: Option<String>,
    context: Context,
}

#[derive(Default)]
struct WatchState {
    value: JsonValue,
    pending: PendingOps,
    lone: Option<WatchFrame>,
    pending_retired: bool,
    pending_context: Option<Context>,
    pending_failure: Option<String>,
    replace_after_failure: bool,
    listener: Option<WatchListener>,
    started: bool,
    draining: bool,
    end: Option<WatchEnd>,
    latest: JsonValue,
    observer: Option<Arc<dyn crate::session::Observer<ObservedDocumentValue>>>,
    observer_reporter: Option<crate::session::session::FailureReporter>,
    detach: Option<Box<dyn Fn() + Send + Sync>>,
}

/// Serialized exact-frame watch bound to one document incarnation (upstream
/// `CommittedWatch`).
#[derive(Clone)]
pub struct CommittedWatch {
    inner: Arc<Mutex<WatchState>>,
    notify: Arc<tokio::sync::Notify>,
}

impl CommittedWatch {
    /// Create a watch holding the acquisition revision value.
    pub fn new(value: JsonValue) -> Self {
        Self {
            inner: Arc::new(Mutex::new(WatchState {
                value: value.clone(),
                latest: value,
                ..WatchState::default()
            })),
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Install the unsubscribe callback invoked when the watch terminates.
    pub(crate) fn set_detach(&self, detach: Box<dyn Fn() + Send + Sync>) {
        if let Ok(mut state) = self.inner.lock() {
            state.detach = Some(detach);
        }
    }

    /// Current observed value; `None` after retirement.
    pub fn value(&self) -> ObservedDocumentValue {
        let value = self
            .inner
            .lock()
            .map(|state| state.value.clone())
            .unwrap_or(JsonValue::Null);
        to_observed(&value)
    }

    /// Switch this address subscription to synchronous observation.
    pub fn observe(
        self,
        observer: Arc<dyn crate::session::Observer<ObservedDocumentValue>>,
        reporter: crate::session::session::FailureReporter,
    ) -> crate::session::Observation<ObservedDocumentValue> {
        let value = self.value();
        if let Ok(mut state) = self.inner.lock() {
            state.observer = Some(observer);
            state.observer_reporter = Some(reporter);
        }
        let detach = crate::session::Subscription::new({
            let watch = self.clone();
            move || watch.detach_observer()
        });
        crate::session::Observation { value, detach }
    }

    fn detach_observer(&self) {
        if let Ok(mut state) = self.inner.lock() {
            state.observer = None;
        }
        self.terminate(WatchEnd::Stopped);
    }

    /// Whether the watch has terminated.
    pub fn is_closed(&self) -> bool {
        self.inner
            .lock()
            .map(|state| state.end.is_some())
            .unwrap_or(true)
    }

    /// Start serialized delivery. Fails when already started or already stopped
    /// (upstream throws).
    pub fn start(&self, listener: WatchListener) -> Result<(), crate::session::SessionError> {
        let has_pending = {
            let mut state = self
                .inner
                .lock()
                .map_err(|_| crate::session::SessionError::message("watch lock poisoned"))?;
            if state.started {
                return Err(crate::session::SessionError::message(
                    "Watch is already started",
                ));
            }
            if state.end.is_some() {
                return Err(crate::session::SessionError::message("Watch is stopped"));
            }
            state.started = true;
            state.listener = Some(listener);
            state.pending_context.is_some()
        };
        if has_pending {
            self.spawn_drain();
        }
        Ok(())
    }

    /// Stop delivery and await the terminal reason. Idempotent.
    pub async fn stop(&self) -> WatchEnd {
        self.terminate(WatchEnd::Stopped);
        self.closed().await
    }

    /// Cancel through the caller's abort signal.
    pub fn cancel(&self) {
        self.terminate(WatchEnd::Aborted);
    }

    /// Terminate because the session closed, or failed with `failure`.
    pub fn close_session(&self, failure: Option<String>) {
        let observing = self
            .inner
            .lock()
            .map(|state| state.observer.is_some())
            .unwrap_or(false);
        match failure {
            Some(error) => self.terminate(WatchEnd::Failed { error }),
            None if observing => self.terminate(WatchEnd::Failed {
                error: "Session is closed".to_string(),
            }),
            None => self.terminate(WatchEnd::Ended),
        }
    }

    /// Await the terminal reason.
    pub async fn closed(&self) -> WatchEnd {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            // Register before checking: `notify_waiters` stores no permit.
            notified.as_mut().enable();
            {
                if let Ok(state) = self.inner.lock()
                    && let Some(end) = state.end.clone()
                {
                    return end;
                }
            }
            notified.await;
        }
    }

    /// Deliver one committed frame; buffers or collapses on overflow.
    pub(crate) fn advance(&self, value: JsonValue, ops: Vec<Op>, retired: bool, context: Context) {
        let notify_started = {
            let Ok(mut state) = self.inner.lock() else {
                return;
            };
            if state.end.is_some() {
                return;
            }
            if !retired {
                state.latest = value.clone();
            }
            let frame_value = state.latest.clone();
            let retired_ops = if retired { Vec::new() } else { ops };
            if let Some(observer) = state.observer.clone() {
                let reporter = state.observer_reporter.clone();
                drop(state);
                let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    observer.advance(
                        WatchCall::Value {
                            value: to_observed(&frame_value),
                            ops: retired_ops,
                            retired,
                        },
                        &context,
                    );
                }));
                if failed.is_err() {
                    if let Some(reporter) = reporter {
                        reporter(
                            "listener_error: document observer panicked".to_string(),
                            Some(context),
                        );
                    }
                    self.terminate(WatchEnd::ListenerError {
                        error: "document observer panicked".to_string(),
                    });
                }
                return;
            }
            if state.replace_after_failure {
                state.pending_failure = None;
            } else {
                state.lone = if state.pending_context.is_none() {
                    Some(WatchFrame {
                        value: frame_value.clone(),
                        ops: retired_ops.clone(),
                        retired,
                        failed: None,
                        context: context.clone(),
                    })
                } else {
                    None
                };
                state.pending.push(&retired_ops);
            }
            state.pending_retired |= retired;
            state.pending_context = Some(context);
            state.started
        };
        if notify_started {
            self.spawn_drain();
        }
    }
    /// Deliver a recoverable source failure and replace the next value's root.
    pub fn fail(&self, error: String, context: Context) {
        let notify_started = {
            let Ok(mut state) = self.inner.lock() else {
                return;
            };
            if state.end.is_some() {
                return;
            }
            if let Some(observer) = state.observer.clone() {
                let reporter = state.observer_reporter.clone();
                drop(state);
                let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    observer.advance(WatchCall::Failed { error }, &context);
                }));
                if failed.is_err() {
                    if let Some(reporter) = reporter {
                        reporter(
                            "listener_error: document observer panicked".to_string(),
                            Some(context),
                        );
                    }
                    self.terminate(WatchEnd::ListenerError {
                        error: "document observer panicked".to_string(),
                    });
                }
                return;
            }
            state.pending = PendingOps::new();
            state.lone = None;
            state.pending_retired = false;
            state.pending_failure = Some(error);
            state.replace_after_failure = true;
            state.pending_context = Some(context);
            state.started
        };
        if notify_started {
            self.spawn_drain();
        }
    }

    fn spawn_drain(&self) {
        let should_spawn = {
            let Ok(mut state) = self.inner.lock() else {
                return;
            };
            if state.end.is_some() {
                return;
            }
            if state.draining {
                false
            } else {
                state.draining = true;
                true
            }
        };
        self.notify.notify_one();
        if should_spawn {
            let inner = Arc::clone(&self.inner);
            let notify = Arc::clone(&self.notify);
            tokio::spawn(async move {
                drain(inner, notify).await;
            });
        }
    }

    fn terminate(&self, end: WatchEnd) {
        let (detach, observer, reporter) = {
            let Ok(mut state) = self.inner.lock() else {
                return;
            };
            if state.end.is_some() {
                return;
            }
            state.end = Some(end.clone());
            state.pending = PendingOps::new();
            state.lone = None;
            state.pending_context = None;
            state.pending_failure = None;
            state.replace_after_failure = false;
            (
                state.detach.take(),
                state.observer.take(),
                state.observer_reporter.clone(),
            )
        };
        if let Some(observer) = observer {
            let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                observer.end(end);
            }));
            if failed.is_err()
                && let Some(reporter) = reporter
            {
                reporter("listener_error: observer end panicked".to_string(), None);
            }
        }
        if let Some(detach) = detach {
            detach();
        }
        self.notify.notify_waiters();
    }
}

async fn drain(inner: Arc<Mutex<WatchState>>, notify: Arc<tokio::sync::Notify>) {
    loop {
        let notified = notify.notified();
        tokio::pin!(notified);
        // Register before draining and checking: the drain loop has await
        // points, and `notify_waiters` stores no permit.
        notified.as_mut().enable();
        // Drain every buffered frame.
        loop {
            let frame = {
                let Ok(mut state) = inner.lock() else {
                    return;
                };
                if state.end.is_some() {
                    state.draining = false;
                    return;
                }
                let Some(context) = state.pending_context.take() else {
                    break;
                };
                let lone = state.lone.take();
                let failed = state.pending_failure.take();
                let replace_after_failure = state.replace_after_failure;
                let retired = std::mem::take(&mut state.pending_retired);
                let ops = match state.pending.take() {
                    PendingTake::Ops(ops) => ops,
                    PendingTake::Replace => vec![Op::Replace(state.latest.clone())],
                };
                let value = state.latest.clone();
                if replace_after_failure && failed.is_none() {
                    state.replace_after_failure = false;
                }
                Some(match lone {
                    Some(mut frame) => {
                        frame.context = context;
                        frame
                    }
                    None => WatchFrame {
                        value: value.clone(),
                        ops: if replace_after_failure && failed.is_none() {
                            vec![Op::Replace(value)]
                        } else {
                            ops
                        },
                        retired,
                        failed,
                        context,
                    },
                })
            };
            let Some(frame) = frame else {
                break;
            };
            let listener = {
                let Ok(mut state) = inner.lock() else {
                    return;
                };
                if frame.failed.is_none() {
                    state.value = frame.value.clone();
                }
                state.listener.clone()
            };
            let Some(listener) = listener else {
                continue;
            };
            let call = match frame.failed {
                Some(error) => WatchCall::Failed { error },
                None => WatchCall::Value {
                    value: to_observed(&frame.value),
                    ops: frame.ops.clone(),
                    retired: frame.retired,
                },
            };
            let result = listener(call, without_abort_signal(&frame.context)).await;
            let mut state = match inner.lock() {
                Ok(state) => state,
                Err(_) => return,
            };
            if state.end.is_some() {
                state.draining = false;
                drop(state);
                return;
            }
            if let Err(message) = result {
                state.end = Some(WatchEnd::ListenerError { error: message });
                state.pending = PendingOps::new();
                state.pending_context = None;
            } else {
                drop(state);
                continue;
            }
            let detach = state.detach.take();
            state.draining = false;
            drop(state);
            if let Some(detach) = detach {
                detach();
            }
            notify.notify_waiters();
            return;
        }
        // Wait for more frames or termination.
        {
            let Ok(state) = inner.lock() else {
                return;
            };
            if state.end.is_some() {
                continue;
            }
        }
        notified.await;
    }
}

// ---------------------------------------------------------------------------
// CommittedStateSource
// ---------------------------------------------------------------------------

struct AttachmentSlot {
    id: u64,
    listener: Option<FrameListener>,
}

struct SourceInner {
    attachments: Mutex<Vec<AttachmentSlot>>,
    value: Mutex<JsonValue>,
    cursor: AtomicU64,
    retired: AtomicBool,
    closed: AtomicBool,
    release: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    next_id: AtomicU64,
}

/// Session-to-Chord bridge owned one-to-one by one attached document state
/// (upstream `CommittedStateSource`). A `null` value retires it.
#[derive(Clone)]
pub struct CommittedStateSource {
    inner: Arc<SourceInner>,
}

impl CommittedStateSource {
    /// Create a source over one committed value.
    pub fn new(value: JsonValue) -> Self {
        Self {
            inner: Arc::new(SourceInner {
                attachments: Mutex::new(Vec::new()),
                value: Mutex::new(value),
                cursor: AtomicU64::new(0),
                retired: AtomicBool::new(false),
                closed: AtomicBool::new(false),
                release: Mutex::new(None),
                next_id: AtomicU64::new(0),
            }),
        }
    }

    /// Install the callback invoked when every attachment is disposed.
    pub(crate) fn set_release(&self, release: Box<dyn Fn() + Send + Sync>) {
        if let Ok(mut slot) = self.inner.release.lock() {
            *slot = Some(release);
        }
    }

    /// Forward one committed frame to every attachment (upstream `advance`).
    pub(crate) fn advance(&self, value: JsonValue, ops: Vec<Op>, context: Context) {
        if self.inner.closed.load(Ordering::SeqCst) || self.inner.retired.load(Ordering::SeqCst) {
            return;
        }
        if let Ok(mut current) = self.inner.value.lock() {
            *current = value.clone();
        }
        let cursor = self.inner.cursor.fetch_add(1, Ordering::SeqCst) + 1;
        if value.is_null() {
            self.inner.retired.store(true, Ordering::SeqCst);
        }
        let frame = ReplicatedStateSourceFrame {
            cursor,
            value,
            ops,
            context,
        };
        if let Ok(mut attachments) = self.inner.attachments.lock() {
            for slot in attachments.iter_mut() {
                if let Some(listener) = slot.listener.as_mut() {
                    listener(frame.clone());
                }
            }
        }
    }

    /// Release every attachment because the session closed (upstream
    /// `closeSession`).
    pub(crate) fn close_session(&self, _failure: Option<String>) {
        if self.inner.closed.load(Ordering::SeqCst) {
            return;
        }
        if let Ok(mut attachments) = self.inner.attachments.lock() {
            attachments.clear();
        }
        self.finish_disposal();
    }

    fn remove_attachment(&self, id: u64) {
        let empty = match self.inner.attachments.lock() {
            Ok(mut attachments) => {
                attachments.retain(|slot| slot.id != id);
                attachments.is_empty()
            }
            Err(_) => false,
        };
        if empty {
            self.finish_disposal();
        }
    }

    fn finish_disposal(&self) {
        if self.inner.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Ok(mut value) = self.inner.value.lock() {
            *value = JsonValue::Null;
        }
        let release = self
            .inner
            .release
            .lock()
            .ok()
            .and_then(|mut slot| slot.take());
        if let Some(release) = release {
            release();
        }
    }
}

impl ReplicatedStateSource for CommittedStateSource {
    fn attach(&self) -> ReplicatedStateSourceAttachment {
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let snapshot = ReplicatedStateSourceSnapshot {
            value: self
                .inner
                .value
                .lock()
                .map(|value| value.clone())
                .unwrap_or(JsonValue::Null),
            cursor: self.inner.cursor.load(Ordering::SeqCst),
        };
        if let Ok(mut attachments) = self.inner.attachments.lock() {
            attachments.push(AttachmentSlot { id, listener: None });
        }
        let this = self.clone();
        let activate = Box::new(move |listener: FrameListener| {
            if let Ok(mut attachments) = this.inner.attachments.lock()
                && let Some(slot) = attachments.iter_mut().find(|slot| slot.id == id)
            {
                slot.listener = Some(listener);
            }
        });
        let this = self.clone();
        let dispose = Box::new(move || {
            this.remove_attachment(id);
        });
        ReplicatedStateSourceAttachment {
            snapshot,
            activate,
            dispose,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers used by the session
// ---------------------------------------------------------------------------

/// Attach a read-only replicated state to one committed document value
/// (upstream `replicatedState(source)`).
pub(crate) fn attach_state(source: &CommittedStateSource) -> AttachedReplicatedState {
    attach_replicated_state_source(source, ReplicatedStateSourceOptions::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    fn listener(log: Arc<StdMutex<Vec<ObservedDocumentValue>>>) -> WatchListener {
        Arc::new(move |call, _context| {
            let log = Arc::clone(&log);
            Box::pin(async move {
                if let WatchCall::Value { value, .. } = call {
                    log.lock().expect("log").push(value);
                }
                Ok(())
            })
        })
    }

    #[tokio::test]
    async fn overflow_collapses_to_one_root_replacement() {
        let watch = CommittedWatch::new(serde_json::json!({ "value": 0 }));
        for value in 1..=crate::chord::delta::MAX_PENDING_OPS as u64 + 1 {
            watch.advance(
                serde_json::json!({ "value": value }),
                vec![Op::Set {
                    path: vec![crate::chord::delta::PathSegment::Key("value".into())],
                    value: serde_json::json!(value),
                }],
                false,
                crate::chord::context::BACKGROUND_CONTEXT.clone(),
            );
        }
        let log = Arc::new(StdMutex::new(Vec::new()));
        watch.start(listener(Arc::clone(&log))).expect("start");
        // Allow the drain task to run.
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let delivered = log.lock().expect("log").clone();
        assert_eq!(delivered.len(), 1);
        assert_eq!(
            delivered[0],
            Some(
                serde_json::json!({ "value": crate::chord::delta::MAX_PENDING_OPS + 1 })
                    .as_object()
                    .expect("object")
                    .clone()
            )
        );
        let _ = watch.stop().await;
    }

    #[tokio::test]
    async fn retirement_does_not_end_the_watch_before_recreation() {
        let watch = CommittedWatch::new(serde_json::json!({ "value": 0 }));
        watch.advance(
            JsonValue::Null,
            Vec::new(),
            true,
            crate::chord::context::BACKGROUND_CONTEXT.clone(),
        );
        watch.advance(
            serde_json::json!({ "value": 1 }),
            vec![Op::Replace(serde_json::json!({ "value": 1 }))],
            false,
            crate::chord::context::BACKGROUND_CONTEXT.clone(),
        );
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let log = Arc::clone(&calls);
        watch
            .start(Arc::new(move |call, _context| {
                let log = Arc::clone(&log);
                Box::pin(async move {
                    log.lock().expect("log").push(call);
                    Ok(())
                })
            }))
            .expect("start");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let calls = calls.lock().expect("log").clone();
        assert_eq!(calls.len(), 1);
        assert!(matches!(
            &calls[0],
            WatchCall::Value {
                retired: true,
                value: Some(value),
                ops,
                ..
            } if value.get("value") == Some(&serde_json::json!(1))
                && matches!(ops.as_slice(), [Op::Replace(_)])
        ));
        assert!(!watch.is_closed());
        watch.close_session(None);
        assert_eq!(watch.closed().await, WatchEnd::Ended);
    }
}
