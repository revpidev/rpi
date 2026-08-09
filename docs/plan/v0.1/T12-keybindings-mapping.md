# T12 keybindings 逐条映射核对表

> 基准：`external/pi/packages/coding-agent/docs/keybindings.md`（198 行，~80 动作）@ pi 0.82.1 (2efa728)。
> 本地定义表：`crates/rpi/src/core/keybindings.rs`（73 条 = 31 `tui.*` + 42 `app.*`，S4b 已全量核对默认键一致）。
> 接线状态列：**wired** = 已接线（T12-S5b）；**hook** = 挂点/提示；**internal** = 组件内部处理（S5a 选择器局部键）；**n/a** = 无默认绑定或平台不适用。

## tui.*（31 条，编辑器/输入/选择器内部处理 — internal）

| id | 默认键 | 处理位置 |
|---|---|---|
| tui.editor.cursorUp / cursorDown / cursorLeft / cursorRight | up / down / left,ctrl+b / right,ctrl+f | rpi-tui Editor（internal） |
| tui.editor.cursorWordLeft / cursorWordRight | alt+left,ctrl+left,alt+b / alt+right,ctrl+right,alt+f | rpi-tui Editor（internal） |
| tui.editor.cursorLineStart / cursorLineEnd | home,ctrl+a / end,ctrl+e | rpi-tui Editor（internal） |
| tui.editor.jumpForward / jumpBackward | ctrl+] / ctrl+alt+] | rpi-tui Editor（internal） |
| tui.editor.pageUp / pageDown | pageUp / pageDown | rpi-tui Editor（internal） |
| tui.editor.deleteCharBackward / deleteCharForward | backspace / delete,ctrl+d | rpi-tui Editor（internal） |
| tui.editor.deleteWordBackward / deleteWordForward | ctrl+w,alt+backspace / alt+d,alt+delete | rpi-tui Editor（internal） |
| tui.editor.deleteToLineStart / deleteToLineEnd | ctrl+u / ctrl+k | rpi-tui Editor（internal） |
| tui.editor.yank / yankPop / undo | ctrl+y / alt+y / ctrl+- | rpi-tui Editor（internal） |
| tui.input.newLine / submit / tab | shift+enter,ctrl+j / enter / tab | rpi-tui Editor（internal；submit → on_submit → 分发链） |
| tui.input.copy | ctrl+c | rpi-tui Editor（internal；编辑器非空时由 CustomEditor 的 app.clear 拦截，见下） |
| tui.select.up / down / pageUp / pageDown / confirm / cancel | up / down / pageUp / pageDown / enter / escape,ctrl+c | rpi-tui SelectList + 各选择器 handle_input（internal） |

## app.*（42 条）

| id | 默认键 | 接线状态 | 处理位置 |
|---|---|---|---|
| app.interrupt | escape | wired | CustomEditor → on_escape → Escape 命令（streaming abort/恢复队列、bash 退出、双 Escape） |
| app.clear | ctrl+c | wired | CustomEditor → handle_ctrl_c（双击 500ms 退出，interactive-mode.ts:3533-3541） |
| app.exit | ctrl+d | wired | CustomEditor → handle_ctrl_d（空编辑器退出） |
| app.suspend | ctrl+z（win 无） | wired | handle_ctrl_z：ui.stop + SIGTSTP；run 循环 SIGCONT 恢复（3690-3725） |
| app.thinking.cycle | shift+tab | wired | cycle_thinking_level（3778-3787） |
| app.model.cycleForward | ctrl+p | wired | cycle_model(Forward)（3789-3826，spawn async） |
| app.model.cycleBackward | shift+ctrl+p | wired | cycle_model(Backward) |
| app.model.select | ctrl+l | wired | show_model_selector（4454） |
| app.tools.expand | ctrl+o | wired | toggle_tool_output_expansion（S4b，header 联动） |
| app.thinking.toggle | ctrl+t | wired | toggle_thinking_block_visibility（S4b） |
| app.session.toggleNamedFilter | ctrl+n | hook | handle_toggle_named_filter（选择器内由 SessionList 处理） |
| app.editor.external | ctrl+g | wired | handle_open_external_editor → `external_editor.rs` 全实现（temp prompt.md、停 TUI、spawn $VISUAL/$EDITOR/nano、读回）；extension-editor 组件 on_external_editor 钩子仍 T15 |
| app.message.copy | ctrl+x | wired | handle_copy_command（/copy；OSC52 写入） |
| app.message.followUp | alt+enter | wired | EditorInput::FollowUp → run loop handle_follow_up（3727-3757） |
| app.message.dequeue | alt+up | wired | handle_dequeue（restore_queued_messages_to_editor，合并全部队列放回） |
| app.clipboard.pasteImage | ctrl+v（win alt+v） | wired | UiCommand::PasteImage → handle_paste_image_impl（xclip PNG 探测 3s 超时 + xclip/wl-paste/pbpaste 文本回退；原生 addon 读取仍 T13） |
| app.session.new | *none* | wired | 转 `/new` 分发链（5859） |
| app.session.tree | *none* | wired | 转 `/tree` 分发链（4635） |
| app.session.fork | *none* | wired | 转 `/fork` 分发链（4576） |
| app.session.resume | *none* | wired | 转 `/resume` 分发链（4770） |
| app.tree.foldOrUp / unfoldOrDown | ctrl+left,alt+left / ctrl+right,alt+right | internal | TreeList（tree_selector.rs，S5a） |
| app.tree.editLabel | shift+l | internal | TreeSelectorComponent（S5a） |
| app.tree.toggleLabelTimestamp | shift+t | internal | TreeSelectorComponent（S5a） |
| app.tree.filter.default / noTools / userOnly / labeledOnly / all / cycleForward / cycleBackward | ctrl+d / ctrl+t / ctrl+u / ctrl+l / ctrl+a / ctrl+o / shift+ctrl+o | internal | TreeList filter 循环（tree_selector.rs，S5a） |
| app.models.save | ctrl+s | internal | ScopedModelsSelectorComponent（S5a） |
| app.models.enableAll / clearAll | ctrl+a / ctrl+x | internal | ScopedModelsSelectorComponent（S5a） |
| app.models.toggleProvider | ctrl+p | internal | ScopedModelsSelectorComponent（S5a） |
| app.models.reorderUp / reorderDown | alt+up / alt+down | internal | ScopedModelsSelectorComponent（S5a） |
| app.session.togglePath | ctrl+p | internal | SessionList（session_selector.rs，S5a） |
| app.session.toggleSort | ctrl+s | internal | SessionList（session_selector.rs，S5a） |
| app.session.rename | ctrl+r | internal | SessionList rename 模式（S5a；on_rename → append_session_info） |
| app.session.delete | ctrl+d | internal | SessionList 删除确认态（S5a；on_delete → trash/unlink） |
| app.session.deleteNoninvasive | ctrl+backspace | internal | SessionList（S5a） |

## 双击手势与组合键（docs 未列表但上游实现）

| 手势 | 语义 | 接线状态 |
|---|---|---|
| Ctrl+C ×2（500ms） | 退出 | wired（handle_ctrl_c，S4b） |
| Escape ×2（500ms，空编辑器） | doubleEscapeAction（tree/fork/none） | wired（S5a 起接 show_tree_selector/show_user_message_selector） |

## 核对结论

- 默认键：73 条与 keybindings.md 全部一致（含平台差异：win 无 ctrl+z、win alt+v、mac 树键序）——S4b 已核对，本表复核无差异。
- 分发优先级（CustomEditor.handle_input，S5a）：扩展快捷键（T15 挂点）→ pasteImage → interrupt → exit → 其余 actionHandlers → 编辑器默认处理。
- 未接线项：extension 快捷键（T15）。（2026-08-06 更新：app.editor.external 已经 `external_editor.rs` 全实现接线；app.clipboard.pasteImage 已经 `handle_paste_image_impl` 接线——3s 超时 + 平台工具降级，原生 addon 读取属 T13。）
