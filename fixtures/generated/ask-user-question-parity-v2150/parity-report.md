# ask-user-question parity report (target track: rpiv-mono v2150 @ ce6d353041 — TE28/TE29/TE30/TE31 G3/G12, dual-track TE51)

generated: 2026-11-01T07:29:37.716Z
upstream submodule: external/rpiv-mono (packages/rpiv-ask-user-question)
submodule HEAD: afc21bc25f7d07cae9fb3103dc309240a16b9f49 (track pin ce6d3530410812ca504ec294995581a52170f9e1)
typebox: 1.3.6

## snapshot (sha256 of the driven upstream modules)

- tool/types.ts: 8fb488b02c914383bb2b47aff338c1b309f8bf14af5e4f450bbbfbe09530a6e1
- tool/normalize-params.ts: 40c5ffef66159a56f5384b0a57d3c19541560acb5bd67ee03de44b643b4dd1ce
- tool/validate-questionnaire.ts: 2a63c6204cde2b41d033c58620eb217dc0dbedbc004628c4c65f04791ad7808d
- tool/response-envelope.ts: 36bc85b3c905da14d77e79c92a12e8018ae3281dac2db456bf192db0e53724c4
- tool/format-answer.ts: ac3cfcb8aef95bb916ca68020fc7920c96f12058efbaa615bec63f58d63c2c6e
- state/row-intent.ts: b146d76bb19dec2abf24e5e61a55b294784cd48256a0d8e8c0f33b93e8997a9b
- state/i18n-bridge.ts: d465d3271ce216fc7bff8f4bc7dcded468651da4ae9b6e63de2b3f3c460d5d80
- rpc-fallback.ts: 11074c840bc13e94cb86817fd14b2d946bb33e46f52180ad456c3183fdb1d7f7
- state/state-reducer.ts: 6cccc0654beaca23c919d3dccc9d158cd9da6104996b6a6d9f68c90e5c5c767b
- state/key-router.ts: 703567e30b8d0f4c29a948faf15c15de0bb5509576067ad67a5ba95ba572650f
- view/components/preview/preview-layout-decider.ts: 0c04ccdf5cd9be94138e4e7c9d10895844f32e378a7dfd7bb296ab2738fb7918
- view/components/preview/preview-box-renderer.ts: 20b900ead8a0e71f005aadb18c00103b2dff87391ec8c8011ebd3c47540a8c33
- external/pi/packages/tui/src/keys.ts (pi-tui stub): 40cd0229bbc36cdcac24f3bdd2fce53f060ff4fe6f8522e239135212033d37f2
- external/pi/packages/tui/src/utils.ts (pi-tui stub, TE31): 6e22ed5385c08965a46f01d6ec9686cb0eada95e34b9dc0f9a5e85d28f8f512a

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
- partial_submission_names_unanswered: MATCH
- out_of_order_answers_follow_question_order: MATCH
- multi_answer_with_custom_text: MATCH

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
- confirm_custom_on_multi_becomes_a_multi_answer: MATCH
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
- multi_confirm_carries_custom_text: MATCH
- nav_away_from_multi_input_persists_the_answer: MATCH

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
- multi_select_input_mode_enter_commits_with_text: MATCH
- digit_key_answers_a_single_select_row: MATCH
- digit_key_toggles_a_multi_select_row: MATCH
- digit_key_focuses_the_type_something_row: MATCH
- digit_key_outside_numbered_rows_is_ignored: MATCH
- blank_type_something_enter_is_ignored: MATCH
- custom_notes_key_matches_as_typed: MATCH
- custom_notes_key_spec_matches_ctrl_n: MATCH
- space_csi_u_toggles_in_multi_select: MATCH

## preview

- decide-40x80: MATCH
- decide-40x99: MATCH
- decide-40x100: MATCH
- decide-40x101: MATCH
- decide-40x120: MATCH
- decide-80x80: MATCH
- decide-80x99: MATCH
- decide-80x100: MATCH
- decide-80x101: MATCH
- decide-80x120: MATCH
- decide-99x80: MATCH
- decide-99x99: MATCH
- decide-99x100: MATCH
- decide-99x101: MATCH
- decide-99x120: MATCH
- decide-100x80: MATCH
- decide-100x99: MATCH
- decide-100x100: MATCH
- decide-100x101: MATCH
- decide-100x120: MATCH
- decide-101x80: MATCH
- decide-101x99: MATCH
- decide-101x100: MATCH
- decide-101x101: MATCH
- decide-101x120: MATCH
- decide-120x80: MATCH
- decide-120x99: MATCH
- decide-120x100: MATCH
- decide-120x101: MATCH
- decide-120x120: MATCH
- decide-200x80: MATCH
- decide-200x99: MATCH
- decide-200x100: MATCH
- decide-200x101: MATCH
- decide-200x120: MATCH
- adaptive-short: MATCH
- adaptive-medium-100: MATCH
- adaptive-medium-120: MATCH
- adaptive-long-cap: MATCH
- adaptive-narrow-available: MATCH
- adaptive-empty: MATCH
- cross-tab-max: MATCH
- cross-tab-max-empty: MATCH
- source-width-none: MATCH
- source-width-single-line: MATCH
- source-width-multi-line: MATCH
- source-width-multi: MATCH
- budget-no-previews: MATCH
- budget-wide: MATCH
- budget-capped: MATCH
- budget-multi-tab: MATCH
- donation-basic: MATCH
- donation-narrow-preview: MATCH
- donation-wide-labels: MATCH
- column-widths-typical: MATCH
- column-widths-clamp: MATCH
- column-widths-tiny: MATCH
- body-widths-stacked: MATCH
- body-widths-side: MATCH
- constants: MATCH
- strip-fences: MATCH
- box-basic: MATCH
- box-pad-truncate: MATCH
- box-hidden: MATCH
- box-hidden-narrow: MATCH
- box-min-width: MATCH
- dims-floor: MATCH
- dims-wide-cap: MATCH
- dims-trailing-ws: MATCH
- dims-ansi: MATCH
- dims-empty: MATCH
- box-hidden-wide: MATCH
- box-hidden-two-digit: MATCH
- strip-fences-osc: MATCH

## golden-frames

- baseline: self-baseline (fresh Rust frames vs the committed `golden-frames/*.jsonl`; no upstream renderer exists)
- collapse_reopen-100.jsonl: MATCH (4 frames)
- collapse_reopen-120.jsonl: MATCH (4 frames)
- collapse_reopen-80.jsonl: MATCH (4 frames)
- four_questions-100.jsonl: MATCH (6 frames)
- four_questions-120.jsonl: MATCH (6 frames)
- four_questions-80.jsonl: MATCH (6 frames)
- height_stability-100.jsonl: MATCH (5 frames)
- height_stability-120.jsonl: MATCH (5 frames)
- height_stability-80.jsonl: MATCH (5 frames)
- input_draft-100.jsonl: MATCH (9 frames)
- input_draft-120.jsonl: MATCH (9 frames)
- input_draft-80.jsonl: MATCH (9 frames)
- multi_select-100.jsonl: MATCH (8 frames)
- multi_select-120.jsonl: MATCH (8 frames)
- multi_select-80.jsonl: MATCH (8 frames)
- notes_editor-100.jsonl: MATCH (7 frames)
- notes_editor-120.jsonl: MATCH (7 frames)
- notes_editor-80.jsonl: MATCH (7 frames)
- preview_pane-100.jsonl: MATCH (4 frames)
- preview_pane-120.jsonl: MATCH (4 frames)
- preview_pane-80.jsonl: MATCH (4 frames)
- single_select-100.jsonl: MATCH (5 frames)
- single_select-120.jsonl: MATCH (5 frames)
- single_select-80.jsonl: MATCH (5 frames)
- submit_page-100.jsonl: MATCH (6 frames)
- submit_page-120.jsonl: MATCH (6 frames)
- submit_page-80.jsonl: MATCH (6 frames)

## locales (against the v2150 target pin)

- de.json: MATCH (51989994feb2)
- en.json: MATCH (b4b1a7e811bc)
- es.json: MATCH (f36793c280b6)
- fr.json: MATCH (dce2433b557c)
- pt-BR.json: MATCH (d3eef30df4bd)
- pt.json: MATCH (f305cc52fa60)
- ru.json: MATCH (7167e0576061)
- uk.json: MATCH (3c30045ccb56)
- zh.json: MATCH (7f30c61eac12)


## Attribution summary

- (none)

## RESULT: MATCH
