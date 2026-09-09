# ask-user-question parity report (TE28 G3/G12)

generated: 2026-09-09T01:36:39.148Z
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
