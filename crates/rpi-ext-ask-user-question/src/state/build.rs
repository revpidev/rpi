//! Per-question row building + sentinel append.
//!
//! Port of upstream `buildItemsForQuestion`
//! (`packages/rpiv-ask-user-question/ask-user-question.ts:...`) together with
//! the `sentinelsToAppend` walker from `state/row-intent.ts` @ `338b264c`.
//! Q0 lands the pure data surface only (row descriptors); rendering consumes
//! it in Q2/Q3.

use serde::{Deserialize, Serialize};

use crate::i18n::I18n;
use crate::state::row_intent::{sentinels_to_append, RowKind};
use crate::tool::types::QuestionData;

/// One row in a question's item list (upstream `WrappingSelectItem`).
///
/// `option` rows carry the author's `label` + `description`; sentinel rows
/// carry only the locale-aware label (`description` stays absent).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionItem {
    /// Row kind discriminator.
    pub kind: RowKind,
    /// User-facing label (per-instance for options, locale-aware for sentinels).
    pub label: String,
    /// One-line description (options only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Build the row list for one question: author options in order, then the
/// sentinel rows the walker appends for this question's mode
/// (`buildItemsForQuestion`).
pub fn build_items_for_question(question: &QuestionData, i18n: &I18n) -> Vec<QuestionItem> {
    let mut items: Vec<QuestionItem> = question
        .options
        .iter()
        .map(|option| QuestionItem {
            kind: RowKind::Option,
            label: option.label.clone(),
            description: Some(option.description.clone()),
        })
        .collect();
    for kind in sentinels_to_append(question) {
        items.push(QuestionItem {
            kind,
            label: i18n.display_label(kind),
            description: None,
        });
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::types::OptionData;
    use serde_json::json;

    fn question(multi_select: Option<bool>) -> QuestionData {
        QuestionData {
            question: "Q?".to_owned(),
            header: "H".to_owned(),
            options: vec![
                OptionData {
                    label: "A".to_owned(),
                    description: "a".to_owned(),
                    preview: None,
                },
                OptionData {
                    label: "B".to_owned(),
                    description: "b".to_owned(),
                    preview: Some("preview".to_owned()),
                },
            ],
            multi_select,
        }
    }

    #[test]
    fn build_items_single_select_appends_other_row_only() {
        let items = build_items_for_question(&question(None), &I18n::for_locale("en"));
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].kind, RowKind::Option);
        assert_eq!(items[0].label, "A");
        assert_eq!(items[0].description.as_deref(), Some("a"));
        assert_eq!(items[1].kind, RowKind::Option);
        assert_eq!(items[1].label, "B");
        assert_eq!(items[2].kind, RowKind::Other);
        assert_eq!(items[2].label, "Type something.");
        assert_eq!(items[2].description, None);
    }

    #[test]
    fn build_items_multi_select_appends_other_then_next() {
        let items = build_items_for_question(&question(Some(true)), &I18n::for_locale("en"));
        assert_eq!(items.len(), 4);
        assert_eq!(items[2].kind, RowKind::Other);
        assert_eq!(items[3].kind, RowKind::Next);
        assert_eq!(items[3].label, "Next");
    }

    #[test]
    fn build_items_localizes_sentinel_labels_only() {
        let items = build_items_for_question(&question(None), &I18n::for_locale("zh"));
        assert_eq!(items[0].label, "A", "author labels never localize");
        assert_eq!(items[2].label, "输入内容");
    }

    #[test]
    fn build_items_serialize_option_rows_without_description_for_sentinels() {
        let items = build_items_for_question(&question(None), &I18n::for_locale("en"));
        let value = serde_json::to_value(&items).expect("serialize");
        assert_eq!(
            value,
            json!([
                {"kind": "option", "label": "A", "description": "a"},
                {"kind": "option", "label": "B", "description": "b"},
                {"kind": "other", "label": "Type something."}
            ])
        );
    }
}
