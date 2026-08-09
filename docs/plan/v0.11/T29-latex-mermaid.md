# T29：LaTeX 与 Mermaid 渲染

- **状态**：未开始
- **里程碑**：M4
- **依赖**：T28、T27
- **上游对照**：`05e89b418`+`aa601d7ba`（LaTeX，`packages/tui/src/latex.ts` 1373 行、`test/latex.test.ts` 483 行）、`714978bf5`（Markdown transform + Marked 导出）、`66534fbdc`（mermaid 组件，`packages/coding-agent/src/modes/interactive/components/mermaid.ts`）；**Mermaid 蓝本**：[xai-org/grok-build](https://github.com/xai-org/grok-build) `crates/codegen/xai-grok-markdown/src/mermaid.rs`（5237 行，Apache-2.0）
- **需求章节**：v0.11 需求 R5.4、R3.3.1、R3.3.3；设计 §4.5、§5.6
- **预估**：0.5 人月

---

## 目标

Markdown 渲染新增 LaTeX（Unicode math）与 Mermaid（Unicode box-drawing）能力，
并接通扩展的宽度感知 transform 钩子。

## 范围

### In

- **LaTeX**（`rpi-tui/src/latex.rs`）：
  - tokenizer：`$...$`/`$$...$$`/`\(...\)`/`\[...\]`，含转义与 **pending 未闭合**处理（流式友好）
  - 渲染：符号表、上下标、分数（display 垂直堆叠）、根式、矩阵/对齐/cases、运算符 limits、间距命令（`\,`\`;`\quad`…）、`\text`、重音符；内部 PUA 标记（`\u{f0000}`–`\u{f0005}`）布局后清除
  - `MarkdownOptions.render_latex`（默认 true）+ `render_latex()` 公共 API
  - 关系/乘法/具名运算符间距与矩阵布局修正（`aa601d7ba`）一并落地
- **Markdown transform**：`MarkdownOptions.transform?: (md, available_width) -> String`；接通 T27 的 `register_markdown_transformer` 扩展 API（链式、context 含 `messageType`/`isStreaming`/`availableWidth`，作用于 assistant/user/thinking 渲染）
- **Mermaid**（`rpi-tui/src/mermaid.rs` 或子模块 + rpi 侧组件）：
  - 以 grok-build `mermaid.rs` 为蓝本移植：graph/flowchart、sequenceDiagram、stateDiagram → Unicode box-drawing；不支持类型回退带框原文
  - 适配层：`ratatui::style::Style`/`Line`/`Span` → rpi-tui 样式模型；`unicode-width` → rpi-tui 等价物
  - `markdown.mermaid` 设置三态（`off`|`final`|`streaming`，默认 streaming）接线
  - Apache-2.0 归因：源文件头注释 + grok-build 源 commit 哈希记录
- Marked `Token`/`Tokens` 类型导出等价面（rpi 用 comrak，对照 D-018 的 AST 映射决策扩展）

### Out

- mermaid 与 fullscreen 的集成渲染（随 T31 视口渲染自然获得，不单独做）
- grok-build 侧 `mermaid.rs` 的后续更新追踪（记录源哈希，属日常维护）

## 开发要点

- LaTeX 与 Mermaid 都是「对照表驱动」移植：先把上游测试（`latex.test.ts` 483 行、grok-mermaid fixtures）移植为失败测试，再实现到全绿
- Mermaid 双向校验：rpi 输出 vs grok-mermaid（TS）输出 vs grok-build 原作测试，三者同源同算法
- `render_latex` 与 transform 的交互顺序（transform 作用于源、latex 在渲染期）与上游 `markdown.ts` 核对

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] `latex.test.ts` 483 行测试意图全移植通过（含 pending 未闭合流式场景）
- [ ] Mermaid 三类图 golden（与 grok-mermaid 输出逐字节比对）；不支持类型回退带框原文
- [ ] `markdown.mermaid` 三态设置行为
- [ ] transform 链式 + 宽度感知 + messageType/isStreaming context 断言（扩展示例驱动）
- [ ] PUA 标记不出现在最终输出（泄漏断言）

## 门禁验收

通用门禁 G1–G7 全过（G4 重点：无 JS 引擎；G6 重点：Apache-2.0 归因与溯源注释）。

任务特有标准：

- [ ] 需求 R5.4/R3.3.1/R3.3.3 逐条核对表
- [ ] grok-build 源文件 commit 哈希与归因文本附验收记录

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| （待登记） | | |

## 验收记录

（按 gates §3 模板填写）
