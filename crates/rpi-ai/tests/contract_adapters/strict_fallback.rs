use super::*;

fn strict_tool(
    name: &str,
    strict: rpi_ai::types::ConstrainedSamplingStrict,
) -> rpi_ai::types::Tool {
    rpi_ai::types::Tool {
        name: name.to_owned(),
        description: "Look up a value".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {"value": {"type": "string"}, "optional": {"type": "number"}},
            "required": ["value"]
        }),
        constrained_sampling: Some(rpi_ai::types::ConstrainedSampling::Config(
            rpi_ai::types::ConstrainedSamplingConfig::JsonSchema { strict },
        )),
    }
}

fn strict_context(tools: Vec<rpi_ai::types::Tool>) -> rpi_ai::types::TranscriptContext {
    rpi_ai::utils::transcript::normalize_context(&Context {
        system_prompt: None,
        messages: vec![user_text("hello")],
        tools: Some(tools),
    })
}

const STRICT_GRAMMAR_ERROR: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"The compiled grammar is too large, which would cause performance issues."}}"#;
const STRICT_OTHER_ERROR: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Required grammar is still too large"}}"#;
const STRICT_AUTH_ERROR: &str = r#"{"type":"error","error":{"type":"authentication_error","message":"The compiled grammar is too large"}}"#;

#[tokio::test]
async fn preferred_strict_tools_fall_back_only_on_schema_limits() {
    let (base_url, mut captured) =
        serve(vec![(400, STRICT_GRAMMAR_ERROR), (200, ANTHROPIC_SSE)]).await;
    let m = model(
        ApiKind::ANTHROPIC_MESSAGES,
        "anthropic",
        &base_url,
        json!({"compat": {"supportsStrictTools": true}}),
    );
    let ctx = strict_context(vec![
        strict_tool(
            "preferred",
            rpi_ai::types::ConstrainedSamplingStrict::Prefer,
        ),
        strict_tool(
            "required",
            rpi_ai::types::ConstrainedSamplingStrict::Require,
        ),
        {
            let mut ordinary =
                strict_tool("ordinary", rpi_ai::types::ConstrainedSamplingStrict::Prefer);
            ordinary.constrained_sampling = None;
            ordinary
        },
    ]);
    let message = AnthropicMessages
        .stream(&m, &ctx, Some(options()))
        .result()
        .await
        .expect("completed");
    assert_eq!(message.stop_reason, StopReason::Stop, "{message:?}");
    let first = captured.recv().await.expect("first request").body_json();
    let second = captured.recv().await.expect("fallback request").body_json();
    assert_eq!(first["tools"][0]["strict"], json!(true));
    assert!(second["tools"][0].get("strict").is_none());
    assert_eq!(
        second["tools"][0]["input_schema"]["required"],
        json!(["value"])
    );
    assert_eq!(second["tools"][1]["strict"], json!(true));
    assert_eq!(second["tools"][2], first["tools"][2]);
    let diagnostics = message.diagnostics.expect("fallback diagnostic");
    assert_eq!(diagnostics[0].kind, "anthropic_strict_tools_fallback");
    assert_eq!(
        diagnostics[0].details.as_ref().unwrap()["reason"],
        json!("strict_tool_limits_exceeded")
    );
}

#[tokio::test]
async fn strict_fallback_applies_to_inline_additions_and_redefinitions() {
    let (base_url, mut captured) =
        serve(vec![(400, STRICT_GRAMMAR_ERROR), (200, ANTHROPIC_SSE)]).await;
    let m = model(
        ApiKind::ANTHROPIC_MESSAGES,
        "anthropic",
        &base_url,
        json!({"compat": {
            "supportsStrictTools": true,
            "supportsMidConvoSystemMessages": true,
            "supportsMidConvoToolChanges": true
        }}),
    );
    let mut ctx = strict_context(vec![strict_tool(
        "lookup",
        rpi_ai::types::ConstrainedSamplingStrict::Require,
    )]);
    for (index, strict) in [
        rpi_ai::types::ConstrainedSamplingStrict::Prefer,
        rpi_ai::types::ConstrainedSamplingStrict::Require,
    ]
    .into_iter()
    .enumerate()
    {
        ctx.messages
            .push(Message::System(rpi_ai::types::SystemMessage {
                role: Default::default(),
                content: Default::default(),
                sections: None,
                tools_added: Some(vec![strict_tool("lookup", strict)]),
                tools_removed: None,
                timestamp: index as i64 + 2,
            }));
    }
    let message = AnthropicMessages
        .stream(&m, &ctx, Some(options()))
        .result()
        .await
        .expect("completed");
    assert_eq!(message.stop_reason, StopReason::Stop, "{message:?}");
    let first = captured.recv().await.expect("first request").body_json();
    let second = captured.recv().await.expect("fallback request").body_json();
    let definitions = |body: &Value| {
        body["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .flat_map(|message| message["content"].as_array().cloned().unwrap_or_default())
            .filter(|block| block["type"] == json!("tool_addition"))
            .map(|block| block["tool"]["definition"].clone())
            .collect::<Vec<_>>()
    };
    let first = definitions(&first);
    let second = definitions(&second);
    assert_eq!(first.len(), 2, "{first:?}");
    assert_eq!(second.len(), 2, "{second:?}");
    assert!(
        first
            .iter()
            .all(|definition| definition["strict"] == json!(true))
    );
    assert!(second[0].get("strict").is_none());
    assert_eq!(second[0]["input_schema"]["required"], json!(["value"]));
    assert_eq!(second[1]["strict"], json!(true));
}

#[tokio::test]
async fn strict_fallback_rejects_required_wrong_errors_and_a_second_rejection() {
    let (base_url, mut captured) = serve(vec![(400, STRICT_GRAMMAR_ERROR)]).await;
    let m = model(
        ApiKind::ANTHROPIC_MESSAGES,
        "anthropic",
        &base_url,
        json!({"compat": {"supportsStrictTools": true}}),
    );
    let ctx = strict_context(vec![strict_tool(
        "required",
        rpi_ai::types::ConstrainedSamplingStrict::Require,
    )]);
    let message = AnthropicMessages
        .stream(&m, &ctx, Some(options()))
        .result()
        .await
        .expect("error");
    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(captured.recv().await.is_some());
    assert!(captured.try_recv().is_err());

    let (base_url, mut captured) = serve(vec![(400, STRICT_AUTH_ERROR)]).await;
    let m = model(
        ApiKind::ANTHROPIC_MESSAGES,
        "anthropic",
        &base_url,
        json!({"compat": {"supportsStrictTools": true}}),
    );
    let ctx = strict_context(vec![strict_tool(
        "preferred",
        rpi_ai::types::ConstrainedSamplingStrict::Prefer,
    )]);
    let message = AnthropicMessages
        .stream(&m, &ctx, Some(options()))
        .result()
        .await
        .expect("error");
    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(captured.recv().await.is_some());
    assert!(captured.try_recv().is_err());

    let (base_url, mut captured) =
        serve(vec![(400, STRICT_GRAMMAR_ERROR), (400, STRICT_OTHER_ERROR)]).await;
    let m = model(
        ApiKind::ANTHROPIC_MESSAGES,
        "anthropic",
        &base_url,
        json!({"compat": {"supportsStrictTools": true}}),
    );
    let ctx = strict_context(vec![
        strict_tool(
            "preferred",
            rpi_ai::types::ConstrainedSamplingStrict::Prefer,
        ),
        strict_tool(
            "required",
            rpi_ai::types::ConstrainedSamplingStrict::Require,
        ),
    ]);
    let message = AnthropicMessages
        .stream(&m, &ctx, Some(options()))
        .result()
        .await
        .expect("second rejection");
    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(
        message
            .error_message
            .as_deref()
            .is_some_and(|error| error.contains("Required grammar"))
    );
    let first = captured.recv().await.expect("first request").body_json();
    let second = captured.recv().await.expect("second request").body_json();
    assert!(second["tools"][0].get("strict").is_none());
    assert_eq!(second["tools"][1]["strict"], json!(true));
    assert_eq!(first["tools"][1]["strict"], json!(true));
    assert!(captured.try_recv().is_err());
}
