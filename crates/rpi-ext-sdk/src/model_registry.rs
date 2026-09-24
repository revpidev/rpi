//! `ctx.modelRegistry.stream()` / `streamSimple()` (#8964, V15-09) —
//! guest-side wrappers for the additive host-calls.
//!
//! Extensions call models through the configured providers with resolved
//! authentication (including providers registered with
//! `pi.registerProvider()`) instead of reimplementing transport. The
//! synchronous JSON host-call boundary cannot carry a live async iterable,
//! so the host **collects** the stream: the reply is
//! `{"events": [StreamEvent…], "result": AssistantMessage | null}` — the
//! same events an upstream `AssistantMessageEventStream` would emit, plus
//! the final message (`result()` equivalent). Setup failures (bad model
//! JSON, unbound session) answer an error event + error result.
//!
//! `options` carries the portable subset (`temperature` / `maxTokens` /
//! `reasoning` / `toolChoice` — `streamSimple` only for the latter two);
//! callback-bearing fields of the upstream options cannot cross JSON.
//!
//! rpi-docs: `extension-abi.md` §3 (method table) / §8.4 (V15-09 entry).

use serde_json::Value;

use crate::host_call;

/// `ctx.modelRegistry.stream` — the api-specific options variant
/// (`reasoning` is the `"off"`-inclusive `ModelThinkingLevel`).
pub const METHOD_STREAM: &str = "ctx.modelRegistry.stream";

/// `ctx.modelRegistry.streamSimple` — the provider-neutral options variant.
pub const METHOD_STREAM_SIMPLE: &str = "ctx.modelRegistry.streamSimple";

/// The collected outcome of one extension-initiated model call (#8964):
/// every emitted [`StreamEvent`] plus the final message. Events are raw
/// JSON (`{"type": "start" | "text_start" | … | "done" | "error"}`) —
/// typed views live with the host crate; guests deserialize what they
/// consume.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamOutcome {
    pub events: Vec<Value>,
    pub result: Option<Value>,
}

fn call(
    method: &str,
    model: &Value,
    context: &Value,
    options: Option<Value>,
) -> Result<StreamOutcome, String> {
    let reply = host_call(
        method,
        serde_json::json!({
            "model": model,
            "context": context,
            "options": options,
        }),
    )?;
    Ok(StreamOutcome {
        events: reply
            .get("events")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        result: reply
            .get("result")
            .filter(|value| !value.is_null())
            .cloned(),
    })
}

/// `ctx.modelRegistry.stream(model, context, options?)` — extension model
/// call through the configured provider. Requires capability `session`;
/// unbound sessions answer an error event + error result.
pub fn stream(
    model: &Value,
    context: &Value,
    options: Option<Value>,
) -> Result<StreamOutcome, String> {
    call(METHOD_STREAM, model, context, options)
}

/// `ctx.modelRegistry.streamSimple(model, context, options?)` — the
/// provider-neutral options variant (`reasoning` without `"off"`,
/// `toolChoice`).
pub fn stream_simple(
    model: &Value,
    context: &Value,
    options: Option<Value>,
) -> Result<StreamOutcome, String> {
    call(METHOD_STREAM_SIMPLE, model, context, options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Host target: the transport is unavailable (no `rpi_host_call`
    /// import) — the wrapper surfaces the structured error instead of
    /// panicking. The wire shape is covered by the host-side dispatch
    /// tests (`rpi-ext-host` wasm::host_call).
    #[test]
    fn stream_surfaces_transport_error_on_host_target() {
        let error = stream(&json!({"id": "m"}), &json!({"messages": []}), None)
            .expect_err("host target has no transport");
        assert!(
            error.contains("wasm32"),
            "error mentions the guest-only transport: {error}"
        );
    }
}
