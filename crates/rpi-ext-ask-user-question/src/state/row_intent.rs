//! Row-kind metadata and sentinel derivation.
//!
//! Port of upstream `packages/rpiv-ask-user-question/state/row-intent.ts` @
//! `338b264c`. Single source of truth for the runtime sentinel rows: the
//! auto-append walker, the reserved-label derivation and the i18n label
//! lookup all read [`ROW_INTENT_META`].
//!
//! Behavior-bearing branches (answer construction in the key router, the Next
//! row render block, the inline-input render branch) land with Q2/Q3 and keep
//! their exhaustive matches, reading flags from this table.

use serde_json::{json, Value};

use crate::tool::types::{QuestionData, RESERVED_LABELS};

/// Row kind discriminator (upstream `WrappingSelectItem["kind"]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RowKind {
    /// Author-defined option row (per-instance label/description).
    Option,
    /// `Type something.` inline-input sentinel.
    Other,
    /// `Next` multi-select submit sentinel.
    Next,
}

impl RowKind {
    /// Wire discriminator string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Option => "option",
            Self::Other => "other",
            Self::Next => "next",
        }
    }
}

/// Sentinel kinds — the subset of [`RowKind`] representing protocol-driven
/// rows (vs author-defined `option` rows), in declaration order.
pub const SENTINEL_KINDS: [RowKind; 2] = [RowKind::Other, RowKind::Next];

/// Per-kind static metadata (upstream `RowIntentMeta`). Pure data — no
/// closures, no per-kind handler functions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowIntentMeta {
    /// User-facing label (`option` is an empty placeholder — per-instance).
    pub label: &'static str,
    /// Author-facing labels matching this string trigger `reserved_label`.
    pub reserved: bool,
    /// `true` iff the row appears in `itemsByTab[i]`.
    pub lives_in_main_list: bool,
    /// `true` iff the row contributes to the main-list numbering.
    pub numbered: bool,
    /// `true` iff focusing the row toggles `state.inputMode = true`.
    pub activates_input_mode: bool,
    /// Multi-select: suppress Space (and Enter-as-toggle) on this row.
    pub blocks_multi_toggle: bool,
    /// Multi-select: Enter on this row commits the question.
    pub auto_submits_in_multi: bool,
    /// `buildItemsForQuestion` appends this row on single-select.
    pub auto_append_on_single_select: bool,
    /// `buildItemsForQuestion` appends this row on multi-select.
    pub auto_append_on_multi_select: bool,
}

/// Per-kind metadata table (upstream `ROW_INTENT_META`).
pub const ROW_INTENT_META: [RowIntentMeta; 3] = [
    RowIntentMeta {
        label: "",
        reserved: false,
        lives_in_main_list: true,
        numbered: true,
        activates_input_mode: false,
        blocks_multi_toggle: false,
        auto_submits_in_multi: false,
        auto_append_on_single_select: false,
        auto_append_on_multi_select: false,
    },
    RowIntentMeta {
        label: crate::tool::types::SENTINEL_OTHER_LABEL,
        reserved: true,
        lives_in_main_list: true,
        numbered: true,
        activates_input_mode: true,
        blocks_multi_toggle: false,
        auto_submits_in_multi: false,
        auto_append_on_single_select: true,
        auto_append_on_multi_select: true,
    },
    RowIntentMeta {
        label: crate::tool::types::SENTINEL_NEXT_LABEL,
        reserved: true,
        lives_in_main_list: true,
        numbered: false,
        activates_input_mode: false,
        blocks_multi_toggle: true,
        auto_submits_in_multi: true,
        auto_append_on_single_select: false,
        auto_append_on_multi_select: true,
    },
];

/// Metadata for one kind.
pub fn meta(kind: RowKind) -> &'static RowIntentMeta {
    match kind {
        RowKind::Option => &ROW_INTENT_META[0],
        RowKind::Other => &ROW_INTENT_META[1],
        RowKind::Next => &ROW_INTENT_META[2],
    }
}

impl RowIntentMeta {
    /// Wire JSON for this entry (camelCase field names, upstream
    /// `ROW_INTENT_META` shape).
    pub fn to_json(&self) -> Value {
        json!({
            "label": self.label,
            "reserved": self.reserved,
            "livesInMainList": self.lives_in_main_list,
            "numbered": self.numbered,
            "activatesInputMode": self.activates_input_mode,
            "blocksMultiToggle": self.blocks_multi_toggle,
            "autoSubmitsInMulti": self.auto_submits_in_multi,
            "autoAppendOnSingleSelect": self.auto_append_on_single_select,
            "autoAppendOnMultiSelect": self.auto_append_on_multi_select,
        })
    }
}

/// Kind-keyed label view as wire JSON (upstream `LABELS_BY_KIND`).
pub fn labels_by_kind_json() -> Value {
    json!({
        "other": crate::tool::types::SENTINEL_OTHER_LABEL,
        "next": crate::tool::types::SENTINEL_NEXT_LABEL,
    })
}

/// Kind-keyed label view (upstream `LABELS_BY_KIND`); `option` is excluded.
pub fn label_by_kind(kind: RowKind) -> Option<&'static str> {
    match kind {
        RowKind::Option => None,
        RowKind::Other | RowKind::Next => Some(meta(kind).label),
    }
}

/// Reserved-label set for runtime validation: `"Other"` (a model-conditioned
/// label that has no runtime kind) plus every sentinel with `reserved: true`.
pub fn reserved_label_set() -> Vec<&'static str> {
    let mut labels = vec![RESERVED_LABELS[0]];
    labels.extend(
        SENTINEL_KINDS
            .iter()
            .filter(|kind| meta(**kind).reserved)
            .map(|kind| meta(*kind).label),
    );
    labels
}

/// Whether `label` is reserved (upstream `RESERVED_LABEL_SET` membership).
pub fn is_reserved_label(label: &str) -> bool {
    reserved_label_set().contains(&label)
}

/// Walk the META table to synthesize sentinel rows for one question
/// (`sentinelsToAppend`). Returns kinds in declaration order of
/// [`SENTINEL_KINDS`].
pub fn sentinels_to_append(question: &QuestionData) -> Vec<RowKind> {
    let mut out = Vec::new();
    for kind in SENTINEL_KINDS {
        let meta = meta(kind);
        if !meta.lives_in_main_list {
            continue;
        }
        if question.multi_select == Some(true) {
            if meta.auto_append_on_multi_select {
                out.push(kind);
            }
        } else if meta.auto_append_on_single_select {
            out.push(kind);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::types::{OptionData, QuestionData};

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
                    preview: None,
                },
            ],
            multi_select,
        }
    }

    #[test]
    fn row_intent_matches_upstream_row_intent() {
        assert_eq!(reserved_label_set(), ["Other", "Type something.", "Next"]);
        assert!(is_reserved_label("Other"));
        assert!(is_reserved_label("Type something."));
        assert!(is_reserved_label("Next"));
        assert!(!is_reserved_label("other"));
        assert!(!is_reserved_label("Next!"));

        assert_eq!(label_by_kind(RowKind::Other), Some("Type something."));
        assert_eq!(label_by_kind(RowKind::Next), Some("Next"));
        assert_eq!(label_by_kind(RowKind::Option), None);
        assert_eq!(RowKind::Option.as_str(), "option");
        assert_eq!(RowKind::Other.as_str(), "other");
        assert_eq!(RowKind::Next.as_str(), "next");
    }

    #[test]
    fn row_intent_sentinels_to_append_matrix() {
        // Single-select: only the `other` row.
        assert_eq!(
            sentinels_to_append(&question(None)),
            vec![RowKind::Other],
            "absent multiSelect == single-select"
        );
        assert_eq!(
            sentinels_to_append(&question(Some(false))),
            vec![RowKind::Other]
        );
        // Multi-select: `other` then `next`.
        assert_eq!(
            sentinels_to_append(&question(Some(true))),
            vec![RowKind::Other, RowKind::Next]
        );
    }

    #[test]
    fn row_intent_meta_flags_match_upstream_table() {
        let option = meta(RowKind::Option);
        assert_eq!(option.label, "");
        assert!(!option.reserved);
        assert!(option.lives_in_main_list);
        assert!(option.numbered);
        assert!(!option.activates_input_mode);
        assert!(!option.blocks_multi_toggle);
        assert!(!option.auto_submits_in_multi);
        assert!(!option.auto_append_on_single_select);
        assert!(!option.auto_append_on_multi_select);

        let other = meta(RowKind::Other);
        assert_eq!(other.label, "Type something.");
        assert!(other.reserved);
        assert!(other.lives_in_main_list);
        assert!(other.numbered);
        assert!(other.activates_input_mode);
        assert!(!other.blocks_multi_toggle);
        assert!(!other.auto_submits_in_multi);
        assert!(other.auto_append_on_single_select);
        assert!(other.auto_append_on_multi_select);

        let next = meta(RowKind::Next);
        assert_eq!(next.label, "Next");
        assert!(next.reserved);
        assert!(next.lives_in_main_list);
        assert!(!next.numbered);
        assert!(!next.activates_input_mode);
        assert!(next.blocks_multi_toggle);
        assert!(next.auto_submits_in_multi);
        assert!(!next.auto_append_on_single_select);
        assert!(next.auto_append_on_multi_select);
    }
}
