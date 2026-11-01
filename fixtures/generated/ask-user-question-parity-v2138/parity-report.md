# ask-user-question parity report (regression track: rpiv-mono v2138 @ afc21bc25f — TE28/TE29/TE30/TE31 G3/G12, dual-track TE51)

generated: 2026-11-01T06:02:08.294Z
upstream submodule: external/rpiv-mono (packages/rpiv-ask-user-question)
submodule HEAD: afc21bc25f7d07cae9fb3103dc309240a16b9f49 (track pin afc21bc25f7d07cae9fb3103dc309240a16b9f49)
typebox: 1.3.6

## snapshot (sha256 of the driven upstream modules)

- tool/types.ts: d41fe638fdc2af67038e5a514ff3896628772620a5d07a513de9d876e45e7c8e
- tool/normalize-params.ts: 97adf84ebd4a82eac70c5eff3a4905054029ec56ce6e1dfe654c9b8abee1ade5
- tool/validate-questionnaire.ts: 5baf8e418756ce8420909c2ad5bea5f4cc6fb4ac315f625c85e26f6ae99b8edd
- tool/response-envelope.ts: 406335512b5be540d9854bc0193ed4900f1ad316881cda38cb736bf46ef97d87
- tool/format-answer.ts: d4504470ccdbdad60127a8f49a09d21cfd8b8a788f00ac250eb07586d889b41a
- state/row-intent.ts: dbb44959cffb56fd031ea751d28f7c5647ef117051ec6edc5a96bb877f64ca16
- state/i18n-bridge.ts: c2921835574265924e0a96a1d10b010d6edd78b07b7ee14e4a4b92cc2287a809
- rpc-fallback.ts: 6df56c7d890d8ba9fa841e33c77839b25875edfeb1c8b19a7f9e86c0d59d63b6
- state/state-reducer.ts: f6045ea83dae832176db0dab5bbb52f424577011aedbbd73bde50f5ae434d3fe
- state/key-router.ts: 9f4ad8bf7ead9afb50fa09b1143c5864a2da7f4cacb9f2ceef733eaad3216782
- view/components/preview/preview-layout-decider.ts: 7baf9ecd1db8af8383d2f792d69d4fd9b22623a7cbd29e1a95bc9fd480796a3b
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
- tab_switch_syncs_multi_checked_from_answers: ATTRIBUTED [upstream-semantics] R19.1 / #281 → TE51 FR-G
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
- digit_key_answers_a_single_select_row: ATTRIBUTED [upstream-semantics] R19.1 / #163 → TE51 FR-G
- digit_key_toggles_a_multi_select_row: ATTRIBUTED [upstream-semantics] R19.1 / #163 → TE51 FR-G
- digit_key_focuses_the_type_something_row: ATTRIBUTED [upstream-semantics] R19.1 / #163 → TE51 FR-G
- digit_key_outside_numbered_rows_is_ignored: MATCH
- blank_type_something_enter_is_ignored: ATTRIBUTED [upstream-semantics] R19.1 / #301 → TE51 FR-G
- custom_notes_key_matches_as_typed: ATTRIBUTED [upstream-semantics] R19.1 / #294 → TE51 FR-G
- custom_notes_key_spec_matches_ctrl_n: ATTRIBUTED [upstream-semantics] R19.1 / #294 → TE51 FR-G
- space_csi_u_toggles_in_multi_select: ATTRIBUTED [upstream-semantics] R19.1 / #287 → TE51 FR-G

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

- state/tab_switch_syncs_multi_checked_from_answers (R19.1 / #281 → TE51 FR-G)
- keys/digit_key_answers_a_single_select_row (R19.1 / #163 → TE51 FR-G)
- keys/digit_key_toggles_a_multi_select_row (R19.1 / #163 → TE51 FR-G)
- keys/digit_key_focuses_the_type_something_row (R19.1 / #163 → TE51 FR-G)
- keys/blank_type_something_enter_is_ignored (R19.1 / #301 → TE51 FR-G)
- keys/custom_notes_key_matches_as_typed (R19.1 / #294 → TE51 FR-G)
- keys/custom_notes_key_spec_matches_ctrl_n (R19.1 / #294 → TE51 FR-G)
- keys/space_csi_u_toggles_in_multi_select (R19.1 / #287 → TE51 FR-G)

## RESULT: MATCH
