# ask-user-question parity report (TE28/TE29/TE30 G3/G12)

generated: 2026-09-10T14:03:42.115Z
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
- state/state-reducer.ts: 8ed9bdf4596c884fdf36d4411f6c1852ca236d5d9f161c1252961fed5dc18145
- state/key-router.ts: c77a98c1435e1c3bcb0a658079ccd1e4e54968f2bb7631f7057ed2be05b4ee6e
- external/pi/packages/tui/src/keys.ts (pi-tui stub): b972facce4233a4623239fc38029e28cae15d0fb326558c0c09dc02cf4345fa7

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

## state

- nav_regular_keeps_the_active_draft_buffer_intact: MATCH
- nav_onto_other_row_with_prior_custom_answer_restores_the_buffer: MATCH
- nav_onto_other_row_with_no_draft_resets_the_buffer: MATCH
- nav_back_onto_other_restores_the_in_flight_draft_ahead_of_a_confirmed_answer: MATCH
- an_explicitly_cleared_draft_does_not_resurrect_a_confirmed_custom_answer: MATCH
- snapshots_the_live_input_value_when_navigation_leaves_the_custom_row: MATCH
- tab_switch_emits_notes_focused_and_value_and_resets_transients: MATCH
- tab_switch_rehydrates_the_target_questions_draft: MATCH
- tab_switch_syncs_multi_checked_from_answers: MATCH
- confirm_regular_option_emits_done_with_the_answer: MATCH
- confirm_makes_the_confirmed_custom_answer_authoritative_by_removing_its_draft: MATCH
- confirm_regular_option_matching_a_preview_bearing_option_augments_answer_preview: MATCH
- confirm_merges_pending_notes_from_notes_by_tab: MATCH
- confirm_custom_on_multi_clears_the_checked_set: MATCH
- confirm_with_auto_advance_switches_tab_instead_of_done: MATCH
- toggle_persists_the_multi_answer: MATCH
- toggle_an_empty_selection_deletes_the_answer: MATCH
- toggle_on_a_single_select_question_keeps_answers_untouched: MATCH
- toggle_keeps_pending_notes_in_the_multi_answer: MATCH
- multi_confirm_commits_the_selection: MATCH
- multi_confirm_accepts_an_empty_selection: MATCH
- multi_confirm_with_auto_advance_switches_tab: MATCH
- multi_confirm_ignores_a_missing_question: MATCH
- input_clear_clears_the_draft: MATCH
- input_edit_emits_open_input_editor: MATCH
- input_replace_sets_the_draft_and_buffer: MATCH
- notes_enter_seeds_from_the_answer_mirror: MATCH
- notes_exit_trims_and_merges_the_draft: MATCH
- notes_exit_on_empty_text_removes_notes_and_strips_the_answer: MATCH
- notes_forward_emits_a_forward_keystroke: MATCH
- submit_lifts_the_global_note: MATCH
- cancel_reports_partial_answers: MATCH
- submit_nav_moves_the_picker_choice: MATCH
- toggle_collapsed_emits_overlay_hidden_and_back: MATCH
- ignore_is_a_noop: MATCH

## keys

- single_select_question: MATCH
- single_select_other_row_input: MATCH
- input_mode_multiline_cursor_middle: MATCH
- input_mode_multiline_cursor_top: MATCH
- multi_select_question_option: MATCH
- multi_select_question_other: MATCH
- multi_select_question_next: MATCH
- two_questions_first_tab: MATCH
- two_questions_second_tab: MATCH
- submit_tab_submit_choice: MATCH
- submit_tab_cancel_choice: MATCH
- notes_editor_open: MATCH
- collapsed_state: MATCH
- collapse_key_off: MATCH
- answered_option_confirmed: MATCH
- empty_questions: MATCH

## golden-frames

- four_questions-100.jsonl: MATCH (6 frames)
- four_questions-120.jsonl: MATCH (6 frames)
- four_questions-80.jsonl: MATCH (6 frames)
- input_draft-100.jsonl: MATCH (9 frames)
- input_draft-120.jsonl: MATCH (9 frames)
- input_draft-80.jsonl: MATCH (9 frames)
- multi_select-100.jsonl: MATCH (8 frames)
- multi_select-120.jsonl: MATCH (8 frames)
- multi_select-80.jsonl: MATCH (8 frames)
- single_select-100.jsonl: MATCH (5 frames)
- single_select-120.jsonl: MATCH (5 frames)
- single_select-80.jsonl: MATCH (5 frames)
- submit_page-100.jsonl: MATCH (6 frames)
- submit_page-120.jsonl: MATCH (6 frames)
- submit_page-80.jsonl: MATCH (6 frames)

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
