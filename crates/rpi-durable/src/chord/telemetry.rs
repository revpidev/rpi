//! Content-free telemetry context (port of the telemetry surface used by
//! `packages/durable/src/harness/telemetry.ts` @ `4a080252f`).
//!
//! A host supplies a [`TelemetrySink`]; phases and tools set attributes and
//! add events carrying durable record IDs, never prompts, tokens, or
//! credentials. Without a sink every call is a no-op.

use std::sync::Arc;

use crate::chord::json::{JsonObject, JsonValue};

/// Durable's content-free span, event and metric vocabulary. IDs address
/// durable records, never content.
pub fn durable_telemetry_schema() -> JsonValue {
    serde_json::json!({
        "version": 1,
        "spans": {
            "pi.durable.phase": {
                "startAttributes": ["task.id", "task.name", "phase", "kind", "attempt", "resumed"],
                "endAttributes": ["outcome", "tool.name", "tool.call_id"],
                "events": ["pi.durable.entry", "pi.durable.submission", "pi.durable.report", "pi.durable.checkpoint", "pi.durable.replay"]
            }
        },
        "metrics": ["pi.durable.reports", "pi.durable.phase.duration", "pi.durable.tasks.active"]
    })
}

/// Handler of one content-free telemetry stream.
pub trait TelemetrySink: Send + Sync {
    /// Merge attributes into the current span.
    fn set_attributes(&self, attributes: JsonObject);
    /// Add a named event with attributes.
    fn add_event(&self, name: &str, attributes: JsonObject);
    /// Link the current span to an opaque carrier.
    fn add_link(&self, carrier: JsonValue);
    /// Record one metric value.
    fn record_metric(&self, name: &str, value: f64);
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

    /// Record one metric value.
    pub fn record_metric(&self, name: &str, value: f64) {
        if let Some(sink) = &self.sink {
            sink.record_metric(name, value);
        }
    }
}
