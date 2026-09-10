//! Golden-frame fixtures for the questionnaire dialog (TE30 G3).
//!
//! Deterministic frame sequences over the Q2 state matrix — single question,
//! four questions, multi-select, inline-input draft, Submit page — at widths
//! 80/100/120. The generator (`scripts/ask-user-question-parity/
//! gen-golden-frames.mjs` → `parity_runner golden`) writes one JSONL file per
//! scenario/width into `fixtures/generated/ask-user-question-parity/
//! golden-frames/`; the Rust integration test and `run-parity.mjs` re-render
//! and compare byte-for-byte, so any visual change must be re-recorded
//! deliberately (the Q3 rich-interaction pass re-records this baseline).

use rpi_ext_host::interactive_ui::Component;
use serde_json::{json, Map, Value};

use crate::i18n::I18n;
use crate::state::session::QuestionnaireComponent;
use crate::tool::types::{OptionData, QuestionData, QuestionParams};

/// One rendered frame in the golden JSONL (`{lines, cursor?, done?}`).
#[derive(Clone, Debug, PartialEq)]
pub struct GoldenFrame {
    /// Frame lines.
    pub lines: Vec<String>,
    /// `(row, col)` cursor, omitted when no editor owns focus.
    pub cursor: Option<(usize, usize)>,
    /// Component result, present on the terminating frame only.
    pub done: Option<Value>,
}

/// One scenario/width golden file.
#[derive(Clone, Debug, PartialEq)]
pub struct GoldenRender {
    /// File name (e.g. `single_select-80.jsonl`).
    pub file: String,
    /// Frames in event order (initial frame first).
    pub frames: Vec<GoldenFrame>,
}

/// Serialize one frame to its golden JSON shape.
pub fn frame_json(frame: &GoldenFrame) -> Value {
    let mut map = Map::new();
    map.insert("lines".to_owned(), json!(frame.lines));
    if let Some((row, col)) = frame.cursor {
        map.insert("cursor".to_owned(), json!({ "row": row, "col": col }));
    }
    if let Some(done) = &frame.done {
        map.insert("done".to_owned(), done.clone());
    }
    Value::Object(map)
}

fn option(label: &str, description: &str) -> OptionData {
    OptionData {
        label: label.to_owned(),
        description: description.to_owned(),
        preview: None,
    }
}

fn single_question() -> QuestionData {
    QuestionData {
        question: "Which library should we use for date formatting?".to_owned(),
        header: "Library".to_owned(),
        options: vec![
            option("date-fns", "Tree-shakeable, functional API"),
            option("Day.js", "Tiny Moment.js-compatible API"),
            option("Luxon", "Intl-based, immutable dates"),
        ],
        multi_select: None,
    }
}

fn second_question() -> QuestionData {
    QuestionData {
        question: "Where should the helper live?".to_owned(),
        header: "Location".to_owned(),
        options: vec![
            option("src/utils", "Close to existing helpers"),
            option("src/lib", "Shared library boundary"),
        ],
        multi_select: None,
    }
}

fn four_questions() -> Vec<QuestionData> {
    vec![
        single_question(),
        second_question(),
        QuestionData {
            question: "Which test level should cover it?".to_owned(),
            header: "Tests".to_owned(),
            options: vec![
                option("Unit", "Fast, narrow"),
                option("Integration", "End-to-end wiring"),
            ],
            multi_select: None,
        },
        QuestionData {
            question: "Should we document the change?".to_owned(),
            header: "Docs".to_owned(),
            options: vec![
                option("Yes", "Add a changelog entry"),
                option("No", "Internal-only refactor"),
            ],
            multi_select: None,
        },
    ]
}

fn multi_select_question() -> QuestionData {
    QuestionData {
        question: "Which features do you want to enable?".to_owned(),
        header: "Features".to_owned(),
        options: vec![
            option("Caching", "Memoize repeated lookups"),
            option("Metrics", "Emit usage counters"),
            option("Tracing", "Structured span events"),
        ],
        multi_select: Some(true),
    }
}

/// Scenario name, questions, and the key script (each event yields one frame).
fn scenarios() -> Vec<(&'static str, Vec<QuestionData>, Vec<&'static str>)> {
    vec![
        (
            "single_select",
            vec![single_question()],
            vec![
                "", // initial frame marker (no event)
                "\x1b[B", "\x1b[B", "\x1b[B", // onto the Type-something row
                "\x1b[A", // back up, draft stays empty
            ],
        ),
        (
            "four_questions",
            four_questions(),
            vec![
                "", "\r",     // answer Q1 (auto-advance to Q2)
                "\t",     // tab to Q3
                "\t",     // tab to Q4
                "\t",     // tab to Submit
                "\x1b[B", // picker -> Cancel
            ],
        ),
        (
            "multi_select",
            vec![multi_select_question()],
            vec![
                "", " ",      // toggle Caching
                "\x1b[B", // down to Metrics
                " ",      // toggle Metrics
                "\x1b[B", "\x1b[B", "\x1b[B", // onto the Next sentinel
                "\r",     // Next: commit the checked set
            ],
        ),
        (
            "input_draft",
            vec![single_question()],
            vec![
                "", "\x1b[B", "\x1b[B", "\x1b[B", "own", " ", "words", "\n", "line two",
            ],
        ),
        (
            "submit_page",
            four_questions(),
            vec![
                "", "\r", // answer Q1
                "\t", "\t", "\t", // Submit tab (Q1 answered, Q2-Q4 open)
                "\x1b[B",
            ],
        ),
    ]
}

/// Golden widths (R-Q5.3/F G3 matrix).
pub const WIDTHS: [usize; 3] = [80, 100, 120];

/// Render every scenario at every width.
pub fn renders() -> Vec<GoldenRender> {
    let mut out = Vec::new();
    for (name, questions, events) in scenarios() {
        for width in WIDTHS {
            let params = QuestionParams {
                questions: questions.clone(),
            };
            let mut component =
                QuestionnaireComponent::new(&params, I18n::for_locale("en"), "ctrl+]".to_owned());
            let mut frames = Vec::new();
            for event in &events {
                if !event.is_empty() {
                    component.dispatch(event);
                }
                let lines = component.render_frame(width);
                let cursor = component.cursor();
                frames.push(GoldenFrame {
                    lines,
                    cursor: cursor.map(|cursor| (cursor.row, cursor.col)),
                    done: component.done(),
                });
            }
            out.push(GoldenRender {
                file: format!("{name}-{width}.jsonl"),
                frames,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_tui::utils::visible_width;

    #[test]
    fn scenarios_produce_one_file_per_name_and_width() {
        let renders = renders();
        assert_eq!(renders.len(), scenarios().len() * WIDTHS.len());
        for render in &renders {
            assert!(render.file.ends_with(".jsonl"), "{}", render.file);
            assert_eq!(
                render.frames.len(),
                scenarios()
                    .iter()
                    .find(|(name, _, _)| render.file.starts_with(name))
                    .map(|(_, _, events)| events.len())
                    .expect("scenario")
            );
        }
    }

    #[test]
    fn every_frame_line_fits_its_width_and_braces_are_balanced() {
        for render in renders() {
            let width: usize = render
                .file
                .trim_end_matches(".jsonl")
                .rsplit('-')
                .next()
                .and_then(|value| value.parse().ok())
                .expect("width");
            for frame in &render.frames {
                assert!(!frame.lines.is_empty(), "{}", render.file);
                for line in &frame.lines {
                    assert!(
                        visible_width(line) <= width,
                        "{} width {width}: {line}",
                        render.file
                    );
                }
            }
        }
    }

    #[test]
    fn terminating_frames_carry_done_and_cursor_only_in_input_mode() {
        let renders = renders();
        let multi = renders
            .iter()
            .find(|render| render.file == "multi_select-80.jsonl")
            .expect("multi_select-80");
        let last = multi.frames.last().expect("frames");
        assert!(last.done.is_some(), "commit frame carries done");
        assert!(
            last.cursor.is_none(),
            "commit from the Next row has no editor cursor"
        );
        assert!(
            multi.frames.iter().any(|frame| frame.cursor.is_some()),
            "navigating onto the inline-input row reports a cursor"
        );
        let done = last.done.as_ref().expect("done");
        assert_eq!(done["answers"][0]["kind"], "multi");
        assert_eq!(
            done["answers"][0]["selected"],
            json!(["Caching", "Metrics"])
        );

        let input = renders
            .iter()
            .find(|render| render.file == "input_draft-80.jsonl")
            .expect("input_draft-80");
        assert!(
            input
                .frames
                .iter()
                .skip(3)
                .any(|frame| frame.cursor.is_some()),
            "typing in the inline input reports a cursor"
        );

        // Frames are deterministic across runs.
        assert_eq!(renders, super::renders());
    }

    #[test]
    fn component_trait_render_matches_the_fixture_loop() {
        let params = QuestionParams {
            questions: vec![single_question()],
        };
        let mut component =
            QuestionnaireComponent::new(&params, I18n::for_locale("en"), "ctrl+]".to_owned());
        let trait_lines = Component::render(&mut component, 80);
        let mut fixture =
            QuestionnaireComponent::new(&params, I18n::for_locale("en"), "ctrl+]".to_owned());
        let fixture_lines = fixture.render_frame(80);
        assert_eq!(trait_lines, fixture_lines);
    }
}
