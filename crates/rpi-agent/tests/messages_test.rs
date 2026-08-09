//! Port of the conversion-logic test intent from
//! `packages/coding-agent` messages tests (bashExecutionToText /
//! convertToLlm), anchored on `external/pi` @ 2efa728.
//!
//! Assertions are byte-level: the produced strings feed provider payloads,
//! so prefixes/suffixes are part of the parity contract.

use rpi_agent::messages::{
    bash_execution_to_text, convert_to_llm, AgentMessage, BashExecutionMessage, BashExecutionRole,
    BranchSummaryMessage, BranchSummaryRole, CompactionSummaryMessage, CompactionSummaryRole,
    CustomMessage, CustomRole, BRANCH_SUMMARY_PREFIX, BRANCH_SUMMARY_SUFFIX,
    COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX,
};
use rpi_ai::types::{
    AssistantMessage, AssistantRole, Message, StopReason, TextContent, ToolResultContent,
    ToolResultMessage, ToolResultRole, Usage, UserContent, UserContentBlock, UserMessage, UserRole,
};

fn bash_msg() -> BashExecutionMessage {
    BashExecutionMessage {
        role: BashExecutionRole::BashExecution,
        command: "ls -la".to_owned(),
        output: "total 0".to_owned(),
        exit_code: Some(0),
        cancelled: false,
        truncated: false,
        full_output_path: None,
        timestamp: 42,
        exclude_from_context: None,
    }
}

fn assistant_msg(text: &str) -> AssistantMessage {
    AssistantMessage {
        role: AssistantRole::Assistant,
        content: vec![rpi_ai::types::AssistantContent::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        api: "faux".into(),
        provider: "faux".to_owned(),
        model: "faux-1".to_owned(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: 7,
    }
}

fn user_text(message: &Message) -> &str {
    let Message::User(user) = message else {
        panic!("expected user message, got {message:?}");
    };
    let UserContent::Blocks(blocks) = &user.content else {
        panic!("expected blocks content, got {:?}", user.content);
    };
    let [UserContentBlock::Text(text)] = blocks.as_slice() else {
        panic!("expected a single text block, got {blocks:?}");
    };
    &text.text
}

// ---------------------------------------------------------------------------
// bashExecutionToText (messages.ts:63-79)
// ---------------------------------------------------------------------------

#[test]
fn bash_execution_to_text_no_output() {
    let msg = BashExecutionMessage {
        output: String::new(),
        ..bash_msg()
    };
    assert_eq!(bash_execution_to_text(&msg), "Ran `ls -la`\n(no output)");
}

#[test]
fn bash_execution_to_text_with_output() {
    let msg = bash_msg();
    assert_eq!(
        bash_execution_to_text(&msg),
        "Ran `ls -la`\n```\ntotal 0\n```"
    );
}

#[test]
fn bash_execution_to_text_cancelled_wins_over_exit_code() {
    // `cancelled` is checked before `exitCode`: a cancelled command with a
    // non-zero exit code reports cancellation only.
    let msg = BashExecutionMessage {
        cancelled: true,
        exit_code: Some(137),
        ..bash_msg()
    };
    assert_eq!(
        bash_execution_to_text(&msg),
        "Ran `ls -la`\n```\ntotal 0\n```\n\n(command cancelled)"
    );
}

#[test]
fn bash_execution_to_text_exit_code_zero_has_no_suffix() {
    let msg = BashExecutionMessage {
        exit_code: Some(0),
        ..bash_msg()
    };
    assert_eq!(
        bash_execution_to_text(&msg),
        "Ran `ls -la`\n```\ntotal 0\n```"
    );
}

#[test]
fn bash_execution_to_text_non_zero_exit_code() {
    let msg = BashExecutionMessage {
        exit_code: Some(3),
        ..bash_msg()
    };
    assert_eq!(
        bash_execution_to_text(&msg),
        "Ran `ls -la`\n```\ntotal 0\n```\n\nCommand exited with code 3"
    );
}

#[test]
fn bash_execution_to_text_unknown_exit_code_has_no_suffix() {
    // `exitCode: undefined` upstream (still running / unknown) -> no suffix.
    let msg = BashExecutionMessage {
        exit_code: None,
        ..bash_msg()
    };
    assert_eq!(
        bash_execution_to_text(&msg),
        "Ran `ls -la`\n```\ntotal 0\n```"
    );
}

#[test]
fn bash_execution_to_text_truncated_with_full_output_path() {
    let msg = BashExecutionMessage {
        truncated: true,
        full_output_path: Some("/tmp/full.log".to_owned()),
        ..bash_msg()
    };
    assert_eq!(
        bash_execution_to_text(&msg),
        "Ran `ls -la`\n```\ntotal 0\n```\n\n[Output truncated. Full output: /tmp/full.log]"
    );
}

#[test]
fn bash_execution_to_text_truncated_without_path_has_no_suffix() {
    let missing = BashExecutionMessage {
        truncated: true,
        full_output_path: None,
        ..bash_msg()
    };
    assert_eq!(
        bash_execution_to_text(&missing),
        "Ran `ls -la`\n```\ntotal 0\n```"
    );

    let empty = BashExecutionMessage {
        truncated: true,
        full_output_path: Some(String::new()),
        ..bash_msg()
    };
    assert_eq!(
        bash_execution_to_text(&empty),
        "Ran `ls -la`\n```\ntotal 0\n```"
    );
}

#[test]
fn bash_execution_to_text_cancelled_and_truncated_stacks_suffixes() {
    let msg = BashExecutionMessage {
        cancelled: true,
        truncated: true,
        full_output_path: Some("/tmp/full.log".to_owned()),
        ..bash_msg()
    };
    assert_eq!(
        bash_execution_to_text(&msg),
        "Ran `ls -la`\n```\ntotal 0\n```\n\n(command cancelled)\n\n[Output truncated. Full output: /tmp/full.log]"
    );
}

// ---------------------------------------------------------------------------
// convertToLlm (messages.ts:120-164)
// ---------------------------------------------------------------------------

#[test]
fn convert_to_llm_filters_bash_execution_excluded_from_context() {
    let excluded = BashExecutionMessage {
        exclude_from_context: Some(true),
        ..bash_msg()
    };
    let out = convert_to_llm(&[AgentMessage::BashExecution(excluded)]);
    assert!(out.is_empty());

    for flag in [None, Some(false)] {
        let kept = BashExecutionMessage {
            exclude_from_context: flag,
            ..bash_msg()
        };
        let expected_text = bash_execution_to_text(&kept);
        let out = convert_to_llm(&[AgentMessage::BashExecution(kept)]);
        assert_eq!(out.len(), 1);
        assert_eq!(user_text(&out[0]), expected_text);
        let Message::User(user) = &out[0] else {
            unreachable!("checked above");
        };
        assert_eq!(user.timestamp, 42);
    }
}

#[test]
fn convert_to_llm_custom_string_content_becomes_blocks() {
    let msg = CustomMessage {
        role: CustomRole::Custom,
        custom_type: "notification".to_owned(),
        content: UserContent::Text("plain text".to_owned()),
        display: true,
        details: None,
        timestamp: 9,
    };
    let out = convert_to_llm(&[AgentMessage::Custom(msg)]);
    assert_eq!(out.len(), 1);
    assert_eq!(user_text(&out[0]), "plain text");
    let Message::User(user) = &out[0] else {
        unreachable!("checked above");
    };
    assert_eq!(user.timestamp, 9);
}

#[test]
fn convert_to_llm_custom_blocks_content_passes_through() {
    let blocks = vec![
        UserContentBlock::Text(TextContent {
            text: "first".to_owned(),
            text_signature: None,
        }),
        UserContentBlock::Text(TextContent {
            text: "second".to_owned(),
            text_signature: None,
        }),
    ];
    let msg = CustomMessage {
        role: CustomRole::Custom,
        custom_type: "artifact".to_owned(),
        content: UserContent::Blocks(blocks.clone()),
        display: false,
        details: None,
        timestamp: 10,
    };
    let out = convert_to_llm(&[AgentMessage::Custom(msg)]);
    assert_eq!(out.len(), 1);
    let Message::User(user) = &out[0] else {
        panic!("expected user message");
    };
    assert_eq!(user.content, UserContent::Blocks(blocks));
}

#[test]
fn convert_to_llm_wraps_branch_summary_with_prefix_suffix() {
    let msg = BranchSummaryMessage {
        role: BranchSummaryRole::BranchSummary,
        summary: "branch body".to_owned(),
        from_id: "entry-1".to_owned(),
        timestamp: 11,
    };
    let out = convert_to_llm(&[AgentMessage::BranchSummary(msg)]);
    assert_eq!(out.len(), 1);
    assert_eq!(
        user_text(&out[0]),
        format!("{BRANCH_SUMMARY_PREFIX}branch body{BRANCH_SUMMARY_SUFFIX}")
    );
    // Byte-exact wrapper check (not just "contains").
    let text = user_text(&out[0]);
    assert!(text.starts_with("The following is a summary of a branch that this conversation came back from:\n\n<summary>\n"));
    assert!(text.ends_with("</summary>"));
}

#[test]
fn convert_to_llm_wraps_compaction_summary_with_prefix_suffix() {
    let msg = CompactionSummaryMessage {
        role: CompactionSummaryRole::CompactionSummary,
        summary: "compacted body".to_owned(),
        tokens_before: 1234,
        timestamp: 12,
    };
    let out = convert_to_llm(&[AgentMessage::CompactionSummary(msg)]);
    assert_eq!(out.len(), 1);
    assert_eq!(
        user_text(&out[0]),
        format!("{COMPACTION_SUMMARY_PREFIX}compacted body{COMPACTION_SUMMARY_SUFFIX}")
    );
    let text = user_text(&out[0]);
    assert!(text.starts_with("The conversation history before this point was compacted into the following summary:\n\n<summary>\n"));
    assert!(text.ends_with("\n</summary>"));
}

#[test]
fn convert_to_llm_passes_through_base_message_kinds() {
    let user = AgentMessage::User(UserMessage {
        role: UserRole::User,
        content: UserContent::Text("hello".to_owned()),
        timestamp: 1,
    });
    let assistant = AgentMessage::Assistant(assistant_msg("hi"));
    let tool_result = AgentMessage::ToolResult(ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: "call-1".to_owned(),
        tool_name: "read".to_owned(),
        content: vec![ToolResultContent::Text(TextContent {
            text: "out".to_owned(),
            text_signature: None,
        })],
        details: None,
        usage: None,
        added_tool_names: None,
        is_error: false,
        timestamp: 3,
    });

    let out = convert_to_llm(&[user.clone(), assistant.clone(), tool_result.clone()]);
    assert_eq!(out.len(), 3);
    let AgentMessage::User(u) = user else {
        unreachable!()
    };
    let AgentMessage::Assistant(a) = assistant else {
        unreachable!()
    };
    let AgentMessage::ToolResult(t) = tool_result else {
        unreachable!()
    };
    assert_eq!(out[0], Message::User(u));
    assert_eq!(out[1], Message::Assistant(a));
    assert_eq!(out[2], Message::ToolResult(t));
}
