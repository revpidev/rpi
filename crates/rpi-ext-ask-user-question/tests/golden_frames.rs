//! Golden-frame baseline test (TE30 G3).
//!
//! Re-renders the fixture matrix (single / four questions / multi-select /
//! input draft / Submit × 80/100/120) and compares it byte-for-byte against
//! the committed JSONL in
//! `fixtures/generated/ask-user-question-parity/golden-frames/`.
//!
//! A mismatch means the dialog rendering changed; re-record deliberately with
//!
//! ```text
//! node scripts/ask-user-question-parity/gen-golden-frames.mjs
//! ```
//!
//! and review the diff (Q3 re-records this baseline for the rich-interaction
//! pass).

use std::collections::BTreeSet;

use rpi_ext_ask_user_question::golden;

fn golden_dir() -> String {
    format!(
        "{}/../../fixtures/generated/ask-user-question-parity/golden-frames",
        env!("CARGO_MANIFEST_DIR")
    )
}

#[test]
fn golden_frames_match_committed_baseline() {
    let dir = golden_dir();
    let renders = golden::renders();
    assert_eq!(
        renders.len(),
        15,
        "5 scenarios x 3 widths (single / four / multi / input / submit)"
    );

    let mut expected_files: BTreeSet<String> = BTreeSet::new();
    for render in &renders {
        expected_files.insert(render.file.clone());
        let path = format!("{dir}/{}", render.file);
        let committed = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "read {path}: {error}\nre-record with: node scripts/ask-user-question-parity/gen-golden-frames.mjs"
            )
        });
        let actual = render
            .frames
            .iter()
            .map(|frame| {
                serde_json::to_string(&golden::frame_json(frame)).expect("frame serializes")
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        assert_eq!(
            committed, actual,
            "golden frame mismatch for {}: re-record if intended",
            render.file
        );
    }

    let committed_files: BTreeSet<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("read_dir {dir}: {error}"))
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".jsonl"))
        .collect();
    assert_eq!(
        committed_files, expected_files,
        "committed golden files must match the fixture matrix"
    );
}

#[test]
fn golden_frames_are_stable_across_runs() {
    assert_eq!(golden::renders(), golden::renders());
}
