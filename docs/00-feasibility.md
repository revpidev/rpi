# Pir 可行性分析：用 Rust 1:1 复刻 Pi Agent Harness

> 分析对象：`external/pi`（[@earendil-works/pi](https://github.com/earendil-works/pi)，v0.82.1）  
> 目标产品：`pir`（Pi in Rust）——功能 1:1、架构同构（「1:1」的三层边界定义见 [`01-requirements.md`](./01-requirements.md) §1.5）  
> 日期：2026-07-27

---

## 1. 结论摘要

| 维度 | 判定 |
|------|------|
| **整体可行性** | **可行**，但属于大型工程，不是库移植 |
| **纯 Rust 行为级 1:1** | 核心循环 / Session / Tools / RPC / Skills / Providers：**可行** |
| **扩展策略（已决策）** | **Rust / Wasm API 同构** + **安装计划**；不跑 TS pi-package（[ADR-0001](./adr/0001-extension-and-config-dir.md)、[ADR-0002](./adr/0002-baseline-decisions.md)） |
| **配置目录（已决策）** | 默认 **`~/.pir/agent`** 与项目 **`.pir`** |
| **上游（已决策）** | 钉死 **0.82.1 / `2efa728`**（[`UPSTREAM.md`](../UPSTREAM.md)） |
| **粗估工作量** | 约 **23–33 人月**核心工程（含 TUI 与扩展安装；另计 20–30% 持续开销，全量约 28–43 人月；不含 TS 运行时） |

**一句话**：Pi 是「薄产品层 + 厚协议层 + 自研 TUI」。Rust 复刻在架构上可同构；扩展只保证 API 形状同构并由 Rust/Wasm 重写。其余（含数十 Provider、Session 树、差分 TUI）难，但都是确定性工程。

---

## 2. Pi 现状盘点

### 2.1 Monorepo 包结构与规模

```
packages/
  ai/            ~21k LOC   统一 LLM API / Auth / Providers
  agent/         ~10k LOC   agentLoop + Agent + Harness
  tui/           ~12k LOC   差分渲染终端 UI
  coding-agent/  ~55k LOC   CLI / Session / Extensions / Interactive
  storage/                  SQLite 适配（可选）
  server/        ~2k LOC    辅助服务
  evals/                    评测
```

四核心包 TypeScript 源码合计约 **~99k LOC**（不含测试、生成模型目录、文档）。

### 2.2 产品定位

Pi 自称 **minimal terminal coding harness**：

- 默认只给模型四个核心工具：`read` / `write` / `edit` / `bash`（coding-agent 另提供可选 `grep` / `find` / `ls`）
- **不**内置子 agent、plan mode、MCP、权限弹窗——这些靠 Extensions / Packages 外挂
- 四种运行面：Interactive TUI、Print、JSON 事件流、RPC；另有 Node SDK

### 2.3 架构分层（必须同构保留）

```
┌──────────────────────────────────────────────────────────┐
│  coding-agent：CLI / 四种 Mode / Session / Ext / Skills  │
├──────────────────────────────────────────────────────────┤
│  agent-core：Agent 状态机 + agentLoop + 可选 Harness     │
├──────────────────────────────────────────────────────────┤
│  ai：Models / Provider / Api 协议 / Auth / Stream 事件   │
├──────────────────────────────────────────────────────────┤
│  tui：差分渲染 + Editor + Overlay + Key 协议             │
└──────────────────────────────────────────────────────────┘
```

依赖方向严格单向：`coding-agent → agent → ai`，`coding-agent → tui`。Rust workspace 应保持相同边界。

---

## 3. 分模块可行性

### 3.1 `pi-ai` → `pir-ai`（高可行，工作量大）

| 能力 | 难度 | 说明 |
|------|------|------|
| 统一消息 / Tool / Usage / Cost 类型 | 低 | 直接映射到 serde 类型 |
| 10 种 KnownApi 协议适配 | **高** | OpenAI Completions/Responses、Anthropic Messages、Google、Bedrock、Mistral、Codex、Azure、Vertex、Pi-Messages |
| 30+ Provider 工厂 + 模型目录刷新 | 中高 | 多数是薄封装；难点在鉴权差异与 catalog 同步 |
| Streaming + partial tool JSON | 中 | 需仔细对齐事件序 |
| Thinking / Image / Handoff | 中 | 有明确接口 |
| OAuth（Anthropic / Codex / Copilot / …） | **高** | PKCE、device code、token refresh、本地回调页 |
| TypeBox 参数校验 | 低–中 | 改为 JSON Schema + `jsonschema` / 自研校验 |

**Rust 映射**：`reqwest` + `tokio` + `serde_json` + 可选 `aws-sdk-bedrockruntime`；OAuth 用独立 crate。

**风险**：协议细节与边界用例极多；必须以 pi 的 vitest 行为与真实 provider 流量为金标准，而非「看起来像」。

### 3.2 `pi-agent-core` → `pir-agent`（高可行）

核心资产是 **事件序确定的 tool-use 状态机**：

- `agentLoop` / `agentLoopContinue`
- `transformContext` → `convertToLlm` 边界
- parallel / sequential 工具执行与结果排序
- steering / follow-up 队列
- `beforeToolCall` / `afterToolCall` / `terminate` / `shouldStopAfterTurn`
- subscribe 屏障（`Agent` 包装层 await listener）

这些是纯逻辑，Rust `async` + channel/broadcast 可 1:1。应优先移植并用事件序测试锁死。

### 3.3 `pi-tui` → `pir-tui`（可行，勿误用 ratatui 当 1:1）

Pi TUI **不是** ratatui 同类：

- 行缓冲差分渲染 + CSI 2026 同步输出
- Kitty keyboard protocol + legacy CSI
- Overlay / Focus / IME 硬件光标（APC marker）
- Editor（2.3k LOC）+ Markdown + 内联图像

**结论**：应用 crossterm/termios 做 I/O，**算法与语义从 pi-tui 移植**；ratatui 只适合非 1:1 原型。

粗估 TUI+Interactive：**8–11 人月**。

### 3.4 `pi-coding-agent` → `pir` CLI（可行，扩展除外）

| 子系统 | 1:1 难度 | 备注 |
|--------|----------|------|
| CLI args / 子命令（install/update/config） | 中 | clap 可覆盖 |
| Session JSONL 树 v1–v3 | **高** | 必须字节兼容以便迁移 |
| Compaction / branch summary | **高** | 切点、split turn、token 估算 |
| Skills / Prompt templates / Themes | 低–中 | 文件格式已文档化 |
| Packages（npm/git） | 中 | 可调系统 `npm`/`git` |
| Project trust 两阶段加载 | 中 | 时序敏感 |
| Print / JSON / RPC | 中–高 | RPC 命令面大、扩展 UI 往返 |
| Interactive Mode | **高** | 与 TUI 强耦合（单文件 6k+ LOC） |
| SDK（Rust crate） | 中 | 对应 `createAgentSession` |
| **TS Extensions（jiti）** | **架构级阻断** | 见下节 |

### 3.5 扩展系统：唯一架构级风险

现有生态扩展是：

```typescript
export default function (pi: ExtensionAPI) { ... }
```

经 **jiti** 动态执行，可 `import` 任意 npm、注册 Provider、自定义 Doom 级 TUI。

| 方案 | 兼容现有 pi-package | 复杂度 | 推荐度 |
|------|---------------------|--------|--------|
| A. 嵌入 Deno / Node / QuickJS，复现 ExtensionAPI + virtualModules | **高** | 很高 | **若目标真·1:1 必选** |
| B. Rust 插件 / Wasm ABI + 迁移工具 | 低（需重写扩展） | 中 | 长期更干净 |
| C. 双进程：Rust 核心 + Node sidecar 跑扩展 | 高 | 高（IPC） | 过渡方案 |
| D. 只做内置能力，扩展延后 | 无 | — | 不符合「功能 1:1」 |

**已采纳 B**（[ADR-0001](./adr/0001-extension-and-config-dir.md)）：Rust/Wasm ABI + API 同构；A/C 不在当前范围。

---

## 4. 功能覆盖矩阵（摘要）

完整需求见 [`01-requirements.md`](./01-requirements.md)。此处只标可行性颜色：

| 功能域 | 1:1 | 风险 |
|--------|-----|------|
| 多 Provider 流式对话 + tool call | ✅ | 协议细节 |
| 内置工具 read/write/edit/bash(+grep/find/ls) | ✅ | edit/bash 边界 |
| Session 树 / fork / clone / tree 导航 | ✅ | JSONL 兼容 |
| Compaction / branch summary | ✅ | 算法对齐 |
| Skills / prompts / themes / packages | ✅ | 包安装细节 |
| Interactive TUI + keybindings | ✅ | 终端矩阵 |
| Print / JSON / RPC | ✅ | RPC 面大 |
| OAuth 订阅登录 | ✅ | 维护成本 |
| llama.cpp 集成 | ✅ | 中 |
| HTML/JSONL export、gist share | ✅ | 低 |
| 扩展 API 形状同构（Rust/Wasm） | ✅ | 生态需自建 |
| 现有 TS pi-package 扩展 | ❌ 非目标 | ADR-0001 |
| Node SDK 嵌入 | ➡️ | 改为 Rust SDK；跨语言用 RPC |

---

## 5. 技术风险登记

| ID | 风险 | 影响 | 缓解 |
|----|------|------|------|
| R1 | 无 TS 扩展生态 | 冷启动扩展供给 | 提供 Rust/Wasm 脚手架与示例；Skills 等声明式资源仍兼容 |
| R2 | Provider/OAuth 回归 | 用户无法登录/调用 | 对拍 pi 测试 + 契约测试 + faux provider |
| R3 | TUI 终端碎片化 | 闪烁/快捷键失效 | 虚拟终端测试 + Kitty/Ghostty/WT/tmux 矩阵 |
| R4 | Session 格式漂移 | 无法续跑旧会话 | 黄金 JSONL 样例 + 迁移测试 |
| R5 | Compaction 行为差 | 丢上下文/乱切 | 用同一 prompts 与 token 估计算法对拍 |
| R6 | 体量导致烂尾 | 半成品 | 分阶段里程碑（见设计文档） |
| R7 | 上游 pi 快速演进 | 持续追赶 | 锁定对照版本 + 月度 changelog 评审与影响面清单（流程见设计文档 §11.3） |

---

## 6. 与「类似架构」的符合度

用户要求「架构也类似」。下列同构点均可在 Rust workspace 实现：

1. 四个 crate 对应四个包：`pir-ai` / `pir-agent` / `pir-tui` / `pir`（coding-agent）
2. `StreamFn` 注入，agent 不绑死 provider
3. `AgentMessage` 可扩展，LLM 边界 `convert_to_llm`
4. Session JSONL 树 + compaction 条目语义
5. ResourceLoader：extensions / skills / prompts / themes / context files
6. 四种 Mode 共用 `AgentSession` / `AgentSessionRuntime`
7. ExtensionAPI 形状同构（实现语言可变）

---

## 7. 工作量粗估（人月）

| 阶段 | 内容 | 人月 |
|------|------|------|
| M0 | 工程骨架、类型、事件契约、faux provider、对拍 harness、Wasm ABI spike | 1–1.5 |
| M1 | `pir-ai` 核心协议（Anthropic + OpenAI 系）+ Auth 基础 | 3–4 |
| M2 | `pir-agent` loop + 工具 + 并行执行 | 1.5–2 |
| M3 | Session / Compaction / Settings / Skills / Prompts | 2–3 |
| M4 | Print / JSON / RPC | 1.5–2 |
| M5 | `pir-tui` + Interactive | 8–11 |
| M6 | 剩余 Providers + OAuth 全家桶 | 3–4 |
| M7 | Packages / Trust / Update / Export / llama.cpp | 1.5–2 |
| M8 | 扩展宿主（Rust + Wasm ABI） | 1.5–3 |
| — | 测试、文档、追赶上游 | 持续 20–30% |

**合计**：约 **23–33 人月**（核心工程，已排除 TS 扩展运行时）。叠加测试、文档与上游跟踪的持续开销（约 20–30%），全量约 **28–43 人月**。

---

## 8. Go / No-Go 建议

**Go**。决策见 [ADR-0001](./adr/0001-extension-and-config-dir.md)、[ADR-0002](./adr/0002-baseline-decisions.md)。

1. 对拍钉死 commit；token 估算与 Pi 完全一致  
2. TUI 为硬性交付；RPC/JSON 作自动化对拍通道  
3. 单文件发布，Wasm runtime 打进主包  

---

## 9. 参考材料（本仓库）

- 上游克隆：`external/pi/`
- 文档入口：`external/pi/packages/coding-agent/docs/index.md`
- 关键规格：`session-format.md` / `extensions.md` / `rpc.md` / `sdk.md` / `compaction.md`
- 本仓库产出：[`01-requirements.md`](./01-requirements.md)、[`02-design.md`](./02-design.md)
