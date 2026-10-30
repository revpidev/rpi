//! Content-free telemetry context (port of the telemetry surface used by
//! `packages/durable/src/harness/telemetry.ts` @ `4a080252f`).
//!
//! A host supplies a [`TelemetrySink`]; phases and tools set attributes and
//! add events carrying durable record IDs, never prompts, tokens, or
//! credentials. Without a sink every call is a no-op.

use std::sync::{Arc, Mutex};

use crate::chord::json::{JsonObject, JsonValue};

/// Durable's content-free span, event and metric vocabulary. IDs address
/// durable records, never content.
pub fn durable_telemetry_schema() -> JsonValue {
    serde_json::json!({
        "version": 1,
        "spans": {
            "pi.durable.submit": {
                "startAttributes": ["submission.type", "conversation.id"],
                "endAttributes": ["submission.id", "result"],
                "events": ["pi.durable.entry", "pi.durable.submission", "pi.durable.report"],
                "status": { "default": "ok", "errorWhen": "Admission failed." }
            },
            "pi.durable.phase": {
                "startAttributes": ["task.id", "task.name", "phase", "kind", "attempt", "resumed"],
                "endAttributes": ["outcome", "tool.name", "tool.call_id"],
                "events": {
                    "pi.durable.entry": ["entry.id", "entry.kind"],
                    "pi.durable.submission": ["submission.id", "status", "answer.entry_id"],
                    "pi.durable.report": ["type", "reason", "extension", "task.id"],
                    "pi.durable.checkpoint": ["phase"],
                    "pi.durable.replay": ["tool.name"]
                },
                "status": {
                    "default": "ok",
                    "errorWhen": "Faulted, failed and orphaned outcomes; diagnostics are capped at 500 characters."
                }
            }
        },
        "metrics": {
            "pi.durable.reports": {
                "kind": "counter",
                "description": "Reports raised, grouped by fixed ReportInfo type.",
                "attributes": ["type"]
            },
            "pi.durable.phase.duration": {
                "kind": "histogram",
                "unit": "ms",
                "description": "Duration of a phase invocation and its decision commit.",
                "attributes": ["task", "phase", "outcome"]
            },
            "pi.durable.tasks.active": {
                "kind": "upDownCounter",
                "description": "Live tasks held by this Harness.",
                "attributes": ["task"]
            }
        }
    })
}

/// Opaque adapter-owned propagation data (upstream `TelemetryCarrier`).
pub type TelemetryCarrier = JsonObject;

/// Handler of one content-free telemetry stream.
pub trait TelemetrySink: Send + Sync {
    /// Start a child span named `name` with start attributes and links. The
    /// returned sink records that span's attributes, events, links, status and
    /// metrics; `end_span` settles it. `None` means this sink records no spans,
    /// and the caller keeps using the surrounding context.
    fn start_span(
        &self,
        _name: &str,
        _attributes: JsonObject,
        _links: Vec<TelemetryCarrier>,
    ) -> Option<Arc<dyn TelemetrySpanSink>> {
        None
    }

    /// Adapter-owned carrier of the current span, stored in durable records so a
    /// later invocation can extract it as the parent span.
    fn inject(&self) -> Option<TelemetryCarrier> {
        None
    }

    /// A context whose parent is the span identified by `carrier`.
    fn extract(&self, _carrier: &TelemetryCarrier) -> Option<Arc<dyn TelemetrySink>> {
        None
    }

    /// Merge attributes into the current span.
    fn set_attributes(&self, attributes: JsonObject);
    /// Add a named event with attributes.
    fn add_event(&self, name: &str, attributes: JsonObject);
    /// Link the current span to an opaque carrier.
    fn add_link(&self, carrier: JsonValue);
    /// Record one metric value with attributes.
    fn record_metric(&self, name: &str, value: f64, attributes: JsonObject);

    /// Set the current span's status; an error message is kept bounded.
    fn set_status(&self, _status: &str, _message: Option<&str>) {}
}

/// A live span created by [`TelemetrySink::start_span`]. Its end attributes and
/// final status settle the span exactly once.
pub trait TelemetrySpanSink: TelemetrySink {
    /// Merge end attributes, set the final status and settle the span.
    fn end_span(&self, attributes: JsonObject, status: &str, message: Option<&str>);
}

/// A telemetry context; a no-op without a host sink.
#[derive(Clone, Default)]
pub struct TelemetryContext {
    sink: Option<Arc<dyn TelemetrySink>>,
}

impl std::fmt::Debug for TelemetryContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryContext")
            .field("enabled", &self.sink.is_some())
            .finish()
    }
}

impl TelemetryContext {
    /// A context that records nothing.
    pub fn none() -> Self {
        Self { sink: None }
    }

    /// A context backed by `sink`.
    pub fn new(sink: Arc<dyn TelemetrySink>) -> Self {
        Self { sink: Some(sink) }
    }

    /// Whether a sink is attached.
    pub fn enabled(&self) -> bool {
        self.sink.is_some()
    }

    /// Merge attributes into the current span.
    pub fn set_attributes(&self, attributes: JsonObject) {
        if let Some(sink) = &self.sink {
            sink.set_attributes(attributes);
        }
    }

    /// Add one named event.
    pub fn add_event(&self, name: &str, attributes: JsonObject) {
        if let Some(sink) = &self.sink {
            sink.add_event(name, attributes);
        }
    }

    /// Link the current span to an opaque carrier.
    pub fn add_link(&self, carrier: JsonValue) {
        if let Some(sink) = &self.sink {
            sink.add_link(carrier);
        }
    }

    /// Record one metric value with attributes.
    pub fn record_metric(&self, name: &str, value: f64, attributes: JsonObject) {
        if let Some(sink) = &self.sink {
            sink.record_metric(name, value, attributes);
        }
    }

    /// Set the current span's status, capping an error message at 500 characters.
    pub fn set_status(&self, status: &str, message: Option<&str>) {
        if let Some(sink) = &self.sink {
            let message = message.map(|message| message.chars().take(500).collect::<String>());
            sink.set_status(status, message.as_deref());
        }
    }

    /// Adapter-owned carrier of the current span, when the adapter propagates.
    pub fn inject(&self) -> Option<TelemetryCarrier> {
        self.sink.as_ref().and_then(|sink| sink.inject())
    }

    /// A context whose parent is the span identified by `carrier`; this context
    /// when the adapter does not propagate.
    pub fn extract(&self, carrier: &TelemetryCarrier) -> TelemetryContext {
        self.sink
            .as_ref()
            .and_then(|sink| sink.extract(carrier))
            .map(|sink| Self { sink: Some(sink) })
            .unwrap_or_else(|| self.clone())
    }

    /// Start a child span. Without a span-recording sink the returned handle
    /// forwards to this context and settles without recording a span.
    pub fn start_span(
        &self,
        name: &str,
        attributes: JsonObject,
        links: Vec<TelemetryCarrier>,
    ) -> TelemetrySpan {
        let Some(sink) = &self.sink else {
            return TelemetrySpan::detached();
        };
        let Some(span) = sink.start_span(name, attributes.clone(), links.clone()) else {
            // A flat sink records no spans: keep the previous attribute/link
            // surface instead of dropping the start data.
            if !attributes.is_empty() {
                sink.set_attributes(attributes);
            }
            for link in links {
                sink.add_link(JsonValue::Object(link));
            }
            return TelemetrySpan {
                context: self.clone(),
                end: None,
                status: Mutex::new(("ok".to_string(), None)),
                ended: false,
            };
        };
        let end_sink = Arc::clone(&span);
        let context = TelemetryContext {
            sink: Some(span as Arc<dyn TelemetrySink>),
        };
        TelemetrySpan {
            context,
            end: Some(Box::new(move |attributes, status, message| {
                end_sink.end_span(attributes, status, message)
            })),
            status: Mutex::new(("ok".to_string(), None)),
            ended: false,
        }
    }
}

type SpanEnd = Box<dyn FnOnce(JsonObject, &str, Option<&str>) + Send + Sync>;

/// One live telemetry span; dropping it without [`TelemetrySpan::end`] settles
/// it with its last status and no end attributes.
pub struct TelemetrySpan {
    context: TelemetryContext,
    end: Option<SpanEnd>,
    status: Mutex<(String, Option<String>)>,
    ended: bool,
}

impl std::fmt::Debug for TelemetrySpan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetrySpan")
            .field("ended", &self.ended)
            .finish()
    }
}

impl TelemetrySpan {
    fn detached() -> Self {
        Self {
            context: TelemetryContext::none(),
            end: None,
            status: Mutex::new(("ok".to_string(), None)),
            ended: false,
        }
    }

    /// The span-scoped context installed for the work inside it.
    pub fn context(&self) -> TelemetryContext {
        self.context.clone()
    }

    /// Merge attributes into the span.
    pub fn set_attributes(&self, attributes: JsonObject) {
        self.context.set_attributes(attributes);
    }

    /// Add one event to the span.
    pub fn add_event(&self, name: &str, attributes: JsonObject) {
        self.context.add_event(name, attributes);
    }

    /// Link the span to an opaque carrier.
    pub fn add_link(&self, carrier: TelemetryCarrier) {
        self.context.add_link(JsonValue::Object(carrier));
    }

    /// Set the span's final status; an error message is capped at 500 characters.
    pub fn set_status(&self, status: &str, message: Option<&str>) {
        if let Ok(mut current) = self.status.lock() {
            *current = (
                status.to_string(),
                message.map(|message| message.chars().take(500).collect()),
            );
        }
    }

    /// Merge end attributes and settle the span. Idempotent through `Drop`.
    pub fn end(mut self, attributes: JsonObject) {
        self.finish(attributes);
    }

    fn finish(&mut self, attributes: JsonObject) {
        if self.ended {
            return;
        }
        self.ended = true;
        let (status, message) = self
            .status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_else(|_| ("ok".to_string(), None));
        if let Some(end) = self.end.take() {
            end(attributes, &status, message.as_deref());
        }
    }
}

impl Drop for TelemetrySpan {
    fn drop(&mut self) {
        self.finish(JsonObject::new());
    }
}
