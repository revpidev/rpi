//! T17 integration tests: the two scenarios this task originated from —
//! bash `lscpu` rendering (`$ lscpu` + raw output + `Took X.Xs`) and the
//! streaming write preview (clamped, real newlines, no JSON dump) — plus the
//! 1s `Elapsed` ticker, exercised end to end through
//! `ToolExecutionComponent` with the built-in renderer registry (no
//! extension definition involved).
//!
//! Upstream anchors: tool-execution.ts:57 (built-in definitions),
//! bash.ts:231-237/462-496, write.ts:136-167/232-266.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rpi::core::themes::load_theme;
use rpi::modes::interactive::components::tool_execution::{
    ToolExecutionComponent, ToolExecutionOptions, ToolResultContentLoose, ToolResultState,
};
use rpi_tui::tui::{Component, RenderHandle};
use serde_json::json;

fn theme() -> Arc<rpi::core::themes::Theme> {
    Arc::new(load_theme("dark", None).expect("builtin dark theme"))
}

fn bash_component(args: serde_json::Value) -> ToolExecutionComponent {
    ToolExecutionComponent::new(
        "bash",
        "call_1",
        args,
        ToolExecutionOptions::default(),
        None,
        theme(),
        RenderHandle::new(|| {}),
        "/cwd",
    )
}

/// The originating case: `lscpu` renders as `$ lscpu` + raw output +
/// `Took X.Xs`, not `bash\n{ "command": "lscpu" }`.
#[test]
fn bash_lscpu_case_matches_upstream_shape() {
    let mut component = bash_component(json!({"command": "lscpu"}));
    component.mark_execution_started();
    component.update_result(
        ToolResultState {
            content: vec![ToolResultContentLoose::text(
                "Architecture:                    x86_64\nCPU(s):                          16",
            )],
            is_error: false,
            details: None,
        },
        false,
    );
    let out = rpi_test_support::vt::strip_ansi(&component.render(80).join("\n"));
    assert!(out.contains("$ lscpu"), "out: {out}");
    assert!(!out.contains("\"command\""), "out: {out}");
    assert!(
        out.contains("Architecture:                    x86_64"),
        "out: {out}"
    );
    assert!(out.contains("Took 0."), "out: {out}");
}

/// The 1s `Elapsed` ticker (bash.ts:474-476): during a quiet partial
/// execution the ticker requests a re-render every second, and the displayed
/// label flips from `Elapsed` to `Took` once settled.
#[test]
fn bash_elapsed_ticker_ticks_during_partial_and_settles() {
    let renders = Arc::new(AtomicU64::new(0));
    let renders_in_handle = Arc::clone(&renders);
    let mut component = ToolExecutionComponent::new(
        "bash",
        "call_1",
        json!({"command": "sleep 5"}),
        ToolExecutionOptions::default(),
        None,
        theme(),
        RenderHandle::new(move || {
            renders_in_handle.fetch_add(1, Ordering::SeqCst);
        }),
        "/cwd",
    );
    component.mark_execution_started();
    component.update_result(
        ToolResultState {
            content: vec![],
            is_error: false,
            details: None,
        },
        true,
    );
    let out = rpi_test_support::vt::strip_ansi(&component.render(80).join("\n"));
    assert!(out.contains("Elapsed 0."), "out: {out}");

    std::thread::sleep(Duration::from_millis(1300));
    let ticks = renders.load(Ordering::SeqCst);
    assert!(
        ticks >= 1,
        "expected the 1s ticker to request renders, got {ticks}"
    );

    component.update_result(
        ToolResultState {
            content: vec![ToolResultContentLoose::text("done")],
            is_error: false,
            details: None,
        },
        false,
    );
    let settled_ticks = renders.load(Ordering::SeqCst);
    let out = rpi_test_support::vt::strip_ansi(&component.render(80).join("\n"));
    // ~1.3s elapsed since execution start, so the settled label reads
    // `Took 1.Xs` (formatDuration keeps one decimal).
    assert!(out.contains("Took 1."), "out: {out}");
    std::thread::sleep(Duration::from_millis(1200));
    let after = renders.load(Ordering::SeqCst);
    assert_eq!(settled_ticks, after, "ticker must stop once settled");
}

/// Streaming write: the preview grows with the streamed content (real
/// newlines, never a literal `\n` JSON escape), the collapsed preview stays
/// clamped to 10 content lines, and a successful result renders nothing
/// (upstream `formatWriteResult` returns `undefined` for non-errors).
#[test]
fn write_streaming_preview_grows_but_stays_clamped() {
    let mut component = ToolExecutionComponent::new(
        "write",
        "call_w",
        json!({"path": "/tmp/rpi.txt", "content": ""}),
        ToolExecutionOptions::default(),
        None,
        theme(),
        RenderHandle::new(|| {}),
        "/cwd",
    );

    let mut content = String::new();
    for round in 0..5usize {
        for i in 0..5 {
            content.push_str(&format!("line {} content\n", round * 5 + i));
        }
        component.update_args(json!({"path": "/tmp/rpi.txt", "content": content}));
        let lines = component.render(80);
        let out = rpi_test_support::vt::strip_ansi(&lines.join("\n"));
        assert!(
            out.contains("write /tmp/rpi.txt"),
            "round {round} out: {out}"
        );
        assert!(!out.contains("\\n"), "round {round} out: {out}");
        assert!(out.contains("line 0 content"), "round {round} out: {out}");
        // Collapsed preview: ≤ 10 content lines + hint + frame overhead.
        assert!(lines.len() <= 16, "round {round}: {} lines", lines.len());
    }
    let out = rpi_test_support::vt::strip_ansi(&component.render(80).join("\n"));
    assert!(out.contains("... (15 more lines, 25 total,"), "out: {out}");

    component.set_args_complete();
    component.mark_execution_started();
    component.update_result(
        ToolResultState {
            content: vec![ToolResultContentLoose::text(
                "Successfully wrote 250 bytes to /tmp/rpi.txt",
            )],
            is_error: false,
            details: None,
        },
        false,
    );
    let out = rpi_test_support::vt::strip_ansi(&component.render(80).join("\n"));
    assert!(!out.contains("Successfully wrote"), "out: {out}");
}
