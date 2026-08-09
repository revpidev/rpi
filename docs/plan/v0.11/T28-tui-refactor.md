# T28：rpi-tui 渲染器 trait 化与行为修正

- **状态**：未开始
- **里程碑**：M4
- **依赖**：T17
- **上游对照**：`c13ffe187` 起的重构链（`b103937d3` 双渲染器 + Proxy 引用、`b70c0f5b4` 回退不参考）、`29d9f087c`（输入即时渲染）、`dfe47d3fb`（graphemeWidth #6987）、`b780d20aa`+`229afb825`（OSC 8 关闭 #7657）、`0e633790c`（颜色批量 #7550）、`e8a17822d`（OSC 9;4 #7581）、`bf4a90d81`（SettingsList 空格）、`b0d382e25`+`16ad96ae8`（键位）、`fa07e7bd9`+`73dd066ee`（Windows）、`5446cd754`（tuiMode 更名）、`18dee5f0a`（渲染性能）；`packages/tui/src/{tui-base,tui-main-screen}.ts`
- **需求章节**：v0.11 需求 R5.1、R5.3、R5.5；设计 §4.1、§4.4、§4.5
- **预估**：0.6 人月

---

## 目标

rpi-tui 从单一 Tui struct 重构为「trait + 基类复用 + 双渲染器」结构，main-screen 渲染逻辑
**逐行等价迁移**（上游已验证等价，禁止顺手改）；同步落地既有行为修正。**第一天统一重录
TUI 逐帧黄金基线**（输入即时渲染使旧基线失效）。

## 范围

### In

- **重构**：`Tui` trait（type-level 接口面）+ `TuiBase`（输入分发/overlay 栈/渲染调度/颜色查询，字段共享以基类复用实现）+ `TuiMainScreen`（现有差分渲染整体迁入）；`TuiMode`（`regular|fullscreen`）、`TuiStopOptions{preserve_screen}`、`ViewportTui` trait（`set_layout_root`，全屏实现属 T31，本任务定接口）
- `stop(options)` 参数化：`preserve_screen: true` 时 main-screen 不写光标归位序列；默认路径行为不变
- `capture_render_state()/restore_render_state()`（main-screen 7 个渲染状态字段）
- 输入监听器更名 `InputListener` → `TuiInputListener`；`compositeTuiLine` 提升公共函数
- **行为修正**（逐条配回归）：
  - 输入即时渲染（`request_immediate_render()` 取消排队 timer；`render_now(force)`；`request_render(true)` 语义变更）
  - `grapheme_width()` 例外表重写（Spacing_Mark 减 `\u1734 \u302E \u302F` + 12 非间距例外 + Indic 辅音/FF00-FFEF/泰老 AM 尾随 +1；无基字符 spacing mark 按码点数）——`scripts/gen-tui-unicode-data.py` 更新，**码点级全表对拍**（不抽样）
  - `truncate_to_width` OSC 8 关闭序列 + 纯文本快路径
  - 颜色方案批量解析 `+` 量词；OSC 9;4 清除序列去分号
  - SettingsList 搜索空格参与过滤；Editor 默认键位扩充（ctrl+home/end/pageUp/pageDown）+ `tui.editor.historyPrevious/Next` action（默认无键）
  - Windows：truecolor 放宽（无 WT_SESSION 启用）、Shift+Enter 检测（native helper 修饰键查询——平台能力缺口处理沿用 ADR-0004 流程）
- **基线重录**：全部 VT 帧级黄金文件以新时序重录，首批人工评审
- 性能：整行 box 直接引用源行（分配减少 9–18x 场景，`render-churn-bench.ts` 为基准参考）

### Out

- 布局引擎与全屏渲染器实现（T30/T31）
- `--tui-mode` CLI 与 `/settings` 热切换接线（T32）
- LaTeX/Mermaid（T29）

## 开发要点

- 迁移与行为修正**分两个提交**：先纯迁移（旧基线应全绿，证明等价），再行为修正 + 基线重录
- 重构后 rpi 主路径（interactive-mode 等）全部调用点编译迁移，行为不变
- 上游 main-screen 等价验证（旧 `TUI.doRender` 368 行 vs 新 368 行仅两处等价改写）作为 rpi 侧迁移的核对基准

## 进度跟踪

- [ ] 设计细化
- [ ] 实现（迁移提交）
- [ ] 实现（行为修正 + 基线重录）
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] 纯迁移提交：v0.1 VT 帧级黄金 + 快照黄金 11 例全绿（未重录前）
- [ ] 行为修正后：重录基线入库，首批人工评审记录附验收
- [ ] grapheme 宽度例外表码点级全表对拍（`gen-tui-unicode-data.py` 产物 vs 上游 `graphemeWidth`）
- [ ] OSC 8 关闭 / 颜色批量 / OSC 9;4 / SettingsList 空格 / 键位扩充各回归
- [ ] 渲染分配基准对比（重录前后 render-churn 场景）

## 门禁验收

通用门禁 G1–G7 全过（G3 强制：基线重录记录；G4 重点：`--alt` 兼容保留、`uiMode` 旧键忽略）。

任务特有标准：

- [ ] 需求 R5.1/R5.3/R5.5 逐条核对表
- [ ] 「迁移等价」与「行为修正」两提交可分别审查

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| （待登记） | | |

## 验收记录

（按 gates §3 模板填写）
