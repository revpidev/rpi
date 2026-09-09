# ask-user-question parity report (TE28/TE29 G3/G12)

generated: 2026-09-09T14:36:06.214Z
upstream submodule: external/rpiv-mono/packages/rpiv-ask-user-question
submodule HEAD: 338b264c1ca4fd8828cc849b632f4f7ad88d2e78 (pinned 338b264c1ca4fd8828cc849b632f4f7ad88d2e78)
typebox: 1.3.6

## snapshot (sha256 of the driven upstream modules)

- tool/types.ts: f6e4ca589d8c8f11726c6d93313fe8907752140915f62aea2055cca691fc49e6
- tool/normalize-params.ts: 660b75bff74b97aa49767c31b201dec527bed60c8602751e0e5560038643c22f
- tool/validate-questionnaire.ts: 5baf8e418756ce8420909c2ad5bea5f4cc6fb4ac315f625c85e26f6ae99b8edd
- tool/response-envelope.ts: 299971663030c4f7eee5798d698f41679ecb27664738006ceabd27a83ac9b473
- tool/format-answer.ts: aa382c244d9230c08dbf1788c2510f0dc07cc347550ee50d7407ec26bb919b06
- state/row-intent.ts: dbb44959cffb56fd031ea751d28f7c5647ef117051ec6edc5a96bb877f64ca16
- state/i18n-bridge.ts: c2921835574265924e0a96a1d10b010d6edd78b07b7ee14e4a4b92cc2287a809
- rpc-fallback.ts: 14997c0b37294a466665b9661529522112de56891bf380dae81346d299396a29

## schema

- schema_and_constants: MATCH

## normalize

- noop: MATCH
- crlf_question_and_fields: MATCH
- lone_cr_deleted: MATCH
- mixed_terminators: MATCH
- astral_emoji: MATCH
- preview_absent_stays_absent: MATCH
- empty_preview_string_stays_present: MATCH
- multi_select_preserved: MATCH
- multi_select_false_preserved: MATCH

## validate

- ok_min_one_question: MATCH
- ok_max_four_questions: MATCH
- ok_four_options: MATCH
- no_questions: MATCH
- too_many_questions: MATCH
- duplicate_question: MATCH
- empty_options: MATCH
- reserved_other: MATCH
- reserved_type_something: MATCH
- reserved_next: MATCH
- reserved_precedes_duplicate_label: MATCH
- duplicate_option_label: MATCH
- reserved_case_sensitive: MATCH
- duplicate_label_across_questions_ok: MATCH
- duplicate_question_precedes_empty_options: MATCH

## envelope

- answered_option: MATCH
- answered_custom: MATCH
- answered_custom_empty_placeholder: MATCH
- answered_custom_null_placeholder: MATCH
- answered_option_empty_string_kept: MATCH
- answered_option_null_placeholder: MATCH
- answered_multi: MATCH
- answered_multi_empty_placeholder: MATCH
- answered_with_notes: MATCH
- answered_with_preview: MATCH
- answered_with_preview_and_notes: MATCH
- empty_preview_and_notes_no_suffix: MATCH
- global_note_only: MATCH
- global_note_with_answer: MATCH
- cancelled: MATCH
- cancelled_with_global_note: MATCH
- null_result: MATCH
- empty_answers_not_cancelled_declines: MATCH
- partial_submission_skips_unanswered: MATCH
- out_of_order_answers_follow_question_order: MATCH

## row-intent

- single_select_absent: MATCH
- single_select_false: MATCH
- multi_select_true: MATCH
- constants: MATCH

## rpc

- has_dialog_ui_both_primitives: MATCH
- has_dialog_ui_only_select: MATCH
- has_dialog_ui_only_input: MATCH
- has_dialog_ui_neither: MATCH
- has_dialog_ui_undefined: MATCH
- single_select_option_answer: MATCH
- single_select_second_option: MATCH
- single_select_custom_answer: MATCH
- single_select_custom_answer_cancel: MATCH
- single_select_no_header_no_prefix: MATCH
- single_select_cancel_dismisses: MATCH
- single_select_non_numeric_reply_is_cancel: MATCH
- single_select_out_of_range_reply_is_cancel: MATCH
- single_select_preview_folded_and_truncated: MATCH
- single_select_preview_astral_boundary: MATCH
- multi_select_indices_dedup: MATCH
- multi_select_space_separated: MATCH
- multi_select_period_suffixed: MATCH
- multi_select_empty_commit: MATCH
- multi_select_non_index_custom: MATCH
- multi_select_out_of_range_number_custom: MATCH
- multi_select_cancel_dismisses: MATCH
- multi_question_sequential_walk: MATCH
- mid_walk_cancel_preserves_answers: MATCH

## locales

- de.json: MATCH (d6d4491504bd)
- en.json: MATCH (49d142c22c26)
- es.json: MATCH (31312a491a58)
- fr.json: MATCH (d6dfc6a162bf)
- pt-BR.json: MATCH (f9c79ae849eb)
- pt.json: MATCH (69687a3d97ec)
- ru.json: MATCH (9702bc2b55f6)
- uk.json: MATCH (81c3c2762bc8)
- zh.json: MATCH (b07a54cbad95)


## RESULT: MATCH
