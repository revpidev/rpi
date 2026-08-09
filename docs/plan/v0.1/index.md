# Pir 开发计划 v0.1 — 任务索引

> 本计划基于 [`../../docs/01-requirements.md`](../../01-requirements.md)、
> [`../../docs/02-design.md`](../../02-design.md)、[ADR-0001](../../adr/0001-extension-and-config-dir.md) /
> [ADR-0002](../../adr/0002-baseline-decisions.md) / [ADR-0003](../../adr/0003-coverage-review-scope-decisions.md)、
> [`coding-standards.md`](../../coding-standards.md) 制定。
> 上游对照：`external/pi` @ `2efa728` / 0.82.1（见 [`UPSTREAM.md`](../../../UPSTREAM.md)）。
>
> 版本：v0.1　创建：2026-07-28　最近修订：2026-07-29（覆盖度审查后全面修订，见 §6）

---

## 1. 使用说明

- 每个任务一个文件（`TNN-<slug>.md`），可**独立开发、独立自测、独立验收**。
- 任务完成后必须通过门禁验收：通用门禁见 [`gates.md`](./gates.md)，任务特有验收标准见各任务文件「门禁验收」一节。**未通过门禁的任务不得标记为已完成。**
- 实现过程中与原始文档（需求 / 设计 / ADR / 编码规范）产生的任何偏离，必须：
  1. 登记到 [`deviations/`](./deviations/)（一事一记，流程见 [`deviations/README.md`](./deviations/README.md)）；
  2. **回写**到原始文档对应位置，保持文档与实现一致；
  3. 行为级偏离（影响对拍契约）不允许直接落地，须先立 ADR。
- 偏离未闭环（登记 + 回写）的任务，门禁验收不通过。

## 2. 进度标识约定

任务状态（各任务文件头部「状态」字段，本索引表同步维护）：

| 状态 | 含义 |
|------|------|
| `未开始` | 依赖未就绪或未排期 |
| `进行中` | 已开始实现 |
| `待验收` | 实现与自测完成，等待门禁验收 |
| `已完成` | 门禁验收通过，偏离已闭环 |
| `受阻` | 被外部条件阻塞（在任务文件记录阻塞原因） |

任务内部进度用五个阶段复选框跟踪：**设计细化 → 实现 → 自测 → 门禁验收 → 文档回写**。

## 3. 任务索引

| ID | 任务 | 里程碑 | 依赖 | 状态 | 验收日期 |
|----|------|--------|------|------|----------|
| T01 | [工程骨架与类型契约锁定](./T01-workspace-skeleton.md) | M0 | — | 已完成 | 2026-07-30 |
| T02 | [对拍基建与关键技术验证](./T02-parity-harness.md) | M0 | T01 | 已完成 | 2026-07-30 |
| T03 | [pir-ai 核心协议（Anthropic + OpenAI 系）](./T03-pir-ai-core-protocols.md) | M1 | T01 | 已完成 | 2026-07-30 |
| T04 | [pir-ai Auth 基础](./T04-pir-ai-auth.md) | M1 | T03 | 已完成 | 2026-07-31 |
| T05 | [pir-agent：agent_loop 与 Agent](./T05-pir-agent-loop.md) | M2 | T01、T02 | 已完成 | 2026-08-01 |
| T06 | [内置四工具与 ToolContext](./T06-builtin-tools.md) | M2 | T05 | 已完成 | 2026-08-01 |
| T07 | [SessionManager（JSONL 树）](./T07-session-manager.md) | M3 | T01、T05 | 已完成 | 2026-08-03 |
| T08 | [Compaction](./T08-compaction.md) | M3 | T07 | 已完成 | 2026-08-03 |
| T09 | [Settings 与资源加载](./T09-settings-resources.md) | M3 | T01 | 已完成 | 2026-08-03 |
| T16 | [pir-agent harness 层](./T16-agent-harness.md) | M3 | T05、T07、T08 | 已完成 | 2026-08-06 |
| T10 | [Headless 模式：print / json / rpc](./T10-headless-modes.md) | M4 | T03、T04、T05、T06、T07、T08、T09 | 已完成 | 2026-08-04 |
| T11 | [pir-tui 核心引擎](./T11-pir-tui-core.md) | M5 | T01 | 已完成 | 2026-08-05 |
| T12 | [pir-tui 组件与 Interactive 模式](./T12-interactive-mode.md) | M5 | T10、T11 | 已完成 | 2026-08-06 |
| T13 | [全量 Provider 与 OAuth](./T13-providers-oauth.md) | M6 | T03、T04 | 已完成 | 2026-08-07 |
| T14 | [可选工具 / Packages / Trust / Export / llama / 更新](./T14-packages-trust-export.md) | M7 | T09、T10 | 已完成 | 2026-08-07 |
| T15 | [扩展宿主 L0+L1 与 Parity Freeze](./T15-extension-host.md) | M8 | T02（spike）、T10、T12 | 已完成 | 2026-08-09 |
| T17 | [内置工具渲染钩子（renderCall/renderResult）与语法高亮](./T17-builtin-tool-renderers.md) | M8 | T12、T15 | 已完成 | 2026-08-09 |

## 4. 里程碑映射与并行建议

```
M0: T01 → T02
M1: T03 → T04          ┐ 并行
M2: T05 → T06          ┘
M3: T07 → T08,  T09,  T16   ┐ 与 M5（T11→T12）尽早重叠
M4: T10                     ┘
M5: T11 → T12          （TUI 为硬性交付，ADR-0002 §3，不可压后）
M6: T13                （与 M3–M5 并行）
M7: T14
M8: T15                （Parity Freeze）
    T17                （冻结后补漏：内置工具渲染钩子）
```

并行口径沿用设计文档 §11：T03∥T05；T07–T10 与 T11–T12 尽早重叠；T13∥T07–T12；T16 在 T07/T08 就绪后插入，可与 T09 并行。

## 5. 目录结构

```
docs/plan/v0.1/
├── index.md            # 本文件：任务索引与进度跟踪
├── gates.md            # 门禁验收标准与流程（所有任务共用）
├── deviations/         # 偏离登记目录（一事一记 + 登记表）
│   ├── README.md       # 偏离管理流程
│   └── TEMPLATE.md     # 偏离记录模板
└── TNN-*.md            # 任务文件（T01–T17）
```

## 6. 变更记录

| 日期 | 变更 | 说明 |
|------|------|------|
| 2026-08-09 | T17 审查修复 | 全面审查复核 G1–G7 通过，另修 3 项移植缺陷（实现 bug，非偏离）：M1 `LANGUAGE_ALIASES` 与 vendored hljs 10.7.3 事实不符（原 12 别名且「ts 非别名」断言错误；vendored 全量包约 90 语言带别名）——拆 `CANONICAL_NAME_REDIRECTS`(6) + `LANGUAGE_ALIASES`(105，逐条对照 vendored aliases 数组，目标名对照 bat 198 语法集 dump 校准；tsx→TypeScriptReact、jsp→JSP、html/xhtml→HTML、toml 保留直解 TOML），Markdown 围栏 ```ts/```sh/```rs 等恢复高亮；L1 `render_shell` trait Option 化，`get_render_shell` 修正为上游 `ext ?? builtin ?? "default"`（扩展显式 default 也赢）；L2 edit 空 path 不渲染预览（edit.ts:189 falsy）；新增自校验/合并语义用例 2 个；L3（toFixed 平局舍入，不可达）/N3（T15 既有契约）留待；测试 3902 passed / 0 failed |
| 2026-08-09 | T17 验收完成 | 内置工具渲染钩子与语法高亮完成（W1–W9）：渲染基础设施（内置定义注册表、扩展/内置按 hook 双源合并、`RendererStateSlot` 类型化状态、invalidate 桥 + bash 1s Elapsed ticker、edit 异步 diff 预览）；七渲染器移植（bash `$ cmd`+`Took X.Xs`/5 行预览/截断 warning 两文案，read 三态+紧凑分类，edit diff 预览+renderShell self，write 10 行钳制+增量高亮缓存+成功不渲染，grep/find/ls 折叠 15/20/20）；syntect 高亮子系统（bat 198 语法集 fancy-regex 纯 Rust、build.rs 预编译校验 + zlib 内嵌 792KiB、58 扩展名映射、Markdown 代码块接线）；验收修复：write 测试 Mutex 自死锁、OSC 8 超链接 strip 六处 + vt::strip_ansi（消 capability cache 并行污染）、5 个旧回退用例改 `custom-tool`（read 获内置渲染器后回退路径需具名未知工具）；基线核对：七工具渲染钩子 `2efa728`→`4181f66` 唯一实质差异为 read 紧凑分类 `AGENTS.override.md`（已与新基线一致）；测试 3900 passed / 0 failed（84 目标）；gnu release 34,035,032B ≈32.5MiB < 50MB（musl 复测跳过，工具链缺 target，用户裁定）；偏离 D-051 置「已关闭」（ADR-0008）；真机 smoke 两案例留用户复核 |
| 2026-08-09 | T17 高亮路线定稿 | 高亮移植调研：Rust 生态无 hljs 功能等价库（syntect/tree-sitter token 边界均异），逐字节只能手工移植 hljs 10.7.3 文法（数月级、G4 禁 JS 执行、hljs 正则近似质量反低）。裁定 syntect 替代（fancy-regex 纯 Rust、musl 兼容、体积 ≤2MB），高亮 ANSI 分段不对拍——立 ADR-0008、登记 D-051（行为级，已回写）；T17 预估下调至 0.25–0.35 人月 |
| 2026-08-09 | T17 扩大范围 | 核查发现语法高亮为 T17 隐性依赖且全项目零登记：pir 交互模式 `highlight_code: None`（`theme.rs:47`），write/read 高亮分支与 Markdown 代码块高亮（需求 §8.6）均未交付；上游实为自研 `syntax-highlight.ts` + hljs 10.7.3（非 cli-highlight）。裁定：T17 扩范围含高亮移植，ANSI 输出逐字节对拍（声明语言矩阵内）；hljs 语法 Rust 重实现（G4 禁 JS 执行），fixtures 黄金分波；预估上调至 0.4–0.6 人月 |
| 2026-08-09 | 新增 T17 | v0.1 测试发现内置工具渲染缺口：`lscpu` 调用上游渲染 `$ lscpu` + `Took 0.0s`，pir 渲染 `bash\n{json}` 无时长行——pir 未实现七内置工具（bash/read/edit/write/grep/find/ls）的 `renderCall`/`renderResult`，全部走 `formatToolExecution` 回退；原挂 T15 的注释（`tool_execution.rs:8-13`）实为 Parity Freeze 漏项（T15 范围为扩展渲染三桥）。裁定非合理偏差、不登记偏离，立 T17 补齐（M8，依赖 T12/T15） |
| 2026-08-09 | T15 验收完成 | 扩展宿主 L0+L1 与 Parity Freeze 完成（W1–W8 八波次）：`pir-ext-host` 宿主核心（注册表同名冲突规则全量/发现加载/串行 emit/错误总线）、33 事件全接线（含 tool_call 改参穿线、before_provider_request/headers 链式、user_bash、project_trust、resources_discover）、24 API 动作绑定、28 UI 方法三桥（Interactive/Rpc 9+18 降级/Null）、三级 Context 与 stale 失效、L1 wasmtime 宿主（ABI v1 + capability 沙箱 + fuel）与 `pir-ext-sdk`、L0 abi_stable 动态库插件（`native` manifest 字段）、安装管理 e2e 与启动管线 packages→loader 接线、llama 迁移真扩展（D-047 关闭）、switchSession 异步信任选择器（ADR-0006/D-044 关闭）、`--wasm-smoke` 钩子移除；Parity Freeze：`docs/parity-checklist.md` 四类清单（扩展 API 88 条逐条锚点）+ session 互通终验补全栈 faux 续跑测试 + 需求 §1.2 五条总核对全过；gnu release 复测 32,125,208B ≈30.6MiB < 50MB；偏离 D-048~D-050 登记回写（行为级三缺口 → ADR-0007）、`02-design.md` §7.2/§13 定稿回写、README 状态节同步；测试 3815 passed / 0 failed |
| 2026-08-07 | T14 验收完成 | 可选工具 / Packages / Trust / Export / llama / 更新完成（W1–W7 七波次）：grep/find/ls Rust 原生实现（ignore/globset，rg 15/fd 10.4 实机交叉验证）；package-manager 核心 + install/remove/list 子命令；update 全目标（互斥矩阵/--force/release note/self 更新/并发 4）+ config 子命令；trust 产品化（完整优先级链 + 启动弹窗 + 两阶段加载）；HTML/JSONL export（模板资产逐字节内嵌对拍）+ gist share；endpoint/telemetry 配置化（三个 PIR_*_URL + settings 三键 + "off" 零请求）；llama.cpp 集成（/llama + /login llama.cpp + HF 搜索下载）；gnu release 29MB 发布物 smoke 全过（musl 本次豁免，用户决策）；终审修复 7 项（edit 测试目录竞态、display() 凭据脱敏、/share 临时文件串号等）；偏离 D-039~D-047 登记回写（D-039 第 1 条 → ADR-0005、D-044 → ADR-0006 留 T15 关闭，其余验收后置「已关闭」）；`01-requirements.md` §3.2/§4.5/§7.6/§7.8/§10、`02-design.md` §6.1/§8/§12 同步 |
| 2026-08-07 | T13 验收完成 | 全量 Provider 与 OAuth 完成（W1–W7 七波次）：7 个新适配器（pi-messages/mistral/google-generative-ai+google-shared/azure/google-vertex+ADC/bedrock 手写 SigV4+event-stream/codex WS 含缓存续传与 zstd）、38 工厂 + 目录生成管线（37 份 vendored JSON、1153 模型、compat 全量）、6 OAuth 流程 + load registry、images 子系统、handoff/deferred tools 收尾（修复 last-wins 语义偏差）、远程 catalog overlay + `pir update --models`；需求 §5 映射表 55 条、上游测试移植清单 114 文件逐条标注、live smoke 无 key 全豁免；偏离 D-021~D-038 登记回写（D-029/030/031 已关闭）；`02-design.md` §3.3–3.6/§12/§13 同步（WS 状态机开放项定稿） |
| 2026-08-06 | T12 验收完成 | 用户真机 smoke 人工验证通过（本机 + tmux 两种环境：启动、提问、streaming、abort、快捷键、退出恢复），T12 置「已完成」，验收日期 2026-08-06 |
| 2026-08-06 | T16 验收通过 | pir-agent harness 层完成：types（22 事件/错误族/trait）/agent_harness（phase 机、三队列、持久化屏障、失败重放）/session 门面与四存储实现/env/tools/resources/utils/proxy 全量移植；上游 14 个测试文件意图移植（sqlite-* 除外）+ 互通对拍 4 用例（修复 SessionManager build_index leaf 重放分歧）；偏离 D-020 登记回写（含 harness compaction 变体勘误：prepareCompaction 不提前返回、带 retainedTail，变体移植于 agent_harness.rs）；`02-design.md` §6.4/§12 同步 |
| 2026-07-28 | v0.1 创建 | 初始 15 任务划分（M0–M8） |
| 2026-07-28 | 选型收口 | Bedrock 接入 / OAuth 回调 / 动态库 ABI / 事件通道 / 工具并行原语钉死（设计文档 §14），同步 T04/T05/T13/T15 与编码规范 |
| 2026-07-29 | 覆盖度审查修订 | 依据 2026-07-29 覆盖度审查与 ADR-0003 全面修订：新增 T16（harness 层，M3）；T05 循环语义 9→19 条；T06 补 output_accumulator/bash_executor 与全常数锚点；T07 修正 session 无锁、补延迟落盘/id 规则/条目全集；T10 补 RPC 30 命令与 CLI 全标志语义；T13 provider 清单更新为 39 工厂 + 7 OAuth + compat 矩阵；grep/find/ls 原生实现（ADR-0003 §2）归入 T14；T15 能力面更新为 27 事件 + 27 API + 29 UI；gates 补红线与逐条对拍基准 |
| 2026-07-29 | 二次覆盖度复核修订 | 对上一轮审查报告逐项回查上游源码后修订：修正系统性计数错误（RPC 30→32、provider 39→38、扩展事件 27→33、API 方法 27→24、UI 方法 29→28、Context 两级→三级补 ReplacedSessionContext、harness 事件 21→22）；修正与源码相反的描述（originator 字面值 "pi"、-p 吞噬条件、bash 输出不清洗、settings 单层浅合并、agent loop terminate 语义、theme `/` 为 light/dark 分隔符、vertex `{location}` 占位符丢弃、theme colors 51 必填、diagnostics 三种字面值、扩展同名冲突分项规则）；补协议字面量（Claude Code 伪装 2.1.75/beta 头/system 前缀、17 条 canonical 工具名、compat 21 字段与 thinkingFormat 10 取值、call_id\|item_id 复合格式、Codex WS 续传、Azure/mistral/Google/Bedrock 字段级清单）、OAuth 遗漏流程（device code 5 家、codex deviceauth 旁路、copilot policy-enable、ANTHROPIC_AUTH_TOKEN 走 Bearer）、compaction 第 4 个 prompt 与格式串、harness 语义（emitRunFailure 失败路径、subscribe/on 双订阅、entryTransforms/entryProjectors、leaf 重放、proxy 12 事件、JSONL 硬要 v3）、终端自省四件套与 auto light/dark、工具 P2 细节（read 图像拒绝子规则/路径变体、edit JSON-string 强转、grep `:`/`-` 分隔、schema 强转表、retry-after 优先级链、calculateCost tier 口径），并标注 Google/Bedrock SDK 委托来源空白 |
| 2026-08-03 | T09 验收通过 | Settings 与资源加载完成：settings_manager/environment/skills/prompt_templates/system_prompt/themes/keybindings/resource_loader 八模块落地；resources 对拍黄金 6 组（`fixtures/generate-resources-golden.mjs`）；偏离 D-014 登记并回写 `02-design.md` §6.7/§12；设计文档 §12 映射表补 resource-loader/skills/prompt-templates/system-prompt/themes 行 |
| 2026-08-04 | T10 验收通过 | Headless 三模式完成：手写 CLI 解析器（args.test.ts 84 移植测试）、ModelRuntime/ModelResolver、AgentSession 体系、启动管线（app.rs）、print/json（print_mode.rs）与 rpc（rpc.rs，32 命令逐条契约测试锚定 docs/rpc.md + pir-rpc bin）；parity_headless 5 场景 fixtures 归一化 diff；SDK 示例测试；偏离 D-015 登记并回写 `02-design.md` §6.1/§6.3/§6.6/§12；fixtures/README 补齐计划口径更新（RPC 走契约测试不录 transcript） |
| 2026-08-05 | T11 验收通过 | pir-tui 核心引擎完成：tui.rs（渲染管线六步/六种全量回退/16ms 节流/Kitty 图像行/overlay 栈 + focus 恢复状态机/CURSOR_MARKER/终端自省）、terminal.rs（Kitty 协商 + DA 立即回退、drain_input、OSC 9;4）、stdin_buffer/keys/keybindings/native_modifiers/terminal_colors/terminal_image/utils/recovery 与基础组件六件落地；VT 帧级对拍 + 快照黄金 11 例 + tmux/terminal-setup 映射表 31 条；pty smoke 三退出路径验证（人工 smoke 挂起至 T12）；偏离 D-016（含两条平台功能缺口，立 ADR-0004）/D-017 登记回写并关闭；`02-design.md` §5/§12、`01-requirements.md` §8.6、`coding-standards.md` §8.2 同步 |
