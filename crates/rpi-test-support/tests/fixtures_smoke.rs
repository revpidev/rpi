//! Smoke test: the T02 fixtures on disk must parse, normalize idempotently,
//! and carry no volatile data after normalization (coding-standards §12.3;
//! the T02 self-check "normalizer unit tests" applied to the real
//! fixtures).

use rpi_test_support::{diff_jsonl, Normalizer};

const SCENARIOS: &[&str] = &[
    "single-turn",
    "tool-calls",
    "steering-followup",
    "abort",
    "length-truncation",
];

fn fixture_path(scenario: &str, file: &str) -> String {
    format!(
        "{}/../../fixtures/generated/{scenario}/{file}",
        env!("CARGO_MANIFEST_DIR")
    )
}

#[test]
fn test_fixtures_exist_and_parse_as_jsonl() {
    for scenario in SCENARIOS {
        for file in ["session.jsonl", "events.jsonl"] {
            let path = fixture_path(scenario, file);
            let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
                panic!("fixture missing: {path}: {e} (run node fixtures/generate-fixtures.mjs)")
            });
            assert!(!text.trim().is_empty(), "{path} is empty");
            for (i, line) in text.lines().enumerate() {
                serde_json::from_str::<serde_json::Value>(line)
                    .unwrap_or_else(|e| panic!("{path}:{}: invalid JSON: {e}", i + 1));
            }
        }
    }
}

#[test]
fn test_fixture_normalization_idempotent_and_strips_volatiles() {
    for scenario in SCENARIOS {
        let path = fixture_path(scenario, "session.jsonl");
        let text = std::fs::read_to_string(&path).unwrap();
        let once = Normalizer::new().normalize_jsonl(&text).unwrap();
        let twice = Normalizer::new().normalize_jsonl(&once).unwrap();
        assert_eq!(once, twice, "{scenario}: normalization not idempotent");
        // After normalization no ISO timestamps, uuids, or tmp paths remain.
        assert!(
            !once.contains("/tmp/rpi-fixture-"),
            "{scenario}: cwd not stripped"
        );
        assert!(
            !once.contains("019fb0ca"),
            "{scenario}: session uuid survived normalization"
        );
    }
}

#[test]
fn test_fixture_header_shape_pinned() {
    // session-format.md §SessionHeader: first line is the v3 header.
    let text = std::fs::read_to_string(fixture_path("single-turn", "session.jsonl")).unwrap();
    let header: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(header["type"], "session");
    assert_eq!(header["version"], 3);
    assert!(header["id"].is_string());
    assert!(header["timestamp"].is_string());
    assert!(header["cwd"].is_string());
}

#[test]
fn test_fixture_abort_persists_aborted_stop_reason() {
    // session-format.md: stopReason "aborted" must persist (never "pending").
    let text = std::fs::read_to_string(fixture_path("abort", "session.jsonl")).unwrap();
    assert!(
        text.contains("\"stopReason\":\"aborted\""),
        "abort fixture must persist stopReason=aborted"
    );
    assert!(!text.contains("\"stopReason\":\"pending\""));
}

#[test]
fn test_fixture_length_truncation_persists_length_stop_reason() {
    let text = std::fs::read_to_string(fixture_path("length-truncation", "session.jsonl")).unwrap();
    assert!(text.contains("\"stopReason\":\"length\""));
}

#[test]
fn test_fixture_self_diff_passes() {
    let text = std::fs::read_to_string(fixture_path("single-turn", "session.jsonl")).unwrap();
    diff_jsonl(&text, &text).unwrap();
}
