# D-051：语法高亮以 syntect 替代 hljs（高亮 ANSI 分段不与上游逐字节对拍）

- **状态**：已关闭（2026-08-09 T17 验收）
- **关联任务**：T17
- **级别**：行为级偏离（ADR-0008）
- **发现日期**：2026-08-09（T17 立项核查）

## 原文档约定

- 需求：`docs/01-requirements.md` §8.6 Markdown 条「语法高亮」；复刻总原则（需求 §1.2
  行为对拍）隐含与上游渲染输出一致。
- 上游实现：`packages/coding-agent/src/utils/syntax-highlight.ts`（自研 hljs HTML→ANSI
  渲染器）+ highlight.js 10.7.3 + `modes/interactive/theme/theme.ts:1160-1179`
  `highlightCode`、:1184-1250 `getLanguageFromPath` @ 0.82.1（2efa728）。

## 实际实现与偏离原因

rpi 语法高亮完全未交付（`crates/rpi/src/modes/interactive/theme.rs:47`
`highlight_code: None`），T17 立项补齐。初始裁定逐字节对拍，调研后确认 Rust 生态无
hljs 功能等价库：syntect（Sublime 文法）、tree-sitter 系（真解析）的 token 边界与
hljs 10.7.3 均不一致，scope 映射层无法对齐 ANSI 分段；逐字节唯一路线为手工移植 hljs
文法（数月级，且 G4 禁 JS 执行不能内嵌引擎），成本与价值不匹配（hljs 10.7.3 为 2021
年正则近似文法，高亮质量低于 syntect/tree-sitter）。

经 ADR-0008 裁定：以 **syntect**（fancy-regex 纯 Rust 后端）实现语法高亮——write/read
渲染器高亮分支、Markdown 代码块（`highlight_code` 槽位）、write 增量高亮缓存；syntect
scope → `Theme` 映射锚定上游 `getCliHighlightTheme` 同一组 theme 键保持配色意图一致；
`supportsLanguage` 语义改为「syntect 可识别」（覆盖 ≥ 上游 39 语言锚）。渲染结构、文
案、钳制、计时等其余维度仍逐字对拍。

## 影响面

TUI 行为：write/read 高亮分支与 Markdown 代码块的**代码着色 ANSI token 分段与逐
token 配色**与上游不一致（同类 token 同色系）；渲染结构/文案/钳制/计时不受影响。
协议 / session 格式 / 扩展 API：无影响。

## 处置

- **回写位置**：T17 任务文件（范围第 7 项、自测清单、门禁）；`01-requirements.md`
  §8.6 注记；`docs/parity-checklist.md` §5 有意差异表；登记表本行
- **回写日期**：2026-08-09
- **ADR**：ADR-0008
