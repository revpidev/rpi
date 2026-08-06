# D-018：Markdown 解析器 comrak 替代 marked（依赖替代型）

- **状态**：已回写
- **关联任务**：T12（S3 Markdown 组件 + S7b 登记）
- **级别**：实现细节偏离（渲染行为以快照黄金 + marked 产出对拍兜底，T12 计划「关键决策 1」已定）
- **发现日期**：2026-08-05

## 原文档约定

- 文档与章节：`docs/01-requirements.md` §8.6（Markdown：marked 等价解析、`trimPartialClosingFences()` 流式 fence 稳定、code block border + indent、主题 20+ 样式函数）、`docs/02-design.md` §5（组件清单 Markdown 标注「marked 等价」）、`docs/plan/v0.1/T12-interactive-mode.md`（关键决策 1：marked → comrak，属依赖替代型偏离）
- 原文约定：`marked` 解析器语义等价（token 树、`token.raw`、严格删除线、任务列表判定）

## 实际实现与偏离原因

- 解析器为 **comrak 0.54**（`default-features=false` + GFM 扩展），替代上游 `marked@18.0.5`（无 Deno/Node 嵌入，ADR-0001 红线；comrak AST 树形结构与 marked token 树最接近，GFM 表格/删除线/任务列表齐备）。
- AST 对应方案（`crates/pir-tui/src/components/markdown.rs` 头部注释）：
  - `token.raw` 由 comrak `sourcepos` 字节区间**切片源码**还原（marked 的 raw 语义）；
  - marked 的 `space` token（块间空行）由源码空隙**合成**（CommonMark AST 丢弃空行）；
  - 严格删除线语义与 marked `StrictStrikethroughTokenizer` 对拍验证（`~~ foo~~`、`~~foo ~~`、`~~foo~~~`、`~~~foo~~~`、`a~~b~~c` 等），`is_marked_strikethrough` 校验对齐 marked 正则；
  - 任务列表判定对齐 marked 正则（`[x]`/`[ ]` 前缀）。
- 主题回调为 `Box<dyn Fn(&str) -> String + Send + Sync>` 字段 + `Arc<MarkdownTheme>` 共享（显式注入惯例）；渲染缓存与默认样式前缀缓存用 `RefCell`（`render(&self)` 约束）。
- 3 个 xterm 集成用例由字节级断言改**输出级断言**（comrak 与 marked 的源码切片在个别空白/实体细节上不可逐字节对齐）。

## 已知残留边缘差异（3 条）

1. `~~\~~x~~`（开位反斜杠转义波浪号）：marked 剥离转义后触发删除线；comrak 保持纯文本。
2. `~~&amp;~~`（删除线内 HTML 实体）：marked 不解码实体（渲染 `&amp;`）；comrak 解码（渲染 `&`）。
3. （附带）链接 fallback 比较用渲染后纯文本替代 marked 的原始源文本（`token.text`），仅 exotic 链接体（`[a\*b](a*b)` 等）时 `(url)` 后缀判定不同。

## 影响面

TUI 行为（渲染输出）；已用快照黄金 + 对拍测试兜底，三条已知差异登记在案。

## 处置

- **回写位置**：`docs/02-design.md` §5（Markdown 组件标注 comrak 替代）、§12（模块映射表）、`docs/01-requirements.md` §8.6（marked 等价 → comrak 等价 + 差异注记）
- **回写日期**：2026-08-05
- **ADR**：不需要（T12 计划已决策，实现细节偏离）
