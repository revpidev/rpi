//! TE53 R22 joint frames: the real rpi-usage plugin installed over a recording
//! host, publishing the typed footer from the shared
//! `fixtures/joint_footer_golden.json` fixture, composed on the host's status
//! row with the TE52 statusline text.
//!
//! Boundary: linking both abi_stable root modules into one test binary
//! conflicts on the exported module-loader symbol (TE52 note), so the
//! statusline half lives in `crates/rpi-ext-statusline/tests/joint_frames.rs`
//! and reads the same golden file. This file drives the real plugin
//! install/dispatch path (no worker → synchronous jobs), pins the footer
//! strings with the real formatter, and asserts the xAI auto path is
//! cache-only (zero supplier requests).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rpi_ext_usage::config::{PercentageMode, UsageConfig};
use rpi_ext_usage::format::footer_text;
use rpi_ext_usage::{HostCall, HostError, dispatch_for_test, install_host_for_test};
use rpi_tui::utils::{visible_width, wrap_text_with_ansi};
use serde_json::{Value, json};

struct AgentDir(PathBuf);

impl AgentDir {
    fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("rpi-usage-joint-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("agent dir");
        rpi_test_env::set_var("RPI_CODING_AGENT_DIR", &dir);
        // The shared golden fixture's countdown is relative to this clock.
        rpi_test_env::set_var("RPI_USAGE_NOW_MS", "1800000000000");
        AgentDir(dir)
    }
}

impl Drop for AgentDir {
    fn drop(&mut self) {
        rpi_test_env::remove_var("RPI_CODING_AGENT_DIR");
        rpi_test_env::remove_var("RPI_USAGE_NOW_MS");
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn golden() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/joint_footer_golden.json");
    serde_json::from_str(
        &std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display())),
    )
    .expect("joint golden json")
}

#[derive(Default)]
struct FakeHost {
    state: Mutex<FakeState>,
}

#[derive(Default)]
struct FakeState {
    envelope: Value,
    fetch_calls: Vec<bool>,
    cached_calls: usize,
    statuses: Vec<Option<String>>,
    structured: Vec<(String, Value)>,
    calls: Vec<String>,
}

impl FakeHost {
    fn new(envelope: Value) -> Self {
        Self {
            state: Mutex::new(FakeState {
                envelope,
                ..FakeState::default()
            }),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut FakeState) -> R) -> R {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        f(&mut state)
    }

    fn fetch_calls(&self) -> usize {
        self.with(|state| state.fetch_calls.len())
    }

    fn cached_calls(&self) -> usize {
        self.with(|state| state.cached_calls)
    }

    fn statuses(&self) -> Vec<Option<String>> {
        self.with(|state| state.statuses.clone())
    }

    fn structured(&self) -> Vec<(String, Value)> {
        self.with(|state| state.structured.clone())
    }
}

impl HostCall for FakeHost {
    fn call(&self, method: &str, args: Value) -> Result<Value, HostError> {
        self.with(|state| {
            state.calls.push(method.to_owned());
            match method {
                "ctx.hasUI" => Ok(json!(true)),
                "ctx.model" => Ok(json!({"id": "grok-4.5", "provider": "xai"})),
                "ctx.usage.listProviders" => Ok(json!(["xai"])),
                "ctx.usage.cached" => {
                    state.cached_calls += 1;
                    Ok(json!({
                        "envelope": state.envelope,
                        "source": "cache",
                        "stale": false
                    }))
                }
                "ctx.usage.fetchDetailed" => {
                    let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
                    state.fetch_calls.push(force);
                    Ok(json!({
                        "envelope": state.envelope,
                        "source": "network",
                        "stale": false
                    }))
                }
                "ctx.sessionStats" => Ok(Value::Null),
                "ctx.usage.register" | "registerCommand" | "on" => Ok(Value::Null),
                "ui.setStatus" => {
                    state
                        .statuses
                        .push(args.get("text").and_then(Value::as_str).map(str::to_owned));
                    Ok(Value::Null)
                }
                "ui.emitStructured" => {
                    if args
                        .get("validateOnly")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        return Ok(Value::Null);
                    }
                    let method = args
                        .get("method")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    state
                        .structured
                        .push((method, args.get("data").cloned().unwrap_or(Value::Null)));
                    Ok(Value::Null)
                }
                other => Err(HostError {
                    kind: "unknownMethod".to_owned(),
                    message: format!("unexpected host call: {other}"),
                }),
            }
        })
    }
}

/// The TE52 statusline status text (its items renderer output).
const STATUSLINE: &str = "Test Model · Context 6% used · 70";

#[test]
fn real_usage_footer_composes_on_the_host_row_without_requests() {
    let _dir = AgentDir::new("plugin");
    let golden = golden();
    let host = FakeHost::new(golden["envelope"].clone());
    let receipt = install_host_for_test(&host);
    assert!(receipt.get("error").is_none(), "{receipt:#?}");

    // Explicit xAI command: exactly one network read, output via
    // ui.emitStructured, footer from the same result (no second request).
    dispatch_for_test(
        &host,
        None,
        &json!({"kind": "command", "name": "usage", "args": "xai"}),
    );
    assert_eq!(host.fetch_calls(), 1, "one explicit xAI read");
    assert_eq!(host.cached_calls(), 0);
    let report = host
        .structured()
        .into_iter()
        .find(|(method, _)| method == "usageText")
        .map(|(_, data)| data.as_str().unwrap_or_default().to_owned())
        .expect("usageText report");
    assert!(report.starts_with("xai (current)"), "{report}");
    assert!(report.contains("Window"), "{report}");
    let footer = golden["used"].as_str().expect("used golden").to_owned();
    assert_eq!(host.statuses().last(), Some(&Some(footer.clone())));

    // Automatic refresh: xAI is ExplicitOnly -> cache only, zero supplier
    // requests, unchanged footer. (The no-worker synchronous fallback has no
    // persistent per-session state, so it re-emits the same line; the worker
    // path's no-flicker rewrite guard is covered by the footer unit tests.)
    dispatch_for_test(
        &host,
        None,
        &json!({"kind": "event", "event": "model_select"}),
    );
    assert_eq!(host.cached_calls(), 1, "auto path reads cached");
    assert_eq!(host.fetch_calls(), 1, "auto path never runs the script");
    assert_eq!(host.statuses().last(), Some(&Some(footer.clone())));

    // The real formatter pins every shared golden variant.
    let envelope: rpi_ext_host::usage::UsageEnvelope =
        serde_json::from_value(golden["envelope"].clone()).expect("envelope");
    let now_ms = golden["now_ms"].as_i64().expect("now_ms");
    assert_eq!(
        footer_text(&envelope, &UsageConfig::default(), now_ms).as_deref(),
        golden["used"].as_str()
    );
    let remaining = UsageConfig {
        percentage: PercentageMode::Remaining,
        ..UsageConfig::default()
    };
    assert_eq!(
        footer_text(&envelope, &remaining, now_ms).as_deref(),
        golden["remaining"].as_str()
    );
    let static_reset = UsageConfig {
        reset_countdown: false,
        ..UsageConfig::default()
    };
    assert_eq!(
        footer_text(&envelope, &static_reset, now_ms).as_deref(),
        golden["static"].as_str()
    );

    // Host status-row composition: sorted keys (statusline before usage),
    // ` · ` separator, wrap-to-width with no truncation at 40/80/120.
    let row = format!("{STATUSLINE} · {footer}");
    for width in [40usize, 80, 120] {
        let lines = wrap_text_with_ansi(&row, width);
        for line in &lines {
            assert!(
                visible_width(line) <= width,
                "line exceeds {width}: {line:?}"
            );
        }
        let rejoined = lines.join(" ");
        assert_eq!(
            rejoined.split_whitespace().collect::<Vec<_>>(),
            row.split_whitespace().collect::<Vec<_>>(),
            "width {width} preserves the whole row"
        );
        if width == 40 {
            assert!(lines.len() > 1, "40 columns wrap the joint row");
        }
    }
}
