//! Interactive-mode component render snapshots (T12-S4a gate).
//!
//! Each case renders a component at a fixed width through the public
//! `Component::render(width)` contract and compares the raw ANSI line output
//! against a golden file in `crates/rpi/tests/snapshots/<name>.snap` (same
//! pattern as rpi-tui `tests/snapshots.rs`). The snapshots are observations
//! of the frozen upstream behavior; regenerate with:
//!
//! ```sh
//! RPI_UPDATE_SNAPSHOTS=1 cargo test -p rpi --lib -- snapshots
//! ```
//!
//! and review the diff before committing.

#![cfg(test)]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use rpi_agent::messages::{
    BranchSummaryMessage, BranchSummaryRole, CompactionSummaryMessage, CompactionSummaryRole,
    CustomMessage, CustomRole,
};
use rpi_ai::types::{
    AssistantContent, AssistantMessage, AssistantRole, StopReason, TextContent, ThinkingContent,
    Usage, UserContent,
};
use rpi_tui::tui::Component as _;
use rpi_tui::tui::RenderHandle;

use crate::core::agent_session::ParsedSkillBlock;
use crate::core::themes::{load_theme, Theme};
use crate::modes::interactive::components::{
    render_diff, AssistantMessageComponent, BashExecutionComponent, BranchSummaryMessageComponent,
    BranchSummaryStatusIndicator, CompactionStatusIndicator, CompactionStatusReason,
    CompactionSummaryMessageComponent, CustomMessageComponent, RenderDiffOptions,
    RetryStatusIndicator, SharedStatusIndicator, SkillInvocationMessageComponent,
    ToolExecutionComponent, ToolExecutionOptions, ToolResultContentLoose, ToolResultState,
    UserMessageComponent,
};
use crate::modes::interactive::custom_editor::CustomEditor;
use crate::modes::interactive::theme::markdown_theme;

fn snapshot_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots")
}

/// Compare `lines` against `tests/snapshots/<name>.snap`; rewrite the golden
/// when `RPI_UPDATE_SNAPSHOTS=1` is set.
fn assert_snapshot(name: &str, lines: &[String]) {
    let actual = format!("{}\n", lines.join("\n"));
    let path = snapshot_dir().join(format!("{name}.snap"));

    if std::env::var_os("RPI_UPDATE_SNAPSHOTS").is_some() {
        fs::create_dir_all(snapshot_dir())
            .unwrap_or_else(|err| panic!("create snapshot dir: {err}"));
        fs::write(&path, &actual).unwrap_or_else(|err| panic!("write {path:?}: {err}"));
        return;
    }

    let expected = fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "missing/unreadable snapshot {path:?}: {err}; regenerate with RPI_UPDATE_SNAPSHOTS=1"
        )
    });
    assert_eq!(
        actual, expected,
        "snapshot {name} drifted; if intentional, regenerate with RPI_UPDATE_SNAPSHOTS=1 \
         (escapes shown via debug formatting)\nactual:   {actual:?}\nexpected: {expected:?}"
    );
}

fn dark_theme() -> Arc<Theme> {
    Arc::new(load_theme("dark", None).expect("builtin dark theme"))
}

fn noop_render_handle() -> RenderHandle {
    RenderHandle::new(|| {})
}

// --- user-message (user-message.ts) -----------------------------------------

#[test]
fn user_message_simple() {
    let component = UserMessageComponent::new(
        "hello",
        dark_theme(),
        markdown_theme(&load_theme("dark", None).unwrap()),
        1,
        Vec::new(),
    );
    assert_snapshot("user_message_simple", &component.render(20));
}

// --- assistant-message (assistant-message.ts) -------------------------------

fn assistant_message(content: Vec<AssistantContent>) -> AssistantMessage {
    AssistantMessage {
        role: AssistantRole::Assistant,
        content,
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        model: "m".into(),
        response_model: None,
        response_id: None,
        provider_thinking_level: None,
        diagnostics: None,
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: 0,
        deferred: None,
        end_turn: None,
        raw_stop_reason: None,
    }
}

fn text_content(s: &str) -> AssistantContent {
    AssistantContent::Text(TextContent {
        text: s.to_string(),
        text_signature: None,
    })
}

fn thinking_content(s: &str) -> AssistantContent {
    AssistantContent::Thinking(ThinkingContent {
        thinking: s.to_string(),
        thinking_signature: None,
        redacted: None,
    })
}

#[test]
fn assistant_message_text_and_thinking() {
    let message = assistant_message(vec![
        thinking_content("Let me think about this carefully."),
        text_content("Here is the answer."),
    ]);
    let component = AssistantMessageComponent::new(
        Some(message),
        false,
        dark_theme(),
        markdown_theme(&load_theme("dark", None).unwrap()),
        "Thinking...",
        1,
        Vec::new(),
    );
    assert_snapshot("assistant_message_text_and_thinking", &component.render(60));
}

#[test]
fn assistant_message_hidden_thinking() {
    let message = assistant_message(vec![
        thinking_content("Secret reasoning."),
        text_content("Answer."),
    ]);
    let component = AssistantMessageComponent::new(
        Some(message),
        true,
        dark_theme(),
        markdown_theme(&load_theme("dark", None).unwrap()),
        "Thinking...",
        1,
        Vec::new(),
    );
    assert_snapshot("assistant_message_hidden_thinking", &component.render(60));
}

#[test]
fn assistant_message_length_stop() {
    let mut message = assistant_message(vec![text_content("Partial response")]);
    message.stop_reason = StopReason::Length;
    let component = AssistantMessageComponent::new(
        Some(message),
        false,
        dark_theme(),
        markdown_theme(&load_theme("dark", None).unwrap()),
        "Thinking...",
        1,
        Vec::new(),
    );
    assert_snapshot("assistant_message_length_stop", &component.render(60));
}

// --- compaction / branch / skill summaries ---------------------------------

#[test]
fn compaction_summary_collapsed() {
    let component = CompactionSummaryMessageComponent::new(
        CompactionSummaryMessage {
            role: CompactionSummaryRole::CompactionSummary,
            summary: "The user asked about the parser and we discussed error handling.".into(),
            tokens_before: 12345,
            timestamp: 0,
        },
        dark_theme(),
        markdown_theme(&load_theme("dark", None).unwrap()),
    );
    assert_snapshot("compaction_summary_collapsed", &component.render(60));
}

#[test]
fn compaction_summary_expanded() {
    let mut component = CompactionSummaryMessageComponent::new(
        CompactionSummaryMessage {
            role: CompactionSummaryRole::CompactionSummary,
            summary: "The user asked about the parser and we discussed error handling.".into(),
            tokens_before: 12345,
            timestamp: 0,
        },
        dark_theme(),
        markdown_theme(&load_theme("dark", None).unwrap()),
    );
    component.set_expanded(true);
    assert_snapshot("compaction_summary_expanded", &component.render(60));
}

#[test]
fn branch_summary_collapsed() {
    let component = BranchSummaryMessageComponent::new(
        BranchSummaryMessage {
            role: BranchSummaryRole::BranchSummary,
            summary: "We refactored the tokenizer.".into(),
            from_id: "e1".into(),
            timestamp: 0,
        },
        dark_theme(),
        markdown_theme(&load_theme("dark", None).unwrap()),
    );
    assert_snapshot("branch_summary_collapsed", &component.render(60));
}

#[test]
fn skill_invocation_collapsed() {
    let component = SkillInvocationMessageComponent::new(
        ParsedSkillBlock {
            name: "plan".into(),
            location: "skills/plan".into(),
            content: "1. Investigate the codebase.\n2. Write a plan.".into(),
            user_message: Some("use the plan skill".into()),
        },
        dark_theme(),
        markdown_theme(&load_theme("dark", None).unwrap()),
    );
    assert_snapshot("skill_invocation_collapsed", &component.render(60));
}

// --- custom-message (custom-message.ts) -------------------------------------

#[test]
fn custom_message_default() {
    let component = CustomMessageComponent::new(
        CustomMessage {
            role: CustomRole::Custom,
            custom_type: "artifact".into(),
            content: UserContent::Text("Here is the generated artifact with **markdown**.".into()),
            display: true,
            details: None,
            timestamp: 0,
        },
        None,
        dark_theme(),
        markdown_theme(&load_theme("dark", None).unwrap()),
        1,
    );
    assert_snapshot("custom_message_default", &component.render(60));
}

// --- bash-execution (bash-execution.ts) -------------------------------------

#[test]
fn bash_execution_complete() {
    let mut component =
        BashExecutionComponent::new("ls -la", noop_render_handle(), dark_theme(), false);
    for i in 0..25 {
        component.append_output(&format!("file_{i}.txt\n"));
    }
    component.set_complete(Some(0), false, None, None);
    assert_snapshot("bash_execution_complete", &component.render(60));
}

#[test]
fn bash_execution_error() {
    let mut component =
        BashExecutionComponent::new("false", noop_render_handle(), dark_theme(), false);
    component.append_output("some error output\n");
    component.set_complete(Some(2), false, None, None);
    assert_snapshot("bash_execution_error", &component.render(60));
}

// --- tool-execution (tool-execution.ts) -------------------------------------

#[test]
fn tool_execution_fallback() {
    // A tool with no built-in render definition (T17) exercises the generic
    // `formatToolExecution` fallback snapshot.
    let mut component = ToolExecutionComponent::new(
        "custom-tool",
        "call_1",
        serde_json::json!({"path": "src/main.rs"}),
        ToolExecutionOptions::default(),
        None,
        dark_theme(),
        noop_render_handle(),
        "/cwd",
    );
    component.update_result(
        ToolResultState {
            content: vec![ToolResultContentLoose::text("pub fn main() {}")],
            is_error: false,
            details: None,
        },
        false,
    );
    assert_snapshot("tool_execution_fallback", &component.render(60));
}

// --- diff (diff.ts) ---------------------------------------------------------

#[test]
fn diff_render_hunk() {
    let input = " 12   const a = 1;\n-13   const a = 2;\n+13   const a = 3;\n 14   const b = 4;";
    let rendered = render_diff(
        input,
        &load_theme("dark", None).unwrap(),
        RenderDiffOptions::default(),
    );
    let lines: Vec<String> = rendered.split('\n').map(|s| s.to_string()).collect();
    assert_snapshot("diff_render_hunk", &lines);
}

// --- editor-border status spinners (c1d4c8011, V15-07 FR-C) ----------------
//
// Golden frames for the three spinner states now embedded in the editor's
// top border (compaction / branch summary / retry) plus the custom-editor
// opt-out fallback (plain border; the standalone row hosts the indicator
// instead — rendered as its own frame below).

fn border_editor(embed: bool) -> std::sync::Arc<std::sync::Mutex<CustomEditor>> {
    use rpi_tui::components::editor::{EditorOptions, EditorTheme};
    use rpi_tui::tui_handle::TuiHandle;
    use rpi_tui::tui_main_screen::TuiMainScreen;

    let theme = dark_theme();
    let tui = TuiHandle::from_main(TuiMainScreen::new(Box::new(
        crate::modes::interactive::test_support::TestTerminal::new(),
    )));
    let editor_theme = EditorTheme {
        border_color: Box::new({
            let theme = Arc::clone(&theme);
            move |text: &str| theme.fg("borderMuted", text)
        }),
        select_list: Arc::new(rpi_tui::components::select_list::SelectListTheme::identity()),
    };
    std::sync::Arc::new(std::sync::Mutex::new(CustomEditor::new(
        tui,
        editor_theme,
        EditorOptions::default(),
        embed,
    )))
}

fn with_embedded_spinner(shared: SharedStatusIndicator, dispose: impl FnOnce()) -> Vec<String> {
    use rpi_tui::tui::Component as _;

    let editor = border_editor(true);
    lock_snapshot_editor(&editor).set_working_status_indicator(Some(shared));
    let region = crate::modes::interactive::custom_editor::CustomEditorRegion::new(editor);
    let lines = region.render(60);
    dispose();
    lines
}

fn lock_indicator<T>(indicator: &Arc<std::sync::Mutex<T>>) -> std::sync::MutexGuard<'_, T> {
    indicator
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_snapshot_editor(
    editor: &std::sync::Arc<std::sync::Mutex<CustomEditor>>,
) -> std::sync::MutexGuard<'_, CustomEditor> {
    editor
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[test]
fn editor_border_compaction_threshold_spinner() {
    let indicator = Arc::new(std::sync::Mutex::new(CompactionStatusIndicator::new(
        noop_render_handle(),
        CompactionStatusReason::Threshold,
        dark_theme(),
    )));
    let lines = with_embedded_spinner(
        SharedStatusIndicator::Compaction(Arc::clone(&indicator)),
        || lock_indicator(&indicator).dispose(),
    );
    assert_snapshot("editor_border_compaction_threshold_spinner", &lines);
}

#[test]
fn editor_border_branch_summary_spinner() {
    let indicator = Arc::new(std::sync::Mutex::new(BranchSummaryStatusIndicator::new(
        noop_render_handle(),
        dark_theme(),
    )));
    let lines = with_embedded_spinner(
        SharedStatusIndicator::BranchSummary(Arc::clone(&indicator)),
        || lock_indicator(&indicator).dispose(),
    );
    assert_snapshot("editor_border_branch_summary_spinner", &lines);
}

#[test]
fn editor_border_retry_spinner() {
    // 10s delay: the countdown message is deterministic at construction
    // ("Retrying (1/3) in 10s...").
    let indicator = Arc::new(std::sync::Mutex::new(RetryStatusIndicator::new(
        noop_render_handle(),
        1,
        3,
        10_000,
        dark_theme(),
    )));
    let lines = with_embedded_spinner(SharedStatusIndicator::Retry(Arc::clone(&indicator)), || {
        lock_indicator(&indicator).dispose()
    });
    assert_snapshot("editor_border_retry_spinner", &lines);
}

#[test]
fn editor_border_opt_out_keeps_plain_border_and_standalone_row() {
    use rpi_tui::tui::Component as _;

    // A custom editor that did not opt in keeps the plain top border…
    let editor = border_editor(false);
    let indicator = Arc::new(std::sync::Mutex::new(CompactionStatusIndicator::new(
        noop_render_handle(),
        CompactionStatusReason::Threshold,
        dark_theme(),
    )));
    lock_snapshot_editor(&editor).set_working_status_indicator(Some(
        SharedStatusIndicator::Compaction(Arc::clone(&indicator)),
    ));
    let region = crate::modes::interactive::custom_editor::CustomEditorRegion::new(editor);
    let mut lines = region.render(60);
    // …and the status row hosts the standalone indicator instead
    // (the mode's `ActiveStatus` render).
    lines.extend(lock_indicator(&indicator).render(60));
    lock_indicator(&indicator).dispose();
    assert_snapshot(
        "editor_border_opt_out_keeps_plain_border_and_standalone_row",
        &lines,
    );
}
