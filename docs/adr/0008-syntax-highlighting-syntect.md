# ADR-0008：语法高亮以 syntect 替代 hljs 逐字节移植（高亮 ANSI 分段不与上游对拍）

- **状态**：已采纳
- **日期**：2026-08-09
- **关联**：[`01-requirements.md`](../01-requirements.md) §8.6（Markdown「语法高亮」条）、
  [T17](../plan/v0.1/T17-builtin-tool-renderers.md)、偏离 D-051、ADR-0002（基线决策，
  含 musl 静态与 50MB 体积红线）

## 背景

T17（内置工具渲染钩子）核查发现 rpi 语法高亮完全未交付：交互模式 Markdown 主题
`highlight_code: None`（`crates/rpi/src/modes/interactive/theme.rs:47`），write/read
渲染器的高亮分支与 Markdown 代码块高亮（需求 §8.6）均缺位。上游实现为自研
`utils/syntax-highlight.ts`（hljs `<span>` HTML → ANSI 渲染器）+ highlight.js 10.7.3
（coding-agent package.json:57，`theme.ts:1160-1179` `highlightCode`）。

2026-08-09 初始裁定为「逐字节对拍上游 ANSI 输出」。随后对 Rust 生态的调研结论：

- **无 hljs 功能等价库**。候选三家 tokenization 语义均与 hljs 不同：
  - syntect：Sublime `.sublime-syntax` 正则状态机，scope 体系 `keyword.control.rust` 式；
  - tree-sitter-highlight / arborium：真解析器，capture 更细更准；
  - 三者 token 边界与 hljs 均不一致，即使加 scope→hljs class 映射层再上同一主题色，
    ANSI 序列分段位置仍处处不同，逐字节 diff 不可达。
- 逐字节唯一可行路线是**手工移植 hljs 10.7.3 文法**（fancy-regex 可覆盖其正则特性）：
  每语言数天、声明矩阵 9–39 种语言，总计数月级；且 G4 红线禁 JS/TS 执行能力，不能内嵌
  JS 引擎直接跑 hljs。
- 值得注意：hljs 10.7.3 为 2021 年版本，其正则近似高亮质量一般（会错判）；syntect /
  tree-sitter 的高亮质量实际优于它。逐字节买到的是「与上游分段完全一致」，不是更好的
  高亮。

## 决策

语法高亮以 **syntect** 实现，放弃高亮 ANSI 分段与上游的逐字节对拍（按行为级偏离登记
D-051）：

- **选 syntect 的理由**：纯 Rust（fancy-regex 后端，无 onig C 依赖）、musl 静态兼容、
  压缩语法包体积小（~1–2MB，对 50MB 红线安全）、API 久经考验（bat/delta/zola 在用）。
  tree-sitter 系（含 arborium）因每语言 grammar 编译进二进制的体积风险（个别语法源文
  件达数十 MB 级）与 arborium 项目过新（2025-11 发布，API churn 风险）排除。
- **应用面**：write/read 渲染器高亮分支（write.ts:151-154、read.ts:184-190 对位）、
  Markdown 代码块（rpi-tui `highlight_code` 槽位实装）、write 增量高亮缓存。
- **对拍口径**：T17 其余维度不变——渲染结构、文案、钳制行数、计时器、流式增量行为仍
  逐字对拍；仅「代码着色的 ANSI token 分段与逐 token 配色」不与上游 diff。
- **配色意图对齐**：syntect scope → rpi `Theme` 的映射锚定上游 `getCliHighlightTheme`
  （theme.ts:1140-1154）使用的同一组 theme 键，保持「同类 token 同色系」的视觉一致。
- **语言支持语义**：`supportsLanguage` 对位改为「syntect 语法集可识别」；覆盖目标 ≥
  上游 `getLanguageFromPath` 可达的 39 语言（theme.ts:1184-1250）。**语法集用 bat 的
  198 语法包**（`syntect-assets`，T17 W2 落地时修正：syntect 5.3 发布版内嵌 dump 是约
  2019 年的 Sublime 默认集，缺 TypeScript/TOML/Dockerfile 等 16 种，不达标）；build 期
  对每条正则做 fancy-regex 穷举预编译校验后压缩内嵌（792 KiB）。未识别语言维持上游同
  款回退（整段 `mdCodeBlock` 着色）。

## 后果

- 高亮接入从「数月级文法移植」降为「数天级库接入」；T17 预估随之下调。
- v0.1 发布说明需注明：语法高亮的着色分段与上游 pi 不一致（结构/文案一致）。
- `docs/parity-checklist.md` §5「有意差异」表登记本项；T17 门禁的高亮条目改为「scope
  映射与主题键锚定一致 + 回退语义一致」，不做 ANSI 逐字节 diff。
- 若未来出现高亮逐字节对拍诉求，须回写本 ADR 并另行评估 hljs 文法移植任务（届时参考
  本 ADR 背景节的成本结论）。
