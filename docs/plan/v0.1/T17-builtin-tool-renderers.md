# T17：内置工具渲染钩子（renderCall/renderResult）与语法高亮

- **状态**：已完成（2026-08-09 验收）
- **里程碑**：M8（Parity Freeze 后补漏）
- **依赖**：T12（`ToolExecutionComponent`）、T15（`ToolDefinition` trait 与扩展渲染三桥）
- **上游对照**：`packages/coding-agent/src/core/tools/{bash,read,edit,write,grep,find,ls}.ts`（各 `create*ToolDefinition` 的 `renderCall`/`renderResult`）+ `modes/interactive/components/tool-execution.ts:57,81-113`（内置定义解析与按 hook 合并）+ `utils/syntax-highlight.ts`、`modes/interactive/theme/theme.ts:1140-1289`（highlightCode/getLanguageFromPath/主题映射，**语义对位、ANSI 分段不对拍**，ADR-0008）@ `2efa728` / 0.82.1
- **需求章节**：§8（交互模式 TUI 行为，含 §8.6 Markdown「语法高亮」条）；`docs/parity-checklist.md` §4（TUI 行为）
- **预估**：0.25–0.35 人月（高亮走 syntect 库接入，ADR-0008；渲染基础设施 + 七渲染器为主体）

---

## 起因

2026-08-09 v0.1 测试发现：同一 `lscpu` 调用，上游 pi 渲染为 `$ lscpu` + 原始输出 + `Took 0.0s`，rpi 渲染为 `bash\n{ "command": "lscpu" }` 且无时
长行；write 调用上游渲染 `write <path>` + 前 10 行真实换行预览，rpi 渲染原始 JSON 转储。根因：rpi 只实现了内置工具的 `execute`（T06/T14），
未实现内置工具的 `renderCall`/`renderResult` 渲染钩子，七个内置工具全部落 `formatToolExecution` 默认回退
（`crates/rpi/src/modes/interactive/components/tool_execution.rs:521-534`）；`tool_definition` 只从扩展宿主取
（`interactive_mode.rs:1762` → `extension_renderers.rs:69-82`），内置工具恒 `None`。流式刷新接线（增量 parse → 每 delta MessageUpdate →
update_args/update_result → request_render）经逐段核查与上游一致，观感差异同样归于渲染层缺口。

该缺口在 `tool_execution.rs:8-13` 模块注释中原挂 T15，但 T15（W1–W8）交付范围实为扩展渲染三桥，未含内置渲染器，Parity Freeze 漏项。经裁
定：**不属于合理偏差，立本任务补齐**，不登记偏离。

**2026-08-09 两次范围决策（用户裁定）**：

1. 核查发现语法高亮为隐性依赖且全项目零登记——rpi 交互模式 `highlight_code: None`（`crates/rpi/src/modes/interactive/theme.rs:47`），
   write/read 高亮分支与 Markdown 代码块高亮均未交付（需求 §8.6）。裁定：T17 扩大范围包含语法高亮。
2. 高亮路线调研确认 Rust 生态无 hljs 功能等价库（syntect/tree-sitter token 边界均与 hljs 10.7.3 不同；逐字节只能手工移植 hljs 文法，数
   月级且 G4 禁 JS 执行；hljs 正则近似质量反低于 syntect）。裁定：**以 syntect 替代，高亮 ANSI 分段不与上游对拍**——立
   [`ADR-0008`](../../adr/0008-syntax-highlighting-syntect.md)，登记行为级偏离
   [`D-051`](./deviations/D-051-syntax-highlighting-syntect.md)。

## 目标

1. 移植上游七个内置工具的 `renderCall`/`renderResult`，并让 `ToolExecutionComponent` 具备内置工具定义解析与「扩展覆盖优先、缺 hook 继
   承内置」的按 hook 合并语义（tool-execution.ts:81-99），使交互模式下内置工具的调用行/结果行可见输出与上游逐字一致（消毒后口径）。
2. 以 syntect 交付语法高亮（ADR-0008）：`highlightCode`/`getLanguageFromPath`/`supportsLanguage` 语义对位，接线 Markdown 代码块与
   write/read 渲染器高亮分支；配色意图与上游一致（同组 theme 键），ANSI token 分段不对拍（D-051）。

## 范围

### In

1. **渲染基础设施**（对位 tool-execution.ts:57,81-113）：
   - 内置工具渲染定义注册表：`createAllToolDefinitions(cwd)[toolName]`（tool-execution.ts:57）对位——仅渲染用途的 `ToolDefinition` trait
     对象，按工具名查得；七个内置工具（bash/read/edit/write/grep/find/ls）各一份。
   - 按 hook 合并优先级：扩展定义有某 hook 则用扩展的，缺该 hook 则继承内置（`getCallRenderer`/`getResultRenderer`，
     tool-execution.ts:81-99）；`hasRendererDefinition` 在内置定义存在时即为 true（tool-execution.ts:101-103）；`renderShell` 合并
     （tool-execution.ts:105-113，扩展 `"self"` 优先）。
   - `ToolRenderContext` 补齐两块原 defer 项（`tool_execution.rs` 模块注释）：
     - **`lastComponent` 等效机制**：上游渲染器靠 `context.lastComponent` 复用组件实例并原地 `setText`（如 read.ts:335、edit.ts:427）；
       rpi 组件为 `Box<dyn Component>` 所有权模型，需设计组件复用/内部可变方案（设计细化阶段定稿）。
     - **渲染器可变 state**：上游 `state` 为每工具调用一份的任意对象（`BashRenderState{startedAt,endedAt,interval}`、edit 的
       `callComponent` 引用等）；rpi 现为 `serde_json::Value`，需承载类型化状态（trait 方法为 `&self`，考虑内部可变性）。
   - **invalidate 桥与定时器**：bash 渲染器 partial 期间 1s `setInterval(() => context.invalidate(), 1000)`（bash.ts:474-476），完成/出
     错清定时器（bash.ts:477-483）；rpi 侧经 `RenderHandle` 触发重绘，定时器生命周期随组件/结果终结回收。edit 的异步 diff 预览
     （`computeEditsDiff(...).then(... context.invalidate())`，edit.ts:381-389）同理，需 tokio 任务 + `RenderHandle`，并保留
     `requestKey` 竞态防护。
2. **bash 渲染器**（bash.ts:231-237 formatBashCall、239-319 rebuildBashResultRenderComponent、462-496 两钩子）：
   - renderCall：`$ <command>`（toolTitle 加粗）+ `(timeout Ns)` muted 后缀；command 缺失显示 `...`；无效参数 `invalidArgText`；
     `executionStarted` 首次记录 `startedAt`。
   - renderResult：输出 trim；非 partial 且截断时剥离 `[...Full output: ...]` 页脚（bash.ts:256-261）；折叠态 `BASH_PREVIEW_LINES=5` 行
     预览 + `... (N earlier lines, ctrl+o to expand)` 提示（`truncateToVisualLines` + `keyHint("app.tools.expand")`，宽度缓存失效语义
     bash.ts:272-294）；`[Full output: ... . Truncated: ...]` warning 行（lines/bytes 两种文案，bash.ts:297-312）；末尾
     `Elapsed`/`Took X.Xs`（`formatDuration` = `(ms/1000).toFixed(1)`，partial 为 Elapsed，bash.ts:314-318）。
3. **read 渲染器**（read.ts:334-350 及其 helpers）：renderCall 折叠态走紧凑分类 `formatCompactReadCall`（`getCompactReadClassification`，
   依 cwd 相对路径/图像等分类）、展开态 `formatReadCall`；renderResult `formatReadResult` 三态（read.ts:178-193，以源码为准）：
   折叠 + 非错误返回空；**折叠 + 错误**给 10 行预览 + `... (N more lines, …)` 提示（错误态禁用语言检测）；展开态全量。
   语言检测高亮分支（read.ts:184-190，高亮走 syntect，见第 7 项）；截断警告三分支（首行超 50KB / lines 限 / bytes 限）。
4. **edit 渲染器**（edit.ts:367-435 及其 helpers）：调用行组件含 diff 预览（`getRenderablePreviewInput` → argsKey 变化重置 →
   `argsComplete` 后异步 `computeEditsDiff` → `setEditPreview` + invalidate）；renderResult 复用 call 组件 preview（结果 `details.diff`
   回填）、`settledError` 翻转重建、`formatEditResult` 输出（Spacer + 缩进 1 的 Text）。
5. **write 渲染器**（write.ts:136-167 formatWriteCall、169-184 formatWriteResult、232-266 两钩子）：`write <path>` + 内容预览，折叠态
   10 行（write.ts:157）、`... (N more lines, M total, to expand)` 提示（write.ts:162）；语言检测命中时走高亮 + **增量高亮缓存**
   （write.ts:81-126，流式 delta 逐行追加、前缀复用）；`formatWriteResult` 非错误返回空（成功不渲染，write.ts:173-175）。
6. **grep / find / ls 渲染器**（grep.ts:375-…、find.ts:365-…、ls.ts:215-… 两钩子及 helpers）。
7. **语法高亮子系统（syntect，ADR-0008 / D-051）**：
   - `highlightCode` 语义对位（theme.ts:1160-1179）：`supportsLanguage` 门控（未识别语言整段 `mdCodeBlock` 着色回退，与上游同款）、异
     常回退原文；`ignoreIllegals` 语义由 syntect 引擎天然吸收（Sublime 文法容错解析，无 hljs 的 illegal 概念——注明于实现注释）。
   - `getLanguageFromPath`：47 扩展名 → 39 语言映射逐条移植（theme.ts:1184-1250），落到 syntect 语法名（含别名对齐）。
   - syntect 接入：fancy-regex 纯 Rust 后端（无 onig C 依赖，musl 静态兼容）；压缩语法包体积预算 ≤ 2MB（50MB 红线安全）；语言覆盖 ≥
     上游 39 语言锚。
   - scope → `Theme` 映射：锚定上游 `getCliHighlightTheme`（theme.ts:1140-1154）使用的同一组 theme 键（keyword/string/number/comment/
     title 等同族色系），保持「同类 token 同色系」；**ANSI token 分段与逐 token 配色不与上游 diff（D-051）**。
   - 接线：Markdown 主题 `highlight_code`（`theme.rs:47` None → 实装，assistant 消息代码块高亮，需求 §8.6）；write/read 渲染器高亮分支
     （第 3、5 项）；write 增量高亮缓存（第 5 项，syntect 逐行高亮 + 前缀复用语义不变）。
8. **过期注释清理**：`tool_execution.rs` 模块注释「built-in definitions do not exist (T17)」及 `lastComponent`/`invalidate` defer 表述随
   实现回写为事实陈述；`theme.rs:73` 钉死 `highlight_code.is_none()` 的测试随实装改写。
9. **对拍测试**：七工具调用行/结果行 VT 或快照对拍（消毒后逐字）；高亮 scope 映射与回退语义测试（**非** ANSI 逐字节）；合并优先级用
   例；定时器/异步预览行为用例。

### Out

- 扩展工具与自定义工具的渲染（T15 已交付，`w4_tool_render_override_and_inheritance` 等）。
- HTML 导出侧渲染（`renderedTools`/`ToolHtmlRenderer` 不移植，D-045 已登记关闭）。
- 工具执行逻辑本身（T06/T14 已交付，本任务零改动 `crates/rpi/src/tools/` 执行路径）。
- 未知工具（无内置定义且无扩展 hook）的 `formatToolExecution` 回退——保留现状，与上游一致。
- hljs 10.7.3 文法移植与高亮 ANSI 逐字节对拍——**明确不做**（ADR-0008；未来若有诉求须回写 ADR 另立任务）。

## 开发要点

- **先定基础设施再移植单工具**：`lastComponent`/state/invalidate 三件套的形状决定七个渲染器的移植方式，设计细化阶段先钉死（倾向：renderer
  state 以类型化 `dyn Any` 或每工具具体状态结构持有可复用组件，替代 JS 对象恒等）。
- **合并优先级一处实现**：`ToolExecutionComponent` 当前 `tool_definition: Option<Arc<dyn ToolDefinition>>` 单源，需改为「内置 + 扩展」
  双源按 hook 取（tool-execution.ts:81-99 逐行对位）；扩展覆盖无 hook 时继承内置渲染的回归用例必须保留（T15 W4 语义）。
- **高亮外壳先行**：`highlightCode`/`getLanguageFromPath`/scope 映射层与 syntect 初始化（惰性 `SyntaxSet`，首次使用加载）先钉死，write/
  read 高亮分支只调 `highlightCode`——渲染器移植不依赖高亮完成度，`supportsLanguage` 未命中自然落非高亮路径。
- **常数与文案逐值核对**：`BASH_PREVIEW_LINES=5`（bash.ts:204）、write/read 预览 10 行（write.ts:157、read.ts:187）、`formatDuration` 一
  位小数、`... (N earlier lines, …)` 与 `... (N more lines, M total, …)` 提示、`[Full output: … . Truncated: …]` 两种文案、read 紧凑分
  类文案、edit/write/grep/find/ls 各 helper 文案——全部锚定上游行号进验收附表。
- **可复用件已就位**：`visual_truncate.rs`（truncateToVisualLines 对位）、`keybinding_hints.rs`（keyHint 对位）、`RenderHandle`
  （invalidate/request_render）、`Container`/`Text`/`Spacer` 组件、`rpi-tui` Markdown 的 `highlight_code: Option<HighlightFn>` 槽位
  （markdown.rs:91）。bash 输出清洗走 `get_text_output` 既有路径（sanitize/strip-ANSI，render-utils.ts 对位已在组件内）。
- **定时器与 tokio**：Elapsed 刷新与 edit 异步预览都在渲染线程外完成计算、经 `RenderHandle` 回到 TUI 重绘；注意 abort/完成后不再触发重
  绘（上游 clearInterval/`previewArgsKey` 失配丢弃两语义）。
- **依赖引入纪律**：syntect 版本与 feature（fancy-regex 后端、无默认 onig）进 `coding-standards.md` 附录 A 依赖基线；语法包体积在 G4 体
  积复测时核对。

## 进度跟踪

- [x] 设计细化
- [x] 实现
- [x] 自测
- [x] 门禁验收
- [x] 文档回写

## 自测清单

- [x] bash：`$ lscpu` 调用行 + 原始输出 + `Took X.Xs` 结果行 VT/快照对拍；timeout 后缀；折叠 5 行预览 + expand 提示；截断 warning 两文
  案；partial 期间 `Elapsed` 每秒刷新、完成后定格 `Took` 且定时器回收
- [x] 流式刷新行为（2026-08-09 核查确认接线逐段对齐——`proxy.rs:627-629` 增量 parse、`agent_loop.rs:856-864` 每 delta MessageUpdate、
  `interactive_mode.rs:1295/1402` update_args/update_result + request_render——差异全在渲染层）：参数流式阶段 write 内容预览随 delta 增
  量增长（增量高亮缓存 write.ts:81-126）且折叠态钳制 10 行、组件高度不随内容无限增长；执行期 quiet 命令下 `Elapsed` 每秒刷新（上游
  setInterval 1000ms 语义，bash.ts:474-476）
- [x] read：折叠/展开两态调用行（紧凑分类 vs 完整格式）；renderResult 三态（折叠非错误→空、折叠错误→10 行预览+提示且禁高亮、展
  开→全量；read.ts:178-193 以源码为准）；图像/错误分支
- [x] edit：调用行 diff 预览（argsComplete 后异步出现）；结果回填 `details.diff`；`settledError` 翻转
- [x] write / grep / find / ls：调用行与结果行 helpers 文案逐字对拍
- [x] 语法高亮（syntect 口径）：`getLanguageFromPath` 47 扩展名逐条命中；`supportsLanguage` 门控与 `mdCodeBlock` 回退；scope→Theme 映射
  锚定 theme 键一致（同类 token 同色系断言）；Markdown 代码块高亮接线（36 个快照黄金中涉代码块者随高亮点亮更新）；**不做 ANSI 逐字节
  diff（D-051）**
- [x] 合并优先级：扩展仅覆盖 `renderCall` 时 `renderResult` 继承内置（及反向）；扩展 `renderShell:"self"` 优先
- [x] 未知工具回退 `formatToolExecution` 行为不变（现有 `fallback_renders_title_and_args` 等测试回归）
- [x] 常数/文案逐值核对表（验收附表，锚定上游行号）

## 门禁验收

通用门禁 G1–G7 全过（G4 含 syntect 引入后的 gnu/musl release 体积复测）。

任务特有标准：

- [x] 七内置工具调用行/结果行与上游渲染逐字一致（VT 投影或快照，ANSI 消毒后口径；每工具至少调用行 + 结果行两例；write/read 高亮分支
  以「结构/文案逐字 + 着色存在性」为口径，D-051）
- [x] 语法高亮：声明语言覆盖表（≥ 39 语言锚）逐语言有断言；scope→Theme 映射与回退语义测试全绿
- [x] 扩展覆盖继承回归：`w4_tool_render_override_and_inheritance` 全绿 + 新增「缺 hook 继承内置」用例
- [ ] 真机 smoke：`lscpu` 与 write `/tmp/rpi.txt` 两案例（本任务起因场景）渲染与上游一致——**留用户复核**（已有 VT 级等效用例覆盖，见验收记录 G3）

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| [D-051](./deviations/D-051-syntax-highlighting-syntect.md) | 语法高亮以 syntect 替代 hljs（高亮 ANSI 分段不与上游对拍；行为级，ADR-0008） | 已关闭 |

## 验收记录

- 验收日期：2026-08-09
- 验收人：实现者自证（逐条命令输出如下）；真机 smoke 两案例留用户复核
- G1 构建/静态检查：通过——`cargo build --workspace` ✓；`cargo clippy --workspace --all-targets -- -D warnings` 零告警 ✓；
  `cargo fmt --all -- --check` ✓
- G2 测试：通过——`cargo test --workspace` **3900 passed, 0 failed**（84 个测试目标；live 默认跳过）。其中：七渲染器单元测试 67 过
  （bash/edit/find/grep/ls/read/write + 注册表/render_utils）；`highlight` 名称过滤 18 过（`core::highlight` 10 + 高亮接线 8）；新增集成
  `tool_renderers_test` 3 过（lscpu 起因场景、Elapsed ticker 启停、流式 write 钳制+成功不渲染）；扩展继承回归 `extension_host_w4_test`
  11 过（含新增 `w4_extension_missing_hook_inherits_builtin_renderer` 双向合并用例）；`tool_renderers` 套件连跑 7 次无 flake
  - **修改既有测试期望清单**（T17 落地使 `read` 获得内置渲染器，以下 5 个原验证「通用回退路径」的用例改用具名 `custom-tool`
    （无内置定义）继续验证回退，属 T17 预期行为变化而非回归）：
    1. `tool_execution.rs::tests::{make_component,fallback_renders_title_and_args,result_updates_background_by_state,text_output_falls_back_to_image_indicators}` —— 工具名 `read`→`custom-tool`；旧期望「read 走回退」→ 新期望「custom-tool 走回退」（上游 tool-execution.ts:57 内置定义注册表）。
    2. `interactive_mode.rs::tests::tool_execution_start_update_end_lifecycle` —— 同上改 `custom-tool`；`read` 折叠成功结果渲染为空本是上游三态语义（read.ts:178-179）。
    3. `interactive_mode.rs::tests::render_initial_messages_builds_chat_from_entries` —— 同上。
    4. `snapshots.rs::tool_execution_fallback` + 黄金 `tests/snapshots/tool_execution_fallback.snap` —— 工具名改 `custom-tool` 后
       `RPI_UPDATE_SNAPSHOTS=1` 再生；黄金 diff 仅标题行 `read`→`custom-tool`，回退 JSON 形状不变。
    5. `write.rs::tests::result_error_renders_error_text`（本任务新增，初版断言 `starts_with('\n')`）——`Text` 组件行补空格到全宽，
       前导 `\n` 渲染为空格填充的空行；断言修为「首行 trim 后为空」。
    6. `tool_renderers_test.rs::bash_elapsed_ticker_ticks_during_partial_and_settles`（本任务新增）——sleep 1.3s 后结算时长为
       `Took 1.Xs`，初版误断言 `Took 0.`，修为 `Took 1.`。
    7. 六处 SGR-only 的测试 `strip_ansi`（bash/edit/find/grep/write/tool_execution）与 `rpi-test-support::vt::strip_ansi` —— 补 OSC 8
       剥离，消除 `ls` 超链接能力用例对并行用例的进程级污染（capability cache 全局可变）。
- G3 对拍：通过（TUI 渲染行为类）——常数/文案逐值核对表（锚定上游行号，均已进各渲染器测试逐字断言）：

  | 项 | 上游锚 | rpi 位置 |
  |----|--------|----------|
  | `BASH_PREVIEW_LINES=5` | bash.ts:204 | `tool_renderers/bash.rs:44` |
  | `formatDuration`=`(ms/1000).toFixed(1)s`、`Elapsed`/`Took` | bash.ts:228,315 | `bash.rs` `format_duration`/`render`:245 |
  | 截断 warning 两文案（lines/bytes） | bash.ts:297-312 | `bash.rs:356-379`（测试逐字断言） |
  | `WRITE_PARTIAL_FULL_HIGHLIGHT_LINES=50`、预览 10 行、hint 文案 | write.ts:63,157,162 | `write.rs:40,43,259-266` |
  | 成功结果不渲染（`formatWriteResult` 非错误 → 空） | write.ts:173-175 | `write.rs` `format_write_result` |
  | read renderResult 三态、错误预览 10 行禁高亮、hint 文案 | read.ts:178-193 | `read.rs`（19 测试覆盖三态） |
  | grep/find/ls 折叠 15/20/20 | grep.ts:106、find.ts:103、ls.ts:80 | `grep.rs:26`、`find.rs:26`、`ls.rs:26` |
  | edit `renderShell:"self"` | edit.ts:310 | `edit.rs` `render_shell` |
  | read 紧凑分类资源文件名集 | read.ts:42（4181f66） | `read.rs:43-47`（含 `AGENTS.override.md`） |

  基线说明：T17 立项锚 `2efa728`，当前 `external/pi` HEAD 为 v0.11 基线 `4181f66`；逐文件 diff 确认七个工具的 `renderCall`/
  `renderResult` 在两锚间唯一实质差异为 read 紧凑分类新增 `AGENTS.override.md`（rpi 与新基线一致），其余为 system-prompt 重构
  （渲染无关）。起因两场景已有 VT 级等效用例：`tool_renderers_test.rs::bash_lscpu_case_matches_upstream_shape`（`$ lscpu` 调用
  行 + 原始输出 + `Took X.Xs`，无 JSON 转储）与 `write_streaming_preview_grows_but_stays_clamped`（流式增量、钳制 10 行、无字面
  `\n`、成功不渲染 `Successfully wrote`）；真机 smoke 留用户复核。
- G4 红线：通过——`external/pi` 无改动（porcelain 为空，HEAD=`4181f66`，v0.11 基线）；`cargo tree -i onig` 无匹配（syntect
  fancy-regex 纯 Rust，无 C/onig、无 JS 引擎）；语法包经 `build.rs` 预编译校验 + zlib 内嵌 792 KiB（≤2MB 预算）；gnu release 体积
  复测 34,035,032 B ≈ 32.5 MiB（50MB 红线内）；**musl 体积复测跳过**（当前工具链未装 `x86_64-unknown-linux-musl` target，用户裁
  定跳过，留待环境具备后补测）。
- G5 线格式：不适用（本任务不触碰 JSONL/RPC/settings/models.json 等线格式类型）。
- G6 文档同步：通过——渲染器/高亮代码均带溯源注释（上游文件+行号）；`coding-standards.md` 附录依赖基线已登记 syntect；
  `01-requirements.md` §8.6、`parity-checklist.md` §5、ADR-0008、D-051、本任务文件均已回写；`tool_execution.rs` 模块注释与
  `theme.rs` 高亮槽位注释随实现改写为事实陈述。
- G7 偏离闭环：通过——D-051（行为级，ADR-0008）状态置「已关闭」，登记表已同步。
- 结论：**通过**（真机 smoke 两案例与 musl 体积复测为环境依赖项，已按上文标注留待复核/补测，不阻塞验收）

### 验收后审查修复（2026-08-09 全面审查，同日完成）

审查复核 G1–G7 通过，另发现 3 项移植缺陷并已修复（均属实现 bug，非偏离，不登记 D 编号）：

1. **M1：`LANGUAGE_ALIASES` 与 vendored hljs 10.7.3 事实不符**——原表仅 6 语言 12 别名且注释/测试断言「`ts` 不是
   typescript 别名」，实际 vendored 全量语言包约 90 语言带别名（`typescript.js:691 aliases:['ts','tsx']` 等）。影响：Markdown
   围栏 ```ts/```js/```sh/```rs 等在上游高亮、rpi 落 `mdCodeBlock` 平色。修复：拆为 `CANONICAL_NAME_REDIRECTS`（6 条）+
   `LANGUAGE_ALIASES`（补齐至 105 条，逐条对照 vendored `aliases` 数组，目标语法名对照 bat 198 语法集全名清单）；`tsx`→
   `TypeScriptReact`、`jsp`→`Java Server Page (JSP)`、`html/xhtml`→`HTML`（hljs 无 `html` 注册名，经 xml 别名解析语义一致）、
   `toml` 保留直解 `TOML`（hljs 映射 ini，D-051 范围内取舍）；无 bat 对应物的语言（haxe/smalltalk/brainfuck 等）与 fancy 不
   兼容的 `ps/ps1/arm` 维持不支持并注释。新增自校验测试 `every_alias_resolves_in_the_syntax_set`（每别名可解析 + 无规范名遮
   蔽），反转 `supports_language_positive_and_negative` 的 `ts` 断言。read/write 渲染器不受影响（走 `getLanguageFromPath`
   规范名）。
2. **L1：`get_render_shell` 合并语义 ≠ 上游 `??` 链**——trait `render_shell` 改返 `Option<RenderShell>`（`None`=上游
   `undefined`），合并修正为 `ext ?? builtin ?? "default"`：扩展显式 `"default"` 也赢（旧实现仅 `"self"` 赢，扩展对 edit 显式
   default 会错误落回内置 self 壳）。六个无 `renderShell` 的内置定义改返 `None`，edit 返 `Some(Self_)`；新增
   `render_shell_merge_follows_upstream_nullish_chain` 用例。
3. **L2：edit 空 path 预览**——上游 `if (!path) return null`（edit.ts:189，空串 falsy），rpi 原接受空串（注释与上游相反）。
   修复为空串返回 `None` 并反转对应断言。

未修（审查已确认可留）：L3 `formatDuration` 平局舍入（JS `toFixed` 取大 vs Rust `{:.1}` 取偶；`Instant` 亚毫秒测量下恰落
.05s 边界实际不可达）；N3 扩展渲染失败回退路径差异（T15 既有契约，模块注释已文档化）。

修复后复跑：`highlight` 18、`tool_renderers` 67、`tool_execution` 8、`extension_host_w4_test` 11、`tool_renderers_test` 3
全绿；全仓门禁（fmt/clippy/`cargo test --workspace`）见 `index.md` 变更记录。
