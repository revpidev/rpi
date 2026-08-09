# ADR-0003：覆盖度审查后的范围决策（harness / 工具基准 / 启动迁移 / pi-ai CLI）

- **状态**：已采纳
- **日期**：2026-07-29
- **关联**：[ADR-0001](./0001-extension-and-config-dir.md)、[ADR-0002](./0002-baseline-decisions.md)、[`01-requirements.md`](../01-requirements.md)、[`02-design.md`](../02-design.md)

## 背景

2026-07-29 对需求/设计文档做了逐模块覆盖度审查（对照 `external/pi` @ `2efa728` / v0.82.1 全部源码与 docs），发现若干范围模糊点。本 ADR 记录审查后的四项范围决策。

## 决策

### 1. 完整移植 agent 包的 harness 层

`packages/agent/src/harness/`（AgentHarness、SessionStorage/SessionRepo 抽象、22 种 harness 事件、compaction/branch-summary/skills/prompt-templates 工具工厂等）**纳入复刻范围**，作为 `rpi-agent` 的可选层。

- 包含 compaction 条目的 **`retainedTail` 自包含形态**（读与写），以及 harness 独有的 `active_tools_change`、`leaf` 条目类型。
- 同时保留 coding-agent 主路径的 `firstKeptEntryId` 形态；`rpi`（coding-agent 对应 crate）按钉死版行为只写 `firstKeptEntryId`。
- 理由：需求 §6.5 已要求与 Pi 会话互通且不丢数据；harness 是 Pi SDK 侧会话的生成者，完整移植可保证两类产物的读写互通，且为后续 Pi 上游切换到 harness 预留同构性。
- 注意：钉死 commit 上 harness 自述「生命周期仍在硬化中」，以**代码行为**为准，不以其设计文档为准。

### 2. 内置工具行为以 coding-agent 实现为基准

`packages/agent` 与 `packages/coding-agent` 存在两份独立的 read/write/edit/bash 实现，行为有细微差异。**对拍基准为 coding-agent 侧实现**（`packages/coding-agent/src/core/tools/` 及 `bash-executor.ts`、`tools-manager.ts`），因为 CLI 实际运行的是这份。

- `rpi/src/tools/` 直接移植 coding-agent 版本（含 grep/find/ls）。
- `rpi-agent` 的 harness 自带工具工厂作为可选层存在（决策 1），但**不作为 `rpi` CLI 的行为来源**；两者行为差异以 coding-agent 为准记录。
- grep/find 在 Pi 中依赖外部 rg/fd 二进制（缺失时自动下载）；rpi 用 `ignore`/`globset` crate 原生实现**同等行为**（gitignore 感知、相同默认 limit 与截断），不引入外部二进制下载机制。这属于实现手段差异，行为契约（输出格式、limit、截断、排序）保持 1:1。

### 3. 不实现 migrations.ts 的启动迁移

Pi `migrations.ts` 的 5 项迁移（legacy `oauth.json`/`settings.apiKeys` → `auth.json`、v0.30.0 会话目录错位修复、`commands/`→`prompts/`、`tools/`→`bin/` fd/rg 迁移、旧 keybindings 键名格式）针对的是 **Pi 老用户的 `~/.pi`**。

- rpi 使用全新 `~/.rpi`，不存在这些 legacy 格式；且 ADR-0001/0002 已定不做 `~/.pi` 迁移工具。因此**不实现**。
- 例外：keybindings 的**旧键名 → 新命名空间 id 迁移表**（`keybindings.ts` 内 60+ 项）属于**当前版本仍生效的加载行为**（用户手写旧格式配置文件会被自动迁移），**予以保留**。
- 会话 JSONL 的 v1→v2→v3 加载迁移属于文件格式兼容（§6），与本决策无关，照常实现。

### 4. 不复刻 pi-ai 包级 CLI

`@earendil-works/pi-ai` 自带的 `login`/`list` CLI 会把 `auth.json` 写到**当前工作目录**（与 coding-agent 的 `~/.pi/agent/auth.json` 分离）。

- rpi **不提供**对应命令；凭据统一走 `rpi` 主 CLI 的 `/login` 与 `~/.rpi/agent/auth.json`（结构与 Pi 兼容）。
- `rpi-ai` 作为纯库 crate，不含 bin 目标。

## 其他范围声明（审查确认，无需决策）

- `packages/server`（实验性实例管理器，coding-agent 不依赖它）、`packages/evals`（private 测试包）、`coding-agent/src/bun`（Bun 打包入口）**不在复刻范围**。
- `packages/storage/sqlite-node` 是 harness SessionRepo 抽象的可选 SQLite 后端，CLI 未使用；rpi 仅实现 JSONL 后端（ADR-0002 §7），但 `rpi-agent` 的存储 trait 与该抽象同构，预留后端扩展能力。
- 内置模型目录在 Pi 中是 `generate-models.ts` 的**生成物**（数据源 models.dev）；rpi 采用 `build.rs` 生成 + `rpi update --models` 远程刷新的同源策略（见 `02-design.md` §3.4），不手抄模型数据。

## 后果

- `01-requirements.md` §6.2 恢复 retainedTail 与 harness 条目的完整要求（读+写仅限 harness 层）。
- `rpi-agent` 范围扩大（harness 层），路线图 M2 相应包含 harness。
- 需求文档新增「范围排除」一节，显式列出 server/evals/bun/pi-ai CLI/migrations。
- 工具行为锚点全部改为 coding-agent 源码引用。
