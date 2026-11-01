//! Model-visible notice marking at request time (FR-J R1).
//!
//! Port of pi-subagents `src/extension/index.ts:626-651` @ v0.83.0
//! (`3a63e0ad`), applied on the **`context_with_system`** phase:
//! Pi/rpi send custom messages to the model as plain user messages, so every
//! model-visible subagent message is wrapped in a marker naming its source
//! (#3002). The host's `context` phase folds mid-conversation system messages
//! into the leading one whenever a handler changes the visible list, which
//! invalidates the provider's prompt cache; `context_with_system` passes the
//! handler's list through verbatim, so system messages keep their positions
//! (#3061/#3058).
//!
//! rpi adaptations (documented as [VARIANT] in the TE51 ledger):
//! - The tag and origin name the **rpi-subagents** extension, following the
//!   ADR-0028 debrand rule for model-visible strings (upstream says
//!   `pi_subagents_notice` / "pi-subagents").
//! - The marked set is the custom types this crate actually injects; the
//!   upstream view-only slash/admin types are never sent here.
//! - `/subagent-cost`-style view-only reports are not dropped: rpi does not
//!   emit them as custom messages (the host-side view-only filter remains the
//!   host's job), so this transform only wraps.

/// Custom message types the parent model can see (`SUBAGENT_MESSAGE_TYPES`
/// subset that rpi injects: completion notices and supervisor asks; the two
/// notice renderers are registered ahead of their producers).
pub const MARKED_MESSAGE_TYPES: [&str; 5] = [
    "subagent-notify",
    "subagent_supervisor_request",
    "subagent_steering_notice",
    "subagent_control_notice",
    // No producer in rpi today; kept so a future watchdog note cannot leak
    // unmarked into the model context (session_fork already lists it).
    "subagent_watchdog_warning",
];

/// Opening marker for one custom type.
fn notice_open(custom_type: &str) -> String {
    format!(
        "<rpi_subagents_notice type=\"{custom_type}\" origin=\"automated message from the rpi-subagents extension, not typed by the user\">"
    )
}

const NOTICE_CLOSE: &str = "</rpi_subagents_notice>";

/// Whether the content already carries this exact wrapper (strict prefix +
/// close, not a substring match, so quoted marker text cannot suppress it).
fn already_marked(content: &serde_json::Value, open: &str) -> bool {
    match content {
        serde_json::Value::String(text) => {
            text.starts_with(open) && text.trim_end().ends_with(NOTICE_CLOSE)
        }
        serde_json::Value::Array(blocks) => {
            let first = blocks
                .first()
                .and_then(|block| block.get("text"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let last = blocks
                .last()
                .and_then(|block| block.get("text"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            first.starts_with(open) && last.trim_end().ends_with(NOTICE_CLOSE)
        }
        _ => false,
    }
}

/// Wrap one custom message's content, keeping its other fields untouched.
fn mark_message(message: &serde_json::Value) -> serde_json::Value {
    if message.get("role").and_then(serde_json::Value::as_str) != Some("custom") {
        return message.clone();
    }
    let Some(custom_type) = message
        .get("customType")
        .and_then(serde_json::Value::as_str)
    else {
        return message.clone();
    };
    if !MARKED_MESSAGE_TYPES.contains(&custom_type) {
        return message.clone();
    }
    let open = notice_open(custom_type);
    let content = message
        .get("content")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    if already_marked(&content, &open) {
        return message.clone();
    }
    let marked = match content {
        serde_json::Value::String(text) => {
            serde_json::Value::String(format!("{open}\n{text}\n{NOTICE_CLOSE}"))
        }
        serde_json::Value::Array(mut blocks) => {
            blocks.insert(
                0,
                serde_json::json!({ "type": "text", "text": format!("{open}\n") }),
            );
            blocks.push(serde_json::json!({ "type": "text", "text": format!("\n{NOTICE_CLOSE}") }));
            serde_json::Value::Array(blocks)
        }
        // A custom message without a usable body still gets the marker so the
        // model can tell it apart from user input.
        _ => serde_json::Value::String(format!("{open}\n\n{NOTICE_CLOSE}")),
    };
    let mut out = message.clone();
    if let Some(object) = out.as_object_mut() {
        object.insert("content".to_string(), marked);
    }
    out
}

/// Machine-readable name for the channel (`EVENT_CONTEXT_WITH_SYSTEM` is the
/// host's constant; kept local to avoid a host-crate dependency).
pub const CONTEXT_WITH_SYSTEM_EVENT: &str = "context_with_system";

/// `context_with_system` handler: return the same list with every
/// model-visible subagent message marked. Order and system-message positions
/// are preserved by construction (only `content` changes in place).
pub fn handle_context_with_system(event: &serde_json::Value) -> serde_json::Value {
    let Some(messages) = event.get("messages").and_then(serde_json::Value::as_array) else {
        // No usable list: leave the host's current context untouched.
        return serde_json::json!({});
    };
    let marked: Vec<serde_json::Value> = messages.iter().map(mark_message).collect();
    serde_json::json!({ "messages": marked })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn marks_string_and_block_custom_messages() {
        let messages = vec![
            json!({"role":"user","content":[{"type":"text","text":"hi"}]}),
            json!({"role":"custom","customType":"subagent-notify","content":"done","display":true}),
            json!({"role":"custom","customType":"subagent_supervisor_request","content":[{"type":"text","text":"ask"}],"display":true}),
            json!({"role":"custom","customType":"other","content":"untouched"}),
        ];
        let event = json!({ "messages": messages });
        let out = handle_context_with_system(&event);
        let out = out["messages"].as_array().expect("messages");
        assert_eq!(out[0]["content"][0]["text"], json!("hi"));
        assert_eq!(
            out[1]["content"],
            json!(
                "<rpi_subagents_notice type=\"subagent-notify\" origin=\"automated message from the rpi-subagents extension, not typed by the user\">\ndone\n</rpi_subagents_notice>"
            )
        );
        assert_eq!(out[1]["display"], json!(true));
        let blocks = out[2]["content"].as_array().expect("blocks");
        assert_eq!(blocks.len(), 3);
        assert!(
            blocks[0]["text"]
                .as_str()
                .unwrap()
                .starts_with("<rpi_subagents_notice type=\"subagent_supervisor_request\"")
        );
        assert_eq!(blocks[1]["text"], json!("ask"));
        assert_eq!(blocks[2]["text"], json!("\n</rpi_subagents_notice>"));
        assert_eq!(out[3]["content"], json!("untouched"));
    }

    #[test]
    fn system_messages_keep_their_positions() {
        let messages = vec![
            json!({"role":"system","content":[{"type":"text","text":"lead"}]}),
            json!({"role":"custom","customType":"subagent-notify","content":"done"}),
            json!({"role":"system","content":[{"type":"text","text":"mid"}]}),
            json!({"role":"user","content":"next"}),
        ];
        let out = handle_context_with_system(&json!({ "messages": messages }));
        let out = out["messages"].as_array().expect("messages");
        assert_eq!(out.len(), 4);
        assert_eq!(out[0]["role"], json!("system"));
        assert_eq!(out[2]["role"], json!("system"));
        assert_eq!(out[2]["content"][0]["text"], json!("mid"));
        assert_eq!(out[3]["role"], json!("user"));
    }

    #[test]
    fn marking_is_idempotent_for_the_exact_wrapper() {
        let event = json!({
            "messages": [{"role":"custom","customType":"subagent-notify","content":"done"}]
        });
        let once = handle_context_with_system(&event);
        let twice = handle_context_with_system(&once);
        assert_eq!(once, twice);
        // Quoted marker text inside a body does not count as a wrapper.
        let event = json!({
            "messages": [{"role":"custom","customType":"subagent-notify",
                          "content":"<rpi_subagents_notice type=\"subagent-notify\" origin=\"automated message from the rpi-subagents extension, not typed by the user\">\nquoted\n</rpi_subagents_notice>"}]
        });
        let marked = handle_context_with_system(&event);
        let text = marked["messages"][0]["content"].as_str().unwrap();
        assert_eq!(text.matches("<rpi_subagents_notice").count(), 1, "{text}");
    }

    #[test]
    fn non_custom_roles_and_unknown_types_are_untouched() {
        let event = json!({
            "messages": [
                {"role":"user","customType":"subagent-notify","content":"done"},
                {"role":"custom","customType":"other","content":"done"},
            ]
        });
        assert_eq!(handle_context_with_system(&event), event);
    }

    #[test]
    fn missing_messages_leaves_the_context_untouched() {
        assert_eq!(handle_context_with_system(&json!({})), json!({}));
        assert_eq!(
            handle_context_with_system(&json!({ "messages": "nope" })),
            json!({})
        );
    }
}
