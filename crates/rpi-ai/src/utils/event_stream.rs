//! Port of `packages/ai/src/utils/event-stream.ts` @ pi 0.82.1 (2efa728).
//!
//! `AssistantMessageEventStream`: producer pushes events synchronously while
//! the consumer iterates asynchronously; `done`/`error` events resolve the
//! final result. Backed by an unbounded mpsc channel (the upstream queue is
//! unbounded; consumers see the same ordering and buffering semantics).
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
            diagnostics: None,
            usage: Usage::default(),
            stop_reason,
            error_message: None,
            timestamp: 0,
        }
    }

    #[tokio::test]
    async fn test_push_iterate_result() {
        let stream = AssistantMessageEventStream::new();
        stream.push(StreamEvent::Start {
            partial: message(StopReason::Pending),
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
            partial: message(StopReason::Pending),
        });
        stream.end(None);
        let events: Vec<StreamEvent> = stream.collect().await;
        assert_eq!(events.len(), 1);
    }

    #[tokio::test]
    async fn test_end_result_fallback() {
        let stream = AssistantMessageEventStream::new();
        stream.push(StreamEvent::Start {
            partial: message(StopReason::Pending),
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
            partial: message(StopReason::Pending),
        });
        producer.end(None);
        let events: Vec<StreamEvent> = stream.collect().await;
        assert_eq!(events.len(), 1);
    }
}
