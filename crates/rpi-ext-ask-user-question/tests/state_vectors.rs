//! State-machine / key-router vector baselines (TE30 G3, §4.1 cases
//! `state_reducer_matches_upstream_vectors` / `key_router_matches_upstream_vectors`).
//!
//! Replays the `state` / `keys` fixture groups from
//! `scripts/ask-user-question-parity/fixtures.json` and compares each case
//! against the committed Rust legs (`rust-state.jsonl` / `rust-keys.jsonl`)
//! that `run-parity.mjs` produced. The upstream diff (the authoritative
//! parity check) runs in the Node harness; this test keeps the same corpus
//! green under plain `cargo test`.
//!
//! Re-record the legs with:
//!
//! ```text
//! node scripts/ask-user-question-parity/run-parity.mjs
//! ```

use std::collections::BTreeMap;

use rpi_ext_ask_user_question::parity::{replay_keys_case, replay_state_case};
use serde_json::Value;

const GENERATED: &str = "../../fixtures/generated/ask-user-question-parity";
const FIXTURES: &str = "../../scripts/ask-user-question-parity/fixtures.json";

fn repo_path(relative: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn load_fixtures() -> Value {
    let path = repo_path(FIXTURES);
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_str(&raw).expect("fixtures.json parses")
}

/// Committed Rust leg lines keyed by case name.
fn load_baseline(file: &str) -> BTreeMap<String, Value> {
    let path = repo_path(&format!("{GENERATED}/{file}"));
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "read {}: {error}\nre-record with: node scripts/ask-user-question-parity/run-parity.mjs",
            path.display()
        )
    });
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let entry: Value = serde_json::from_str(line).expect("baseline line parses");
            let name = entry
                .get("name")
                .and_then(Value::as_str)
                .expect("baseline name")
                .to_owned();
            (name, entry.get("output").cloned().unwrap_or(Value::Null))
        })
        .collect()
}

fn cases(fixtures: &Value, group: &str) -> Vec<(String, Value)> {
    fixtures
        .get(group)
        .and_then(|group| group.get("cases"))
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("fixtures group {group}"))
        .iter()
        .map(|case| {
            (
                case.get("name")
                    .and_then(Value::as_str)
                    .expect("case name")
                    .to_owned(),
                case.get("input").cloned().unwrap_or(Value::Null),
            )
        })
        .collect()
}

#[test]
fn state_vectors_match_the_committed_rust_leg() {
    let fixtures = load_fixtures();
    let baseline = load_baseline("rust-state.jsonl");
    let cases = cases(&fixtures, "state");
    assert_eq!(cases.len(), baseline.len(), "case count drift");
    for (name, input) in cases {
        let expected = baseline
            .get(&name)
            .unwrap_or_else(|| panic!("baseline missing {name}"));
        let actual = replay_state_case(&input);
        assert_eq!(&actual, expected, "state vector drift for {name}");
    }
}

#[test]
fn key_router_vectors_match_the_committed_rust_leg() {
    let fixtures = load_fixtures();
    let baseline = load_baseline("rust-keys.jsonl");
    let cases = cases(&fixtures, "keys");
    let matrix = fixtures
        .get("keys")
        .and_then(|group| group.get("keyMatrix"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert_eq!(cases.len(), baseline.len(), "case count drift");
    for (name, input) in cases {
        let expected = baseline
            .get(&name)
            .unwrap_or_else(|| panic!("baseline missing {name}"));
        let actual = replay_keys_case(&input, &matrix);
        assert_eq!(&actual, expected, "key router drift for {name}");
    }
}

#[test]
fn key_matrix_covers_the_documented_bindings() {
    let fixtures = load_fixtures();
    let matrix: Vec<&str> = fixtures["keys"]["keyMatrix"]
        .as_array()
        .expect("keyMatrix")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    // Enter / space / notes / escape / arrows (CSI + SS3) / tab family /
    // ctrl+] / newline / ctrl+u / ctrl+g / plain text / kitty sequences.
    for expected in [
        "\r",
        " ",
        "n",
        "\u{1b}",
        "\u{1b}[A",
        "\u{1b}[B",
        "\u{1b}OA",
        "\u{1b}OB",
        "\t",
        "\u{1b}[Z",
        "\u{1b}[C",
        "\u{1b}[D",
        "\u{1d}",
        "\n",
        "\u{15}",
        "\u{7}",
        "x",
        "\u{1b}[13;2u",
        "\u{1b}[1;5A",
    ] {
        assert!(matrix.contains(&expected), "missing key {expected:?}");
    }
}
