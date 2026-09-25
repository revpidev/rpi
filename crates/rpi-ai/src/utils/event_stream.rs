//! Port of `packages/ai/src/utils/event-stream.ts` @ pi 0.82.1 (2efa728).
//!
//! `AssistantMessageEventStream`: producer pushes events synchronously while
//! the consumer iterates asynchronously; `done`/`error` events resolve the
//! final result. Backed by an unbounded mpsc channel (the upstream queue is
//! unbounded; consumers see the same ordering and buffering semantics).
//!
//! #9055 (`b2602be77`, "optimize EventStream queue"): upstream replaced the
//! `Array.shift()` queue (O(n) per dequeue, O(n²) when draining buffered
//! events) with a two-stack FifoQueue. This port is exempt by construction:
//! `mpsc::UnboundedReceiver::poll_recv` is O(1), so the drain path was never
//! quadratic. The upstream regression suite is ported below (drain order,
//! post-terminal push, interleaved arrival, waiting-consumer order, drain
//! after end) plus the linearity assertion the task requires.
//!
//! The stream is `Clone`: clones share the same underlying queue, mirroring
//! the single upstream object passed between producers and consumers.

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};

use futures::future::{BoxFuture, Shared};
use futures::prelude::*;
use futures::Stream;
use tokio::sync::{mpsc, oneshot};

use crate::types::{AssistantMessage, StreamEvent};

struct Inner {
    tx: Mutex<Option<mpsc::UnboundedSender<StreamEvent>>>,
    rx: Mutex<mpsc::UnboundedReceiver<StreamEvent>>,
    result_tx: Mutex<Option<oneshot::Sender<AssistantMessage>>>,
    result_rx: Shared<oneshot::Receiver<AssistantMessage>>,
    done: AtomicBool,
}

/// Producer/consumer event stream for assistant message events.
///
/// - [`push`](Self::push) is ignored after a `done`/`error` event or
///   [`end`](Self::end) (upstream `done` flag).
/// - [`end`](Self::end) closes the channel; buffered events drain first.
/// - [`result`](Self::result) resolves with the message from the terminal
///   `done`/`error` event, or the value passed to `end`.
#[derive(Clone)]
pub struct AssistantMessageEventStream {
    inner: Arc<Inner>,
}

impl Default for AssistantMessageEventStream {
    fn default() -> Self {
        Self::new()
    }
}

impl AssistantMessageEventStream {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let (result_tx, result_rx) = oneshot::channel();
        Self {
            inner: Arc::new(Inner {
                tx: Mutex::new(Some(tx)),
                rx: Mutex::new(rx),
                result_tx: Mutex::new(Some(result_tx)),
                result_rx: result_rx.shared(),
                done: AtomicBool::new(false),
            }),
        }
    }

    fn resolve_result(&self, message: AssistantMessage) {
        // First resolution wins (upstream promise resolve is idempotent).
        if let Some(tx) = self
            .inner
            .result_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = tx.send(message);
        }
    }

    fn is_complete(event: &StreamEvent) -> bool {
        matches!(event, StreamEvent::Done { .. } | StreamEvent::Error { .. })
    }

    fn extract_result(event: &StreamEvent) -> Option<AssistantMessage> {
        match event {
            StreamEvent::Done { message, .. } => Some(message.clone()),
            StreamEvent::Error { error, .. } => Some(error.clone()),
            _ => None,
        }
    }

    /// Push an event to the stream.
    pub fn push(&self, event: StreamEvent) {
        if self.inner.done.load(Ordering::SeqCst) {
            return;
        }
        if Self::is_complete(&event) {
            self.inner.done.store(true, Ordering::SeqCst);
            if let Some(result) = Self::extract_result(&event) {
                self.resolve_result(result);
            }
        }
        // A closed channel (consumer dropped / ended) is not an error upstream.
        if let Some(tx) = self
            .inner
            .tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            let _ = tx.send(event);
        }
    }

    /// End the stream. A provided result resolves pending `result()` calls
    /// (unless already resolved by a terminal event).
    pub fn end(&self, result: Option<AssistantMessage>) {
        self.inner.done.store(true, Ordering::SeqCst);
        if let Some(result) = result {
            self.resolve_result(result);
        }
        // Dropping the sender closes the channel: iterators terminate after
        // draining buffered events, and pending `poll_recv` wakers fire.
        self.inner
            .tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }

    /// The final assistant message (resolves on the terminal event).
    pub fn result(&self) -> BoxFuture<'static, Option<AssistantMessage>> {
        self.inner.result_rx.clone().map(Result::ok).boxed()
    }
}

impl Stream for AssistantMessageEventStream {
    type Item = StreamEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        self.inner
            .rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .poll_recv(cx)
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;

    use super::*;
    use crate::types::{ApiKind, AssistantRole, DoneReason, ErrorReason, StopReason, Usage};

    fn message(stop_reason: StopReason) -> AssistantMessage {
        AssistantMessage {
            role: AssistantRole::Assistant,
            content: vec![],
            api: ApiKind::from("anthropic-messages"),
            provider: "p".to_owned(),
            model: "m".to_owned(),
            response_model: None,
            response_id: None,
            provider_thinking_level: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason,
            error_message: None,
            timestamp: 0,
            deferred: None,
            end_turn: None,
            raw_stop_reason: None,
        }
    }

    #[tokio::test]
    async fn test_push_iterate_result() {
        let stream = AssistantMessageEventStream::new();
        stream.push(StreamEvent::Start {
            partial: Arc::new(message(StopReason::Pending)),
        });
        stream.push(StreamEvent::Done {
            reason: DoneReason::Stop,
            message: message(StopReason::Stop),
        });
        stream.end(None);

        let events: Vec<StreamEvent> = stream.collect().await;
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn test_result_resolves_on_terminal_event() {
        let stream = AssistantMessageEventStream::new();
        let result_future = stream.result();
        stream.push(StreamEvent::Error {
            reason: ErrorReason::Error,
            error: message(StopReason::Error),
        });
        let result = result_future.await.expect("resolved");
        assert_eq!(result.stop_reason, StopReason::Error);
    }

    #[tokio::test]
    async fn test_push_after_done_is_dropped() {
        let stream = AssistantMessageEventStream::new();
        stream.push(StreamEvent::Done {
            reason: DoneReason::Stop,
            message: message(StopReason::Stop),
        });
        stream.push(StreamEvent::Start {
            partial: Arc::new(message(StopReason::Pending)),
        });
        stream.end(None);
        let events: Vec<StreamEvent> = stream.collect().await;
        assert_eq!(events.len(), 1);
    }

    #[tokio::test]
    async fn test_end_result_fallback() {
        let stream = AssistantMessageEventStream::new();
        stream.push(StreamEvent::Start {
            partial: Arc::new(message(StopReason::Pending)),
        });
        stream.end(Some(message(StopReason::Aborted)));
        let result = stream.result().await.expect("resolved");
        assert_eq!(result.stop_reason, StopReason::Aborted);
        let events: Vec<StreamEvent> = stream.collect().await;
        assert_eq!(events.len(), 1);
    }

    #[tokio::test]
    async fn test_clone_shares_queue() {
        let stream = AssistantMessageEventStream::new();
        let producer = stream.clone();
        producer.push(StreamEvent::Start {
            partial: Arc::new(message(StopReason::Pending)),
        });
        producer.end(None);
        let events: Vec<StreamEvent> = stream.collect().await;
        assert_eq!(events.len(), 1);
    }

    // -----------------------------------------------------------------------
    // #9055 regression suite (upstream event-stream.test.ts @ b2602be77).
    // The rpi queue is channel-backed (O(1) dequeue), so these pin the
    // ordering semantics the upstream FifoQueue swap had to preserve.
    // -----------------------------------------------------------------------

    fn kind(event: &StreamEvent) -> &'static str {
        match event {
            StreamEvent::Start { .. } => "start",
            StreamEvent::TextStart { .. } => "text_start",
            StreamEvent::TextDelta { .. } => "text_delta",
            StreamEvent::TextEnd { .. } => "text_end",
            StreamEvent::ThinkingStart { .. } => "thinking_start",
            StreamEvent::Done { .. } => "done",
            StreamEvent::Error { .. } => "error",
            _ => "other",
        }
    }

    fn text_event(kind_index: usize, partial: &AssistantMessage) -> StreamEvent {
        match kind_index {
            0 => StreamEvent::Start {
                partial: Arc::new(partial.clone()),
            },
            1 => StreamEvent::TextStart {
                content_index: 0,
                partial: Arc::new(partial.clone()),
            },
            _ => StreamEvent::TextDelta {
                content_index: 0,
                delta: "x".to_owned(),
                partial: Arc::new(partial.clone()),
            },
        }
    }

    /// "drains buffered events in order and ignores events pushed after
    /// completion" + "drains buffered events after end and resolves the
    /// explicit result" (both upstream cases share the buffered-drain shape;
    /// `end(Some(..))` is the explicit-result arm).
    #[tokio::test]
    async fn test_9055_drains_buffered_events_in_order() {
        let partial = message(StopReason::Pending);
        let stream = AssistantMessageEventStream::new();
        stream.push(text_event(0, &partial));
        stream.push(text_event(1, &partial));
        stream.push(StreamEvent::Done {
            reason: DoneReason::Stop,
            message: message(StopReason::Stop),
        });
        // Terminal event flips `done`: later pushes are dropped.
        stream.push(text_event(2, &partial));
        stream.end(Some(message(StopReason::Stop)));

        let result = stream.result().await.expect("resolved");
        assert_eq!(result.stop_reason, StopReason::Stop);

        let kinds: Vec<&str> = stream.clone().map(|event| kind(&event)).collect().await;
        assert_eq!(kinds, vec!["start", "text_start", "done"]);
    }

    /// "preserves order when events arrive after buffered draining starts".
    #[tokio::test]
    async fn test_9055_order_preserved_when_events_arrive_mid_drain() {
        let partial = message(StopReason::Pending);
        let stream = AssistantMessageEventStream::new();
        stream.push(text_event(0, &partial));
        stream.push(text_event(1, &partial));

        let mut iter = stream.clone();
        let first = iter.next().await;
        assert_eq!(kind(&first.expect("event")), "start");

        // Arrives after draining started: must land behind the buffered event.
        stream.push(text_event(2, &partial));
        assert_eq!(kind(&iter.next().await.expect("event")), "text_start");
        assert_eq!(kind(&iter.next().await.expect("event")), "text_delta");
        stream.end(None);
        assert!(iter.next().await.is_none());
    }

    /// "delivers events to waiting consumers in registration order".
    /// Both consumers' `next()` futures are polled once (registering their
    /// waiters on the shared receiver) before any event is pushed; the
    /// channel dequeues FIFO, and the first-registered consumer is the first
    /// to resume.
    #[tokio::test]
    async fn test_9055_waiting_consumers_served_in_registration_order() {
        let stream = AssistantMessageEventStream::new();
        let partial = message(StopReason::Pending);

        let mut first = stream.clone();
        let mut second = stream.clone();
        let producer = stream.clone();

        let mut f1 = Box::pin(first.next());
        let mut f2 = Box::pin(second.next());
        // Deterministic registration: both waiters park before the pushes.
        assert!(futures::poll!(&mut f1).is_pending());
        assert!(futures::poll!(&mut f2).is_pending());

        producer.push(text_event(0, &partial));
        producer.push(text_event(1, &partial));

        let e1 = f1.await.expect("event for first");
        let e2 = f2.await.expect("event for second");
        assert_eq!(kind(&e1), "start");
        assert_eq!(kind(&e2), "text_start");
    }

    /// "wakes all waiting consumers when ended without a result".
    #[tokio::test]
    async fn test_9055_end_wakes_all_waiting_consumers() {
        let stream = AssistantMessageEventStream::new();

        let mut first = stream.clone();
        let mut second = stream.clone();
        let producer = stream.clone();

        let mut f1 = Box::pin(first.next());
        let mut f2 = Box::pin(second.next());
        assert!(futures::poll!(&mut f1).is_pending());
        assert!(futures::poll!(&mut f2).is_pending());

        producer.end(None);
        assert!(f1.await.is_none());
        assert!(f2.await.is_none());
    }

    /// Linearity assertion (task §4 FR-A): draining 10⁴ and 10⁵ buffered
    /// events must stay in the linear regime. A `shift`-style O(n) dequeue
    /// would make the 10⁵ round ~100× the 10⁴ round; O(1) dequeues keep the
    /// ratio near 10×. Both rounds run in milliseconds, so the generous
    /// bounds below only fail on quadratic blowups (50× ratio / 5s absolute).
    #[tokio::test]
    async fn test_9055_buffered_drain_is_linear() {
        async fn drain_round(n: u32) -> (u32, std::time::Duration) {
            let start = std::time::Instant::now();
            let stream = AssistantMessageEventStream::new();
            let partial = message(StopReason::Pending);
            for _ in 0..n {
                stream.push(StreamEvent::Start {
                    partial: Arc::new(partial.clone()),
                });
            }
            stream.end(None);
            let mut iter = stream.clone();
            let mut drained = 0u32;
            while iter.next().await.is_some() {
                drained += 1;
            }
            (drained, start.elapsed())
        }

        let (small_count, small) = drain_round(10_000).await;
        let (large_count, large) = drain_round(100_000).await;
        assert_eq!(small_count, 10_000);
        assert_eq!(large_count, 100_000);
        assert!(
            large.as_secs() < 5,
            "100k drain took {large:?} (quadratic?)"
        );
        let ratio = large.as_secs_f64() / small.as_secs_f64().max(1e-9);
        assert!(
            ratio < 50.0,
            "drain time ratio 100k/10k = {ratio:.1}x (linear ≈ 10x, quadratic ≈ 100x)"
        );
    }
}
