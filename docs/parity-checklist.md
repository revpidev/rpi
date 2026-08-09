# Pir Parity Checklist（T15 W8 Parity Freeze）

> 上游基准：`external/pi` @ `2efa728`（pi 0.82.1，只读 pin，UPSTREAM.md）。
> 每类逐项标注对拍证据（测试名 / fixture 路径 / 文件:行号）；有意差异单独
> 一节列出并以 ADR 钉死。测试均在 `cargo test --workspace` 内运行
> （live 测试未设 `PIR_LIVE_TEST=1` 默认跳过）。

- **冻结日期**：2026-08-09
- **测试基线**：3814 passed / 0 failed（含本文件引用的全部锚点测试）

---

## 1. 协议

### 1.1 Provider 协议适配（T03 / T13）

| 适配器 | 实现锚点 | 对拍证据 |
|--------|----------|----------|
| anthropic-messages | `crates/pir-ai/src/api/anthropic_messages.rs` | `crates/pir-ai/tests/contract_adapters.rs`、`providers_group_a.rs` |
| openai-completions | `crates/pir-ai/src/api/openai_completions.rs` | `contract_adapters.rs`、`providers_group_*.rs` |
| openai-responses | `crates/pir-ai/src/api/openai_responses.rs` | `contract_adapters.rs` |
| pi-messages | `crates/pir-ai/src/api/pi_messages.rs` | `contract_pi_messages.rs`（T13，D-021） |
| mistral-conversations | `crates/pir-ai/src/api/mistral_conversations.rs` | `contract_mistral_conversations.rs`（D-022） |
| google-generative-ai | `crates/pir-ai/src/api/google_generative_ai.rs` | `contract_google_generative_ai.rs`（D-023） |
| azure-openai-responses | `crates/pir-ai/src/api/azure_openai_responses.rs` | `contract_azure_openai_responses.rs`（D-024） |
| google-vertex（ADC 子集） | `crates/pir-ai/src/api/google_vertex.rs` | `contract_google_vertex.rs`（D-025） |
| bedrock-converse-stream（手写 SigV4 + event-stream） | `crates/pir-ai/src/api/bedrock_converse_stream.rs` | `contract_bedrock_converse_stream.rs`（D-026） |
| openai-codex-responses（WS 状态机 + 缓存续传 + zstd） | `crates/pir-ai/src/api/codex_ws.rs` | `contract_openai_codex_responses.rs`（D-027；状态机图钉死于 `02-design.md` §13） |
| compat 修正矩阵 | `crates/pir-ai/src/api/openai_completions.rs:199`（`detect_compat` 表驱动） | `crates/pir-ai/tests/compat_matrix.rs` |
| 38 provider 工厂 + 模型目录（37 vendored JSON / 1153 模型） | `crates/pir-ai/src/providers/` | `providers_group_a-d.rs`、`model_catalog.rs` |
| OAuth 6 流程 + load registry | `crates/pir-ai/src/auth/` | `oauth_codex_openrouter.rs`、`oauth_copilot_radius.rs`、`oauth_kimi_xai.rs`（D-029~D-035） |
| images 子系统 | `crates/pir-ai/src/images.rs`、`images/` | `crates/pir-ai/tests/images.rs`（D-037） |

事件流线格式对拍：`fixtures/generated/<scenario>/events.jsonl`（上游
`createAgentSession` + faux 实录，`fixtures/generate-fixtures.mjs` 生成）vs
pir 全栈事件流 —— `crates/pir/tests/parity_headless_test.rs`（5 场景，
归一化口径见文件头注释）。

### 1.2 RPC 帧协议（32 命令）

- 命令枚举：`crates/pir/src/modes/rpc.rs:98`（`RpcCommand`，32 变体：
  prompt/steer/followUp/abort/newSession/getState/setModel/cycleModel/
  getAvailableModels/setThinkingLevel/cycleThinkingLevel/
  getAvailableThinkingLevels/setSteeringMode/setFollowUpMode/compact/
  setAutoCompaction/setAutoRetry/abortRetry/bash/abortBash/getSessionStats/
  exportHtml/switchSession/fork/clone/getForkMessages/getEntries/getTree/
  getLastAssistantText/setSessionName/getMessages/getCommands）。
- 契约基准：上游 `external/pi/packages/coding-agent/docs/rpc.md`。
- 测试锚点：`crates/pir/tests/rpc_mode_test.rs`（36 用例）——
  `prompt_lifecycle_messages_state_stats`、`model_and_thinking_commands`、
  `cycle_commands_null_data_paths`、`queue_mode_and_toggle_commands`、
  `bash_commands`、`bash_abort_roundtrip`、`entries_tree_fork_messages`、
  `session_replacement_commands`、`compact_command`、
  `steer_follow_up_abort_during_streaming`、`session_name_and_get_commands`、
  `get_commands_with_prompt_template`、`protocol_errors_and_framing`、
  `export_html_in_memory_session_errors`、`pir_rpc_bin_end_to_end`（进程级
  EOF 退出哨兵）、信号退出码两例。

### 1.3 扩展 UI RPC 子协议（`extension_ui_request`）

- 9 协议方法 = 4 对话（`select`/`confirm`/`input`/`editor`，id 配对阻塞等待
  `extension_ui_response`）+ 5 fire-and-forget（`notify`/`setStatus`/
  `setWidget`/`setTitle`/`set_editor_text`）：常量钉于
  `crates/pir/src/modes/rpc.rs:49-57`。
- 18 降级方法逐项语义：`EXTENSION_UI_DEGRADED_METHODS`（rpc.rs:68 起，
  注释逐条对齐 rpc-mode.ts:162-309）。
- 实现锚点：`crates/pir/src/modes/rpc/ui_bridge.rs:127`（`RpcUiBridge`）。
- 测试锚点（W4 契约）：`w4_rpc_select_frame_shape_and_value_response`、
  `w4_rpc_select_cancelled_maps_to_none`、
  `w4_rpc_confirm_frame_and_response_mapping`、`w4_rpc_input_and_editor_frames`、
  `w4_rpc_dialog_timeout_auto_resolves_default`、`w4_rpc_fire_and_forget_frames`、
  `w4_rpc_degraded_methods`（`crates/pir/tests/extension_host_w4_test.rs`）。

---

## 2. Session 格式

| 项 | 证据 |
|----|------|
| JSONL v3 无损往返（加载 → export 归一化 diff） | `parity_fixture_sessions_load_and_export_lossless`（`crates/pir/tests/parity_session_test.rs`；5 个上游实录 fixture：`abort`/`length-truncation`/`single-turn`/`steering-followup`/`tool-calls`） |
| 加载 + 续跑（SessionManager 层追加/重开状态一致） | `parity_fixture_sessions_continue_after_load`（同文件） |
| **W8 终验：fixture → 全栈 AgentSession faux prompt 续跑一轮** | `parity_fixture_session_prompt_continue_with_faux_provider`（同文件；文件 +2 行、原行不动、重开 context 完整） |
| v1–v3 自动迁移（主路径） | `crates/pir/src/core/session_manager.rs` 迁移单测（`session_manager::tests`）+ `parity_session_test.rs` 头注口径 |
| compaction 对拍（threshold/overflow 两场景 golden） | `crates/pir/tests/parity_compaction_test.rs`；golden 由 `fixtures/generate-compaction-golden.mjs` 生成 |
| harness v3 硬校验 + 双向互通（harness↔主路径） | `crates/pir/tests/parity_harness_interop_test.rs`（T16：11 种条目全量、两形态 compaction、fixtures 三方交叉） |
| 基准文档 | `external/pi/packages/coding-agent/docs/session-format.md`（G3 逐条对拍级基准） |
| pir 产出被上游格式校验 | 上游无独立校验器；口径 = pir 产出 JSONL 与上游 fixture 经同一 Normalizer（`pir-test-support/src/normalize.rs`）归一化 diff（`parity_headless_test.rs` session 对拍分支），即「pir 产出 ≡ 上游实录」的反向表达 |

---

## 3. 扩展 API（33 事件 + 24 API + 28 UI + 3 Context = 88 条）

宿主实现：`crates/pir-ext-host/`；会话侧接缝：`crates/pir/src/core/extensions.rs`
（`ExtensionRunner` trait）+ `extension_host_adapter.rs`。ABI 口径：
`docs/extension-abi.md`（L0/L1 同一 method 表）。

### 3.1 事件（33）

通用分发：`runner.rs:493`（串行，加载序→注册序；`runner_emit_dispatches_
serially_in_load_then_registration_order` / `runner_emit_isolates_handler_
errors_and_continues`）。可变语义特化分发单列「分发锚点」。

| # | 事件 | 触发锚点（pir） | 分发锚点（pir-ext-host） | 测试锚点 |
|---|------|----------------|--------------------------|----------|
| 1 | `project_trust` | `app.rs:917` | `runner.rs:938`（首决策胜 + 错误收集） | `runner_project_trust_*`（runner_test.rs）、`w2_project_trust_two_phase_load_and_extension_decision`、`w7_async_trust_selector_resolves_and_persists` |
| 2 | `resources_discover` | `agent_session.rs:3024/3029` | `runner.rs:836`（路径聚合 + 扩展标签） | `runner_resources_discover_aggregates_paths_with_extension_tags`、`w2_resources_discover_extends_resource_loader` |
| 3 | `session_start` | `agent_session.rs:3003/3114` | 通用 emit | `w5_new_session_replacement_sequence_and_stale`、`w5_session_reload_event_sequence` |
| 4 | `session_info_changed` | `agent_session.rs:2493` | 通用 emit | 通用 emit 通道（runner_test）+ `session_name_and_get_commands`（rpc_mode_test.rs，setSessionName 触发路径） |
| 5 | `session_before_switch`（cancel） | `agent_session_runtime.rs:177-185` | `emit_cancelable_with` | `w5_session_before_switch_cancel_blocks_replacement`、`runner_session_before_cancel_short_circuits` |
| 6 | `session_before_fork`（cancel） | `agent_session_runtime.rs:197-205` | `emit_cancelable_with` | `runner_session_before_cancel_short_circuits`、`runner_session_before_last_non_null_result_wins_without_cancel` |
| 7 | `session_before_compact`（cancel/替换） | `compaction_runner.rs:365` | `emit_session_before_compact` | `w2_session_before_compact_cancel_aborts_manual_compaction`、`w2_session_before_compact_no_handlers_runs_default_path`、`w2_session_before_compact_replacement_sets_from_extension_and_emits_session_compact` |
| 8 | `session_compact` | `compaction_runner.rs:475` | 通用 emit | `w2_session_before_compact_replacement_sets_from_extension_and_emits_session_compact` |
| 9 | `session_shutdown` | `agent_session.rs:3082-3086`、`agent_session_runtime.rs:223-230/644` | 通用 emit | `w5_new_session_replacement_sequence_and_stale`（替换时序含 shutdown） |
| 10 | `session_before_tree` | `agent_session.rs:2559` | `emit_session_before_tree` | `w5_navigate_tree_cancel_branch` |
| 11 | `session_tree` | `agent_session.rs:2693` | 通用 emit | `w5_navigate_tree_cancel_branch`（navigate 序列） |
| 12 | `context`（链式替换 messages） | `sdk.rs:445` | `runner.rs:686` | `runner_context_transforms_chain` |
| 13 | `before_provider_request`（链式，undefined 不替换） | `sdk.rs:454` | `runner.rs:714` | `runner_before_provider_request_undefined_does_not_replace` |
| 14 | `before_provider_headers`（返回对象替换；null 删 header） | `sdk.rs:402` | `runner.rs:742` | `runner_before_provider_headers_returned_object_replaces` |
| 15 | `after_provider_response` | `sdk.rs` stream_fn `on_response` | 通用 emit | `w2_after_provider_response_reaches_extension`、`w2_after_provider_response_skipped_without_handlers` |
| 16 | `before_agent_start`（注入 + 链式替换 systemPrompt） | `agent_session.rs:1547` | `runner.rs:770` | `runner_before_agent_start_collects_messages_and_chains_system_prompt`、`runner_before_agent_start_ctx_get_system_prompt_reflects_chain`、`runner_before_agent_start_no_results_returns_none`、`w2_before_agent_start_chains_system_prompt_and_injects_messages` |
| 17 | `agent_start` | `agent_session.rs:767` | 通用 emit | `parity_headless_test.rs` 事件流对拍、`agent_session_test.rs::prompt_lifecycle_events_and_persistence` |
| 18 | `agent_end` | `agent_session.rs:98/768` | 通用 emit | 同 17 |
| 19 | `agent_settled` | `agent_session.rs:633` | 通用 emit | 同 17 + `parity_tools_test.rs` |
| 20 | `turn_start` | `agent_session.rs:769` | 通用 emit | 同 17 |
| 21 | `turn_end` | `agent_session.rs:770` | 通用 emit | 同 17 |
| 22 | `message_start` | `agent_session.rs:771` | 通用 emit | 同 17 |
| 23 | `message_update` | `agent_session.rs:772` | 通用 emit | 同 17（对拍中 delta 边界整类排除，口径见 parity_headless 头注） |
| 24 | `message_end`（替换保 role） | `agent_session.rs:774` | `runner.rs:526` | `runner_message_end_replaces_and_chains`、`runner_message_end_role_mismatch_is_rejected`、`runner_message_end_unmodified_returns_none` |
| 25 | `tool_execution_start` | `agent_session.rs:785` | 通用 emit | 同 17 |
| 26 | `tool_execution_update` | `agent_session.rs:786` | 通用 emit | 同 23 |
| 27 | `tool_execution_end` | `agent_session.rs:787` | 通用 emit | 同 17（完成序归一化排序，口径见 parity_headless 头注） |
| 28 | `model_select` | `agent_session.rs:1862/1868` | 通用 emit | 通用 emit 通道 + `model_and_thinking_commands`（rpc_mode_test.rs，setModel 触发路径） |
| 29 | `thinking_level_select` | `agent_session.rs:2082` | 通用 emit | 同 28 |
| 30 | `user_bash`（首个非空胜；operations 缺口见 §5） | `commands_selectors.rs:1739` | `runner.rs:668` | `runner_user_bash_first_non_null_result_wins`、`w2_user_bash_full_result_replacement`、`w2_user_bash_operations_only_and_no_handler_fall_back` |
| 31 | `input`（三态：transform/continue/handled 短路） | `agent_session.rs:1438` | `runner.rs:877` | `runner_input_transforms_chain_and_continue_when_unchanged`、`runner_input_handled_short_circuits`、`runner_input_continue_when_nothing_changed`、`runner_input_transform_without_images_keeps_current_images` |
| 32 | `tool_call`（改参穿线/block/fail-safe） | `sdk.rs:479`（`before_tool_call` 钩子） | `runner.rs:625` | `runner_tool_call_input_threads_through_handlers`、`runner_tool_call_block_short_circuits`、`runner_tool_call_handler_errors_propagate`、`w2_tool_call_args_mutation_applies_without_revalidation`、`w2_tool_call_block_short_circuits_execution`、`w2_tool_call_handler_error_fail_safe_blocks`、`w6_wasm_tool_executes_and_gate_blocks_in_agent_loop` |
| 33 | `tool_result`（局部补丁链式） | `sdk.rs:482`（`after_tool_call` 钩子） | `runner.rs:569` | `runner_tool_result_partial_patches_chain_across_handlers`、`runner_tool_result_unmodified_returns_none`、`w2_tool_result_partial_patches_chain_across_extensions` |

### 3.2 API 方法（24，含 `events` 属性）

pi 对象实现：`crates/pir-ext-host/src/api.rs`（`ExtensionApi`）；动作绑定：
`crates/pir/src/core/extension_actions.rs`（`SessionHostActions`）。

| # | 方法 | 实现锚点 | 测试锚点 |
|---|------|----------|----------|
| 34 | `on(event, handler)` | `api.rs:1416` | `api_on_typed_round_trips_payload_and_result`、`api_on_typed_deserialize_failure_is_a_handler_error` |
| 35 | `registerTool` | `api.rs:1454` | `api_register_tool_refreshes_only_when_bound`、`w3_extension_tool_overrides_builtin_definition_and_execution`、`w6_wasm_tool_executes_and_gate_blocks_in_agent_loop` |
| 36 | `registerCommand`（`:N` 后缀冲突规则） | `api.rs:1468/1486` | `w3_extension_command_executes_via_prompt`、`runner_resolves_command_conflicts_with_numeric_suffix` |
| 37 | `registerShortcut`（保留键/last-wins） | `api.rs:1506` | `runner_shortcut_conflicts_last_wins_with_diagnostic`、`runner_shortcut_reserved_builtin_key_skips_extension`、`runner_shortcut_non_reserved_builtin_conflict_extension_wins` |
| 38 | `registerFlag` | `api.rs:1523` | `api_flag_defaults_and_per_extension_visibility`、`w3_flag_values_registered_applied_unknown_errors`、`runner_resolves_flag_conflicts_first_registration_wins` |
| 39 | `getFlag`（仅本扩展可见） | `api.rs:1546` | `api_flag_defaults_and_per_extension_visibility` |
| 40 | `registerMessageRenderer`（首注册静默胜） | `api.rs:1555` | `runner_renderer_first_registration_wins_silently`、`w4_message_renderer_descriptor_renders_in_tui` |
| 41 | `registerEntryRenderer` | `api.rs:1567` | `runner_entry_renderer_first_registration_wins_silently`（W8 补） |
| 42 | `sendMessage`（deliverAs/triggerTurn 三分支） | `api.rs:1580` | `w3_send_message_idle_without_trigger_turn_appends_only`、`w3_send_message_next_turn_queues_for_next_prompt`、`w3_send_message_streaming_follow_up_queues_into_run` |
| 43 | `sendUserMessage` | `api.rs:1593` | `w3_send_user_message_streaming_without_deliver_as_errors` |
| 44 | `appendEntry` | `api.rs:1606` → `extension_actions.rs:135` → `agent_session.rs:1792` | `api_actions_forward_after_bind`（绑定后转发全动作面） |
| 45 | `setSessionName` | `api.rs:1615` | `session_name_and_get_commands`（rpc_mode_test.rs） |
| 46 | `getSessionName` | `api.rs:1622` | 同 45 |
| 47 | `setLabel` | `api.rs:1628` → `extension_actions.rs:154` | `api_actions_forward_after_bind`；label 条目互通 `parity_harness_interop_test.rs` |
| 48 | `exec`（超时 SIGKILL，见 §5） | `api.rs:1635` → `extension_actions.rs:171/345` | `w3_exec_runs_command_and_reports_timeout` |
| 49 | `getActiveTools` | `api.rs:1649` | `w3_set_active_tools_ignores_unknown_and_rebuilds_prompt` |
| 50 | `getAllTools`（含 sourceInfo） | `api.rs:1655` | 同 49 |
| 51 | `setActiveTools`（未知名静默忽略 + 重建 prompt + addedToolNames） | `api.rs:1661` | `w3_set_active_tools_ignores_unknown_and_rebuilds_prompt`、`w3_added_tool_names_pure_addition_branch`、`w3_added_tool_names_suppressed_when_tools_removed` |
| 52 | `getCommands`（扩展优先 + 后缀序） | `api.rs:1668` | `w3_get_commands_orders_extension_first_with_suffixes`、`get_commands_with_prompt_template`（rpc_mode_test.rs） |
| 53 | `setModel`（无 key 返 false） | `api.rs:1674` → `extension_actions.rs:227` | `model_and_thinking_commands`（rpc_mode_test.rs） |
| 54 | `getThinkingLevel` | `api.rs:1680` | 同 53 |
| 55 | `setThinkingLevel`（clamp） | `api.rs:1686` | 同 53 |
| 56 | `registerProvider`（双签名；闭包子项显式拒绝，见 §5） | `api.rs:1694/1705` | `w3_register_provider_queue_flush_runtime_and_unregister`、`api_provider_registration_queues_pre_bind_and_flushes_on_bind`、`api_provider_flush_failure_reports_extension_error` |
| 57 | `unregisterProvider`（恢复内建） | `api.rs:1718` | `w3_register_provider_queue_flush_runtime_and_unregister` |
| 58 | `events`（EventBus 共享总线） | `api.rs:1725`（`api.rs:73-108`） | `w3_events_bus_cross_extension_pub_sub`、`api_event_bus_emit_on_unsubscribe_clear` |

### 3.3 UI 方法（28）

trait：`crates/pir-ext-host/src/api.rs:412-516`（`UiBridge`）。三桥：
`InteractiveUiBridge`（`interactive_mode/ui_bridge.rs:134`）/
`RpcUiBridge`（`rpc/ui_bridge.rs:127`）/ `NullUiBridge`（`bridges.rs:49`，
对齐上游 noOpUIContext，`null_bridge_matches_noop_ui_context`）。

| # | 方法 | trait 锚点 | 测试锚点 |
|---|------|-----------|----------|
| 59 | `select` | `api.rs:414` | `w4_rpc_select_frame_shape_and_value_response`、`w4_rpc_select_cancelled_maps_to_none`、interactive 对话框 VT（`w4_session_with_host` 系） |
| 60 | `confirm` | `api.rs:422` | `w4_rpc_confirm_frame_and_response_mapping` |
| 61 | `input` | `api.rs:425` | `w4_rpc_input_and_editor_frames` |
| 62 | `notify` | `api.rs:433` | `w4_rpc_fire_and_forget_frames` |
| 63 | `onTerminalInput`（interactive only） | `api.rs:436` | `w4_rpc_degraded_methods`（降级臂）；interactive 注册路径 `ui_bridge.rs` |
| 64 | `setStatus` | `api.rs:439` | `w4_rpc_fire_and_forget_frames` |
| 65 | `setWorkingMessage` | `api.rs:442` | `w4_rpc_degraded_methods`（RPC 降级）；NullUiBridge 逐项（`null_bridge_matches_noop_ui_context`） |
| 66 | `setWorkingVisible` | `api.rs:445` | 同 65 |
| 67 | `setWorkingIndicator` | `api.rs:448` | 同 65 |
| 68 | `setHiddenThinkingLabel` | `api.rs:451` | 同 65 |
| 69 | `setWidget` | `api.rs:454` | `w4_rpc_fire_and_forget_frames` |
| 70 | `setFooter` | `api.rs:462` | 同 65（降级/Null 臂） |
| 71 | `setHeader` | `api.rs:465` | 同 65 |
| 72 | `setTitle` | `api.rs:468` | `w4_rpc_fire_and_forget_frames` |
| 73 | `custom`（声明式 v1 无交互回传，ADR-0007） | `api.rs:472` | `w4_rpc_degraded_methods`（RPC → undefined）；interactive 立即 resolve（`ui_bridge.rs:415`） |
| 74 | `pasteToEditor` | `api.rs:475` | `w4_rpc_degraded_methods`（RPC 委托 setEditorText） |
| 75 | `setEditorText` | `api.rs:478` | `w4_rpc_fire_and_forget_frames` |
| 76 | `getEditorText` | `api.rs:479` | `w4_rpc_degraded_methods`（RPC 恒 ""） |
| 77 | `editor` | `api.rs:482` | `w4_rpc_input_and_editor_frames` |
| 78 | `addAutocompleteProvider` | `api.rs:485` | 同 65（降级/Null 臂） |
| 79 | `setEditorComponent` | `api.rs:488` | 同 65 |
| 80 | `getEditorComponent` | `api.rs:489` | `w4_rpc_degraded_methods`（RPC → undefined） |
| 81 | `theme` | `api.rs:492` | 同 65 |
| 82 | `getAllThemes` | `api.rs:495` | `w4_rpc_degraded_methods`（RPC → []） |
| 83 | `getTheme` | `api.rs:498` | `w4_rpc_degraded_methods`（RPC → undefined） |
| 84 | `setTheme` | `api.rs:501` | `w4_rpc_degraded_methods`（RPC → `{success:false}`） |
| 85 | `getToolsExpanded` | `api.rs:504` | `w4_rpc_degraded_methods`（RPC → false） |
| 86 | `setToolsExpanded` | `api.rs:505` | 同 65 |

声明式组件树（ComponentTree v1）：schema 常量 `types.rs:802`；映射器
`crates/pir/src/modes/interactive/component_tree.rs`；测试
`w4_component_tree_text_spacer_box_column`、
`w4_tool_render_override_and_inheritance`、
`w4_message_renderer_descriptor_renders_in_tui`。

### 3.4 Context 三级（3）

| # | 级别 | 实现锚点 | 测试锚点 |
|---|------|----------|----------|
| 87a | `ExtensionContext`（isIdle/isProjectTrusted/signal/abort/hasPendingMessages/shutdown 模式差异/getContextUsage/compact fire-and-forget/getSystemPrompt/mode/hasUI/cwd/ui/events/model） | `api.rs:917-1053`；动作层 `ContextActions`（`api.rs:545-570`）→ `extension_actions.rs` | `w5_context_base_accessors`、`w5_context_compact_fires_and_forgets_with_callback`、`w5_context_shutdown_invokes_mode_handler`、`unbound_ui_falls_back_to_null_bridge_and_has_ui_false` |
| 87b | `ExtensionCommandContext`（getSystemPromptOptions/waitForIdle/newSession/fork/navigateTree/switchSession/reload；仅 command 分发内可用，事件上下文 invalidRequest） | `api.rs:1057-1150`；动作层 `CommandContextActions`（`api.rs:597-619`）→ `extension_context.rs:191`（`RuntimeCommandActions`，仅 RPC 模式绑定——ADR-0007 缺口 2） | `w5_new_session_replacement_sequence_and_stale`、`w5_navigate_tree_cancel_branch`、`w5_reload_reruns_factories_preserves_flags_and_stales_old`、`w7_switch_session_cross_cwd_uses_async_trust_selector` |
| 87c | `ReplacedSessionContext`（sendMessage/sendUserMessage 绑新 session） | `api.rs:1161-1196` | `w5_new_session_replacement_sequence_and_stale`（withSession 序列） |
| 88 | stale 失效语义（invalidate/assertActive/默认文案） | `api.rs:698-716`、`DEFAULT_STALE_MESSAGE`（`api.rs:626`） | `api_stale_runtime_rejects_calls`、`runner_invalidate_marks_stale_first_message_wins`、`w5_reload_reruns_factories_preserves_flags_and_stales_old` |

> 编号说明：三级 Context 按 87a/87b/87c 计 3 条，stale 语义为第 88 条
> （需求 §9.1「三级 Context」的失效契约随级交付）。

### 3.5 L1 wasm 宿主与 capability 沙箱（横切证据）

- ABI v1：`docs/extension-abi.md`；宿主 `crates/pir-ext-host/src/wasm/`。
- `wasm_tool_guest_registers_and_executes`、`wasm_gate_guest_blocks_tool_call`、
  `wasm_capability_denied_for_bare_guest_and_allowed_by_manifest`、
  `wasm_abi_version_mismatch_is_rejected`、`wasm_missing_export_is_a_load_error`、
  `wasm_fuel_stops_runaway_guest_init`、`wasm_module_cache_reused_within_generation`
  （`crates/pir-ext-host/tests/wasm_test.rs`）。
- L0/L1 行为一致对拍：`w6_native_and_wasm_gate_behave_identically`（同能力
  双实现）。
- L0 动态库插件（abi_stable）：`native_plugin_test.rs` 3 例 +
  `crates/pir-test-native-plugin` fixture。
- 安装管理 e2e：`w7_install_list_disable_enable_remove_wasm_package`、
  `w7_installed_wasm_package_loads_and_blocks`。

---

## 4. TUI 行为

| 项 | 证据 |
|----|------|
| 组件渲染快照黄金（36 个，ANSI 逐字节） | `crates/pir-tui/tests/snapshots.rs` + `tests/snapshots/*.snap`（36 文件） |
| 差分渲染/帧泵/终端控制单测 | `crates/pir-tui/src/tui.rs`（123 例）、`terminal.rs`（18 例）内联测试 |
| VT 帧工具 | `pir-test-support/src/vt.rs`（帧捕获/消毒/纯文本投影） |
| 键位映射 | `crates/pir/tests/keybindings_test.rs`（26 例）+ `docs/plan/v0.1/T12-keybindings-mapping.md`（逐条映射表） |
| tmux/terminal-setup 映射 | `docs/plan/v0.1/T11-tmux-terminal-setup-mapping.md` |
| interactive 模式行为 | T12 验收（真机 smoke 2026-08-06 通过，index.md 变更记录）；`docs/plan/v0.1/T12-requirements-8-mapping.md` |
| W4 扩展对话框/组件树 VT | `w4_component_tree_text_spacer_box_column`、`w4_message_renderer_descriptor_renders_in_tui`（`vt::strip_ansi` 投影断言） |
| 平台原生缺口（macOS 修饰键/Windows VT input） | 有意差异，ADR-0004（D-016） |

---

## 5. 有意差异（ADR 钉死）

| 差异 | ADR | 偏离登记 |
|------|-----|----------|
| CLI 名 `pir`、配置根 `~/.pir`、env 前缀 `PIR_*`、扩展 = Rust/Wasm（不兼容 TS/jiti） | ADR-0001 / ADR-0002 | —（第 3 层有意差异，需求 §1.5） |
| 范围排除（server/evals/bun/sqlite-node/pi-ai CLI/legacy 迁移） | ADR-0003 | — |
| grep/find/ls Rust 原生（ignore/globset 替代 rg/fd）；ls 排序 codepoint | ADR-0003 §2 / ADR-0005 | D-039 |
| macOS 原生修饰键检测、Windows VT input 两缺口 | ADR-0004 | D-016 |
| ~~switchSession 异cwd 信任提示降级~~（T15 W7 已接线消除） | ADR-0006 | D-044（已关闭） |
| 扩展 API v0.1 三缺口：user_bash `operations` 不支持、非 RPC 模式 `ctx.command.*` 未绑、`custom()` 无交互回传 | ADR-0007 | D-048（2/6）、D-049（1） |
| tool_call 改参穿线 / ProviderConfig 闭包拒绝 / newSession setup 省略 / exec SIGKILL 直杀 | 不需要（实现细节） | D-048（1/3/4/5） |
| ComponentTree v1 无 `row` | 不需要 | D-049（2） |
| L0 动态库无沙箱信任模型 / manifest `native` 字段 | 不需要（ADR-0002 已决策 abi_stable） | D-050 |
| renderedTools/ToolHtmlRenderer 不移植（W7 复核维持） | 不需要 | D-045 |
| 语法高亮 syntect 替代 hljs 10.7.3：高亮 ANSI token 分段与逐 token 配色不与上游对拍（同类 token 同色系，scope 锚定同组 theme 键）；结构/文案/钳制/计时仍逐字 | ADR-0008 | D-051（T17 落地） |
| 其余任务级实现细节差异 | 见各 D 文件 | D-001~D-047（deviations/README.md 登记表） |

---

## 6. 需求 §1.2 成功标准总核对

| # | 标准 | 结论 | 证据 |
|---|------|------|------|
| 1 | 行为对拍（print/json/rpc 同 prompt：工具调用序列/session JSONL/事件类型一致） | ✅ 通过 | `parity_headless_test.rs`（5 场景 print/json 全栈对拍，事件流 + session 双 diff）、`rpc_mode_test.rs`（36 例）、`parity_tools_test.rs`、归一化口径各文件头注 |
| 2 | 会话互通（v1–v3 主路径迁移 + harness v3） | ✅ 通过 | §2 全表；W8 终验 `parity_fixture_session_prompt_continue_with_faux_provider`；`parity_harness_interop_test.rs` 双向 + 三方交叉 |
| 3 | 资源互通（skills/prompts/themes/settings/keybindings/models.json + 凭据存储结构兼容，仅路径 ~/.pir） | ✅ 通过 | `parity_resources_test.rs`、`skills_test.rs`、`prompt_templates_test.rs`、`themes_test.rs`、`keybindings_test.rs`、`resource_loader_test.rs`；golden 生成器 `fixtures/generate-resources-golden.mjs`；凭据存储 `pir-ai/src/auth/`（D-008 结构兼容、路径 `~/.pir`） |
| 4 | 架构同构（四层 crate ↔ Pi 四包） | ✅ 通过 | `pir-ai`↔pi-ai、`pir-agent`↔pi-agent（harness）、`pir-tui`↔pi-tui、`pir`↔coding-agent；`02-design.md` §12 映射表 |
| 5 | 扩展（形状同构、Rust/Wasm 实现、不要求 TS 兼容） | ✅ 通过 | §3 全表（88 条逐条锚点）；ADR-0001；有意差异 ADR-0007 三缺口已钉死 |
