//! Jev router — a virtual model that plans on a strong model and implements
//! on a cheap one (V16-12 port of
//! `packages/coding-agent/examples/extensions/jev-router.ts` @ a13d35a74).
//!
//! Registers `jev/auto`, which routes between three OpenAI Codex models:
//!
//! - Planning: GPT-5.6 Sol for complex work, GPT-5.6 Terra otherwise. The Jev
//!   classifier rates the first user message; planning stays on the chosen
//!   model.
//! - Implementation: GPT-5.6 Luna.
//!
//! The planning model explores, plans, and makes the first edit. After the
//! first successful `edit` or `write` tool call, the next request of the same
//! turn goes to Luna, and the session stays there. The phase is router state:
//! the host stores it on the session branch, so it follows the session tree
//! and survives compaction. Requests outside the agent loop (compaction
//! summaries, `ctx.modelRegistry.*` calls) go to Luna.
//!
//! Requires TypeSafe credentials (`TYPESAFE_API_KEY`) and an OpenAI Codex
//! login. Usage: `rpi -e ./jev-router.wasm --model jev/auto`.

use rpi_ext_sdk::{Extension, export, host_call};
use serde_json::{Value, json};

const PROVIDER: &str = "openai-codex";
const SOL: &str = "gpt-5.6-sol";
const TERRA: &str = "gpt-5.6-terra";
const LUNA: &str = "gpt-5.6-luna";

/// Tools whose successful result means implementation has started.
const EDIT_TOOLS: [&str; 2] = ["edit", "write"];

fn register(ext: &mut Extension) {
    ext.virtual_model(
        json!({
            "provider": "jev",
            "id": "auto",
            "name": "Auto (Jev)",
            "thinkingLevels": ["low", "medium", "high", "xhigh"],
            // Shared by all three models; shown before the first response.
            "contextWindow": 272_000,
            "maxTokens": 128_000,
        }),
        route,
    );
}

export!(register);

fn route(request: Value) -> Result<Value, String> {
    if request.get("reason").and_then(Value::as_str) == Some("direct") {
        return route_to(&request, LUNA, None);
    }
    let state = request.get("state").filter(|state| !state.is_null());
    let Some(state) = state else {
        let model = choose_planning_model(&request)?;
        return route_to(
            &request,
            &model,
            Some(json!({ "phase": "planning", "model": model })),
        );
    };
    // The planning model made the first edit: hand the rest of the work to
    // Luna.
    if state.get("phase").and_then(Value::as_str) == Some("planning")
        && edited_this_turn(&request)
    {
        return route_to(
            &request,
            LUNA,
            Some(json!({ "phase": "implementation", "model": LUNA })),
        );
    }
    let model = state
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(TERRA)
        .to_owned();
    route_to(&request, &model, None)
}

/// Resolve a physical catalog model and build the route result.
fn route_to(request: &Value, model_id: &str, state: Option<Value>) -> Result<Value, String> {
    let model = host_call(
        "ctx.modelRegistry.find",
        json!({ "provider": PROVIDER, "modelId": model_id }),
    )?;
    if model.is_null() {
        return Err(format!("Model {PROVIDER}/{model_id} is not in the catalog"));
    }
    let mut result = json!({
        "model": model,
        // Pass the selected thinking level through as the reasoning effort
        // of the chosen model (the host clamps it to the target).
        "thinkingLevel": request.get("thinkingLevel").cloned().unwrap_or(json!("off")),
    });
    if let Some(state) = state {
        result["state"] = state;
    }
    Ok(result)
}

/// `lastUserText(messages)` — the text of the last user message.
fn last_user_text(messages: &Value) -> String {
    let Some(messages) = messages.as_array() else {
        return String::new();
    };
    let Some(content) = messages
        .iter()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .and_then(|message| message.get("content"))
    else {
        return String::new();
    };
    if let Some(text) = content.as_str() {
        return text.to_owned();
    }
    content
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Whether a tool call since the last user message edited a file
/// successfully.
fn edited_this_turn(request: &Value) -> bool {
    let Some(messages) = request.get("messages").and_then(Value::as_array) else {
        return false;
    };
    let last_user = messages
        .iter()
        .rposition(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .map(|index| index + 1)
        .unwrap_or(0);
    messages[last_user..].iter().any(|message| {
        message.get("role").and_then(Value::as_str) == Some("toolResult")
            && message
                .get("toolName")
                .and_then(Value::as_str)
                .is_some_and(|name| EDIT_TOOLS.contains(&name))
            && message.get("isError").and_then(Value::as_bool) != Some(true)
    })
}

/// Planning model for a new session: Sol for complex work, Terra otherwise
/// or when Jev is unavailable. A planning model the session already uses is
/// kept, so switching to `jev/auto` costs no cache miss.
fn choose_planning_model(request: &Value) -> Result<String, String> {
    if let Some(previous) = request.get("previous").and_then(|previous| previous.get("model"))
        && previous.get("provider").and_then(Value::as_str) == Some(PROVIDER)
        && let Some(id) = previous.get("id").and_then(Value::as_str)
        && (id == SOL || id == TERRA)
    {
        return Ok(id.to_owned());
    }
    let jev = host_call(
        "ctx.modelRegistry.findOfType",
        json!({ "type": "classifier", "provider": "typesafe", "modelId": "jev-latest" }),
    )?;
    if jev.is_null() {
        return Ok(TERRA.to_owned());
    }
    let result = host_call(
        "ctx.modelRegistry.classify",
        json!({
            "model": jev,
            "context": {
                "state": {
                    "prompt": last_user_text(request.get("messages").unwrap_or(&Value::Null))
                        .chars()
                        .take(16_000)
                        .collect::<String>(),
                },
                "questions": {
                    "complexity": {
                        "type": "choice",
                        "instructions": "How demanding is the software engineering work requested in `prompt`?",
                        "criteria": {
                            "standard": "Ordinary features, fixes, reviews, or questions",
                            "complex": "Subtle design, cross-cutting changes, or hard debugging",
                        },
                    },
                },
            },
            "options": {},
        }),
    )?;
    let complex = result
        .get("stopReason")
        .and_then(Value::as_str)
        .is_some_and(|reason| reason == "stop")
        && result
            .pointer("/answers/complexity/probabilities/complex")
            .and_then(Value::as_f64)
            .is_some_and(|probability| probability >= 0.5);
    Ok(if complex { SOL } else { TERRA }.to_owned())
}