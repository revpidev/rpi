# Pir 项目编码规范（Rust Workspace）

> 本规范基于 Pir 项目（Rust workspace 同构复刻 Pi agent harness）的架构设计
> 与工程实践制定，适用于本仓库 `crates/` 下全部 Rust 代码。
>
> 配套文档：[`02-design.md`](./02-design.md)（crate 划分与核心设计）、
> [`UPSTREAM.md`](../UPSTREAM.md)（上游钉死版本）、
> [ADR-0001](./adr/0001-extension-and-config-dir.md) / [ADR-0002](./adr/0002-baseline-decisions.md)。
>
> 文档版本：v2.0（整体重写，替代自其他项目引入的 v1.x）
> 最后更新：2026-07-28

---

## 目录

1. [总则](#1-总则)
2. [Workspace 与 Crate 架构](#2-workspace-与-crate-架构)
3. [目录与模块规范](#3-目录与模块规范)
4. [核心抽象规范](#4-核心抽象规范)
5. [错误处理规范](#5-错误处理规范)
6. [异步与并发规范](#6-异步与并发规范)
7. [流式处理规范](#7-流式处理规范)
8. [TUI 规范](#8-tui-规范)
9. [Session 与持久化规范](#9-session-与持久化规范)
10. [配置与路径规范](#10-配置与路径规范)
11. [安全规范](#11-安全规范)
12. [测试与对拍规范](#12-测试与对拍规范)
13. [Rust 语言编码规范](#13-rust-语言编码规范)
14. [注释与文档规范](#14-注释与文档规范)
15. [质量门禁](#15-质量门禁)
16. [日志规范](#16-日志规范)
17. [设计决策速查表](#17-设计决策速查表)

---

## 1. 总则

### 1.1 适用范围

本规范适用于 Pir workspace 内的所有 crate（`pir-ai`、`pir-agent`、`pir-tui`、
`pir`、`pir-ext-host`、`pir-test-support`）。Pir 的工程目标是：

- 用 Rust **同构复刻** Pi agent harness，行为层 1:1 对拍一致
- 交付**单一可执行文件**（Wasm runtime 嵌入主二进制）
- 完整交互式 TUI 为硬性交付（ADR-0002 §3）

### 1.2 核心原则

| 原则 | 含义 |
|------|------|
| **上游即金标准** | 行为语义以 `external/pi` @ 钉死 commit 为准；语义不明时查上游源码与测试，禁止「自以为合理」的漂移 |
| **包边界同构** | 核心 crate 一一对应 Pi 四包，模块/文件命名镜像上游，便于交叉检索 |
| **错误进流不进 panic** | provider / stream 失败转为事件 + `stopReason`，与 Pi 一致 |
| **依赖注入可测** | Agent 不依赖具体 provider（`StreamFn` 注入）；核心只依赖 `ExtensionHost` trait |
| **显式对齐** | 凡与上游有**有意差异**（命名、配置路径、扩展语言），必须由 ADR 钉死并在代码注释中标注 |
| **显式优于隐式** | 错误、取消、并发、路径解析都显式表达，不藏全局状态 |

### 1.3 不可触碰的红线

- ❌ **不得修改 `external/pi/` 的任何内容**——它是只读上游对照；升级 pin 须新开 ADR（ADR-0002 §1）
- ❌ 不得引入 JS/TS 执行能力（无 Deno/Node/QuickJS 嵌入、无 Node sidecar、无 jiti 兼容）（ADR-0001）
- ❌ 不得默认读写 `~/.pi` / `.pi`，不做迁移工具（ADR-0001 §2）
- ❌ Session 存储不得引入 SQLite 或其他后端——第一版**仅 JSONL**（ADR-0002 §7）
- ❌ token 估算不得使用 tiktoken 等任何其他算法——必须与钉死版 Pi **同一算法与常量**（ADR-0002 §4）
- ❌ 可恢复错误不得 `panic!` / `unwrap()` / `expect()`（测试代码除外）
- ❌ 不得在日志中输出 API key、OAuth token 等凭据

---

## 2. Workspace 与 Crate 架构

### 2.1 Crate 划分

```
pir/
├── Cargo.toml                 # workspace 定义
├── crates/
│   ├── pir-ai/                # ↔ @earendil-works/pi-ai：类型、流事件、API 适配、provider、auth
│   ├── pir-agent/             # ↔ @earendil-works/pi-agent-core：agent loop、状态、事件
│   ├── pir-tui/               # ↔ @earendil-works/pi-tui：组件、差分渲染、键位
│   ├── pir/                   # ↔ @earendil-works/pi-coding-agent：bin + lib SDK
│   ├── pir-ext-host/          # Rust + Wasm 扩展宿主（无 JS，本项目新增）
│   └── pir-test-support/      # faux provider、黄金 JSONL、归一化/diff、VT 助手
├── external/pi/               # 上游只读对照（钉死 commit，见 UPSTREAM.md）
└── fixtures/                  # 从 Pi 导出的 session / RPC 样例
```

### 2.2 依赖规则

```
                 ┌──────────┐
                 │ pir (bin)│
                 └────┬─────┘
        ┌─────────┬───┼───────────┬──────────────┐
        ▼         ▼   ▼           ▼              ▼
    pir-agent  pir-ai  pir-tui  pir-ext-host  pir-test-support(dev)
        └─────────┘
        （pir-agent 依赖 pir-ai 的类型：Model / Context / StreamEvent）
```

- `pir-ai` **不依赖**任何其他内部 crate，可独立使用
- `pir-agent` 只依赖 `pir-ai` 的**类型层**，不依赖具体 provider 实现
  （`StreamFn` 由调用方注入，见 [§4.2](#42-streamfn-注入)）
- `pir-tui` 不依赖 `pir-ai` / `pir-agent`，终端 I/O 只用 `crossterm`
- `pir`（coding-agent）是**唯一的组装点**：定义 `ExtensionHost` trait，
  绑定 `pir-ext-host` 实现，把 `pir-ai` 的 `Models::stream` 注入为 `StreamFn`
- `pir-test-support` 只作为 **dev-dependency** 被引用，不得进入发布依赖链
- 依赖单向，禁止成环；新增跨 crate 依赖须先核对设计文档 §2.1 依赖图

### 2.3 与上游的模块映射

移植代码时**保持文件级对应关系**，命名取上游文件名的 snake_case：

| Pi 路径 | Pir 路径 |
|---------|----------|
| `packages/ai/src/api/anthropic-messages.ts` | `crates/pir-ai/src/api/anthropic_messages.rs` |
| `packages/agent/src/agent-loop.ts` | `crates/pir-agent/src/agent_loop.rs` |
| `packages/tui/src/components/editor.ts` | `crates/pir-tui/src/components/editor.rs` |
| `packages/coding-agent/src/core/session-manager.ts` | `crates/pir/src/core/session_manager.rs` |

完整映射表见设计文档 §12。规则：

- 一个上游文件对应一个同名 Rust 文件；拆分时以上游子导出为界
- 公开类型命名保留上游拼写（`AgentSession`、`SessionManager`、`StreamEvent`），
  保证跨代码库可检索
- 移植文件头部用注释标注上游来源（见 [§14.3](#143-移植溯源注释)）

---

## 3. 目录与模块规范

### 3.1 模块声明风格（强制）

采用 Rust 2018+ 模块规范，**不使用 `mod.rs`**。目录模块的声明放在同名 `.rs` 文件中。

```
crates/pir-ai/src/
├── lib.rs
├── api.rs              # 声明 api/ 下的子模块
├── api/
│   ├── openai_completions.rs
│   ├── anthropic_messages.rs
│   └── ...
└── providers.rs        # 声明 providers/ 下的子模块
```

> ❌ 错误：`api/mod.rs`
> ✅ 正确：`api.rs` + `api/` 目录

### 3.2 生成代码

- 模型目录等生成产物（`build.rs` 或 `pir update --models` 产出）**禁止手工编辑**，
  修改生成器后重新生成
- 生成文件头部必须带 `// @generated ... DO NOT EDIT` 标记
- 启动时对生成数据**只读**；写路径只在显式 update 命令中

### 3.3 文件规模约束

移植类文件允许与上游文件规模相当（上游大文件不强行拆分，保持对应关系）；
新写代码（扩展宿主、组装层、测试设施）单文件超过约 400 行时应在评审中评估拆分。

---

## 4. 核心抽象规范

### 4.1 事件枚举锁定

`StreamEvent`（pir-ai）与 `AgentEvent`（pir-agent）是跨 crate 的行为契约，
在 M0 锁定后，**变体增删改必须同时更新对拍 fixtures**：

```rust
pub enum StreamEvent {
    Start { partial: AssistantMessage },
    TextStart,
    TextDelta { delta: String },
    TextEnd,
    ThinkingStart,
    ThinkingDelta { delta: String },
    ThinkingEnd,
    ToolCallStart { /* ... */ },
    ToolCallDelta { /* ... */ },
    ToolCallEnd { /* ... */ },
    Done { message: AssistantMessage },
    Error { /* ... */ },
}
```

- 事件变体顺序、字段命名镜像上游 TS 定义
- 事件的序列化形状是 RPC / JSON mode 的线格式，见 [§4.4](#44-线格式与持久化类型的-serde-约定)

### 4.2 StreamFn 注入

`pir-agent` 的 Agent **不**直接依赖 provider，流式能力通过函数注入：

```rust
pub type StreamFn = Arc<
    dyn Fn(Model, Context, StreamOptions) -> BoxStream<'static, StreamEvent> + Send + Sync,
>;
```

- 测试注入 faux stream（`pir-test-support`），生产注入 `pir-ai` 的 `Models::stream`
- 新增 agent 能力时保持这一边界：需要 provider 行为的测试一律走 faux，不打真实网络

### 4.3 ExtensionHost trait

核心（`pir` crate）只依赖 trait，实现（Rust 内置 / Wasm）在 `pir-ext-host`：

```rust
#[async_trait]
pub trait ExtensionHost: Send + Sync {
    async fn load(&mut self, paths: &[PathBuf]) -> Result<Vec<ExtensionId>, ExtError>;
    async fn reload(&mut self) -> Result<(), ExtError>;
    fn as_api(&self) -> &dyn ExtensionApi;
}
```

- `ExtensionApi` 的能力面与 Pi `ExtensionAPI` **同构**（registerTool/Command、
  生命周期钩子、UI bridge），但实现语言仅 Rust/Wasm（ADR-0001）
- UI 桥按 mode 分实现：`InteractiveUiBridge` / `RpcUiBridge` / `NullUiBridge`
- 核心代码中出现扩展事件点（tool_call、session_* 等）时，调用 `emit` 并合并
  block/transform 结果，**不得**绕过宿主直接执行扩展逻辑

### 4.4 线格式与持久化类型的 serde 约定

凡是**与 Pi 互通的 JSON 形状**（session JSONL、RPC 帧、settings、models.json、
扩展 manifest）必须字节级对齐上游：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]   // Pi 为 TS，字段一律 camelCase
pub struct SessionHeader {
    pub session_id: String,
    pub parent_id: Option<String>,
    // ...
}
```

- 字段命名用 `rename_all = "camelCase"` 或逐字段 `rename`，禁止顺手用 snake_case 出网
- 枚举的序列化表示（`tag` / `content` / 裸字符串）按上游 JSON 实际形状选择，逐个核对
- 可选字段的 `null` 与缺省语义与上游一致，必要时用 `skip_serializing_if`
- 每种线格式类型必须有 fixtures 对拍测试兜底（见 [§12.3](#123-黄金文件对拍)）
- 纯内部类型（不进 JSONL/RPC 的）不受此限，按 Rust 惯例即可

---

## 5. 错误处理规范

### 5.1 错误类型分层

| 位置 | 错误类型 | 说明 |
|------|---------|------|
| 库 crate（pir-ai、pir-agent、pir-tui、pir-ext-host） | `thiserror` 派生的具体错误（`AiError`、`AgentError`、`ExtError` 等） | 每 crate 一个主错误枚举，必要时按模块拆分 |
| bin / 组装边界（pir 的 main、modes 入口） | `anyhow::Result` | 只在最外层聚合并附加上下文 |
| 流式路径 | **不是异常**——转为 `StreamEvent::Error` + `stopReason` | 与 Pi 一致，见 §5.2 |

```rust
#[derive(Debug, thiserror::Error)]
pub enum AiError {
    #[error("http error: {0}")]
    Http(String),
    #[error("auth failed for provider {provider}: {reason}")]
    Auth { provider: String, reason: String },
    #[error("stream interrupted: {0}")]
    Stream(String),
    // ...
}
```

### 5.2 错误进流，不进 panic

provider 调用、HTTP、流解析的失败**不得**向上抛穿 agent loop：

- 在 `pir-ai` 边界把 `AiError` 转为 `StreamEvent::Error`，assistant 消息携带
  对应 `stopReason`（`error` / `aborted` 等，与上游枚举一致）
- agent loop 把错误事件当作正常事件继续 emit，由 UI / RPC 层呈现
- `Result` 只用于**调用方必须立即处理**的失败（加载 session 损坏、配置非法、
  磁盘写失败等启动期/持久化错误）

### 5.3 一般原则

- 非测试代码禁止 `unwrap()` / `expect()`，除非有注释说明的不变式保证
- 用 `?` 传播错误，跨错误类型靠 `#[from]` / `map_err` 显式转换
- 错误信息面向两类读者：用户可读（出了什么事）+ 开发可定位（哪个 provider、
  哪个文件、哪一阶段），敏感信息（key、token）脱敏后再进错误串
- `panic!` 仅表示程序 bug；TUI 必须挂 panic hook 恢复终端状态（见 [§8.5](#85-终端状态恢复)）

---

## 6. 异步与并发规范

### 6.1 运行时

- 统一 `tokio`；入口 `#[tokio::main]`
- I/O 必须 async；CPU 密集或阻塞调用（如大文件同步 API）用 `tokio::task::spawn_blocking`
- 禁止在 async 上下文中调用阻塞 API（`std::fs` 同步读大文件、`std::thread::sleep` 等）

### 6.2 事件通道与订阅屏障

- 事件分发用每 listener 独立的 `tokio::sync::mpsc` channel（不用 `broadcast`，其 Lagged 语义与 Pi 的背压模型不符）
- `Agent::subscribe` 对每个 listener **逐个 await**（有序屏障），与 Pi 的语义一致：
  不得改成并发 fan-out 后再合并，事件顺序是对拍契约的一部分

### 6.3 工具并发执行

- 并行工具执行用 `JoinSet` + 完成通道
- 结果必须按 toolCall **源序**组装后再 emit，不得按完成序
- 顺序/并行的选择逻辑移植上游，不自行「优化」

### 6.4 取消

- 取消令牌（`CancellationToken` 或等价抽象）贯穿 stream、bash 子进程、工具执行
- abort 语义对齐上游：stream 收到取消后以 `stopReason: "aborted"` 收尾，
  bash 工具负责终止子进程组
- 每个 `spawn` 的任务必须有明确的取消/退出路径，禁止泄漏后台任务

### 6.5 共享状态

- 跨任务共享用 `Arc<T>`；可变内部状态用 `tokio::sync::Mutex` / `RwLock`
- 锁持有时间尽可能短，**不在锁内 `.await`**（先取数据、释放锁、再 await）
- 会话状态（消息视图、当前 model、thinking level）集中在 `AgentSession`，
  不散落为全局静态

---

## 7. 流式处理规范

### 7.1 适配器统一接口

每个 API 适配器实现同一 trait，由 `Models::stream` 按 `model.api` 分发：

```rust
#[async_trait]
pub trait ApiStream: Send + Sync {
    async fn stream(
        &self,
        model: &Model,
        ctx: &Context,
        opts: StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = StreamEvent> + Send>>, AiError>;
}
```

- 每个上游 provider 一个适配器文件（`api/anthropic_messages.rs` 等），
  文件名与上游一一对应
- thinking 等跨协议能力在 `stream_simple` 层统一映射，适配器内不各自发明

### 7.2 增量解析

- partial tool-call JSON 采用「流式累积 + 结束时校验」，与 Pi 对齐；
  中途不半成品校验、不提前报错
- 工具参数 schema 用 `jsonschema` 在 tool-call 完成时校验
- 文本/thinking 增量原样透传，合并重排只发生在事件消费侧（UI、JSON mode）

### 7.3 背压与缓冲

- 事件流不无限缓冲：消费者慢时通过 channel 容量施加背压
- TUI 侧渲染节流（16ms）在消费端做，不影响流本身

---

## 8. TUI 规范

### 8.1 技术边界

- 移植 pi-tui 的算法（ANSI 行列表 + 自定义差分 + Overlay + Kitty 输入），
  **不引入 ratatui**——其 widget/layout 模型与 Pi 交互契约不兼容（设计文档 §5.1）
- 终端 I/O 只用 `crossterm`（raw mode、读写、尺寸）

### 8.2 组件抽象

```rust
pub trait Component: Send {
    /// Render to ANSI lines at the given terminal width.
    fn render(&self, width: usize) -> Vec<String>;
}

pub trait Focusable: Component {
    fn handle_input(&mut self, raw: &str);
    fn focused(&self) -> bool;
}
```

- 组件输出 ANSI 行，不直接写终端；写终端只有 `Tui` 一处
- 组件移植优先级与拆分按设计文档 §5.5

### 8.3 渲染管线

保持与上游一致的渲染步骤，不得随意重排：

1. CSI 2026 包裹同步输出
2. 首次 / 尺寸变化 → 清屏全量；否则行差分
3. 渲染节流 16ms
4. 行尾补 SGR + OSC 8 reset
5. Kitty 图像行范围 expand + delete

### 8.4 键位

- `StdinBuffer` → 键位解析（Kitty flags=7 + legacy）→ 全局 listener → focused 组件
- **禁止硬编码按键判断**；默认键位集中定义在默认 keybindings 表中，
  可被用户 JSON 配置覆盖，token 名与 Pi 一致（保证配置互通）

### 8.5 终端状态恢复

- 进入 TUI 时保存终端状态，退出 / panic / 收到信号时**必须恢复**（raw mode、
  alternate screen、光标）
- 安装 panic hook：先恢复终端，再走默认 panic 输出
- 这条是硬性正确性要求，评审时逐条核对所有退出路径（正常退出、abort、错误、panic）

---

## 9. Session 与持久化规范

### 9.1 存储后端

- **仅 JSONL**（ADR-0002 §7）；路径默认 `~/.pir/agent/sessions/`
- JSONL 记录格式与钉死版 Pi 对齐（字段 camelCase，见 [§4.4](#44-线格式与持久化类型的-serde-约定)），保证可手工拷贝互通
- 加载迁移 v1–v3 的逻辑移植上游，迁移规则变更须有 fixtures 覆盖

### 9.2 写入纪律

- 追加写在 message_end / tool / model_change 等事件点触发，与上游事件点一致
- 文件锁用 `fs2`（对齐 `proper-lockfile` 意图）；锁的获取/释放在 `SessionManager` 内闭环
- 追加写失败是持久化错误（`Result` 上抛），不进事件流

### 9.3 Session 树

- `id` 为 8 位 hex，`parentId` 组织分支树；leaf 导航逻辑移植上游
- `build_context_messages()` 的 compaction / branch_summary retainedTail 规则逐行移植，
  此处是行为对拍重点，禁止凭理解重写

### 9.4 Compaction

- token 估算逐字节移植 Pi `estimateTokens`（chars/4 启发式），不允许偏差（ADR-0002 §4）
- 切点搜索、split turn、summary prompt 模板**字节级对齐**，保证可对拍
- compaction 相关纯函数集中在一个模块，配黄金用例

---

## 10. 配置与路径规范

### 10.1 路径解析单点

```rust
pub struct PirConfig {
    pub app_name: String,          // "pir"
    pub config_dir_name: String,   // ".pir"
    pub env_prefix: String,        // "PIR"
}
// agent_dir   = ~/.pir/agent
// project_dir = <cwd>/.pir
```

- 所有路径解析集中在单一模块（对齐上游 `config.ts`），**禁止**在业务代码里
  各自拼 `home_dir()` / `join(".pir")`
- 环境变量统一 `PIR_` 前缀，读取也集中在配置模块
- 不读 `~/.pi` / `.pi`（ADR-0001 §2）

### 10.2 资源发现

`ResourceLoader` 统一发现顺序：全局 `~/.pir/agent` → 项目 `.pir` → settings 指定路径
→ CLI flags → packages。新增资源类型时挂到这一管线，不另起发现逻辑。

### 10.3 可配置 Endpoint

版本检查、telemetry 等产品 endpoint 必须 settings / env 可配置、可关闭，
不硬编码唯一官方 URL（ADR-0002 §8）。LLM 的自定义 base URL 走
`models.json` / provider 配置，与产品 endpoint 分开。

---

## 11. 安全规范

### 11.1 凭据存储

- `CredentialStore` 为 JSON 文件，权限 **0600**；创建时用显式权限位，
  不依赖 umask 运气
- API key 与 OAuth token（含 refresh）分条目管理；OAuth 流程为 PKCE /
  device code / localhost callback
- 内存中的凭据不进入 Debug 输出（自定义 `fmt::Debug` 或 `secrecy` 包装）

### 11.2 日志与错误脱敏

- 禁止在 tracing 事件、错误消息、`/debug` 快照中输出 API key、token、
  Authorization 头
- provider payload debug 开关与上游对齐，默认关闭

### 11.3 工具执行

- bash 工具的 `spawn_hook` 是扩展/沙箱改道的唯一入口；核心不绕过 hook 直接 spawn
- 子进程以进程组管理，取消时能整组终止

### 11.4 扩展沙箱

- Wasm 扩展只通过 host ABI 获得能力，按 capability 授予；不给默认全量文件/网络权限
- Rust 动态库扩展为高级路径，文档中明确其信任级别等同于主进程

---

## 12. 测试与对拍规范

### 12.1 测试金字塔

| 层级 | 内容 | 位置 |
|------|------|------|
| 纯逻辑单测 | loop 事件序、工具排序、session 树、compaction 切点 | 与被测代码同文件 `#[cfg(test)]` |
| 契约测试 | RPC 命令/响应、session JSONL schema | 各 crate `tests/` |
| 黄金文件对拍 | fixtures diff（归一化后） | `fixtures/` + `pir-test-support` |
| Faux provider 场景 | 确定性 tool-call 脚本驱动的端到端 | `pir` crate `tests/` |
| Live 测试 | 真实 provider | `PIR_LIVE_TEST=1` + API keys 才启用 |

### 12.2 测试意图移植

- `external/pi` 中相关 vitest 用例的**意图**移植为同名 Rust 测试
  （`agent-loop.test.ts` 的 `emits events in order` → `agent_loop::tests::emits_events_in_order`）
- 移植的是断言意图，不是逐行翻译；TS 特有的 mock 手法换成 faux provider / fake stream

### 12.3 黄金文件对拍

- fixtures 生成 runbook 见设计文档 §10.2；生成必须可重复（钉死 commit + 固定脚本）
- 归一化规则：剥离 timestamp / uuid / session id / cwd，其余字节保留；
  归一化与 diff 脚本归属 `pir-test-support`，**只有一处实现**
- 对拍断言：事件类型序列、工具调用序列、session JSONL 结构必须一致
- fixtures 变更与行为变更同 PR 提交，并在提交信息中说明

### 12.4 Faux provider

- 所有非 live 测试一律用 faux provider / fake stream，**禁止**测试打真实网络
- faux 脚本（确定性的 stream 事件序列 + tool-call 序列）放 `pir-test-support`，
  场景可复用

### 12.5 TUI 测试

- `VirtualTerminal` 记录输出帧，断言关键 ANSI 序列子集（去 CSI 2026 抖动）
- 组件级渲染快照黄金文件（Editor / SelectList / Markdown / SettingsList 等）
- 真机矩阵仅 smoke，进 CI nightly，不阻塞 PR

### 12.6 测试纪律

- 命名：`test_<被测行为>_<场景>` 或移植上游用例名（snake_case）
- 一个测试验证一个行为；测试数据用工厂函数构造
- 新增/修改测试必须本地跑过再提交
- live 测试默认跳过，未设置 `PIR_LIVE_TEST=1` 时不得失败

---

## 13. Rust 语言编码规范

### 13.1 命名约定

| 类型 | 风格 | 示例 |
|------|------|------|
| 模块、函数、变量、字段 | `snake_case` | `agent_loop`、`stream_fn` |
| 类型、结构体、枚举、trait | `UpperCamelCase` | `AgentSession`、`ExtensionHost` |
| 常量、静态变量 | `SCREAMING_SNAKE_CASE` | `DEFAULT_CONTEXT_WINDOW` |
| 泛型参数 | 单大写字母或 `UpperCamelCase` | `T`、`S` |
| 移植上游的公开类型 | **保留上游拼写** | `SessionManager`（而非 `SessionMgr`） |

### 13.2 类型与错误

- 错误类型用 `thiserror` 派生（库）/ `anyhow`（bin 边界），见 [§5](#5-错误处理规范)
- 业务可恢复错误用 `Result`；流式失败进事件（见 §5.2）
- 字符串：`String`（所有权）/ `&str`（借用），避免热路径上的多余 `clone()`
- token 计数、窗口大小用 `u32`/`u64`；与上游数值类型语义对齐

### 13.3 所有权与借用

- 优先借用；跨任务共享用 `Arc<T>`；trait 对象用 `Arc<dyn Trait>`
- 事件/消息在流上传递时注意拷贝成本，热路径上的大消息考虑 `Arc<AssistantMessage>`
- 避免为「图省事」的 `.clone().await` 链；先想清楚所有权归属

### 13.4 trait 与泛型

- 异步 trait 用 `#[async_trait]` 标注（MSRV 允许原生 `async fn in trait` 后可逐步迁移，另行决策）
- 跨线程共享的 trait 必须有 `Send + Sync` 约束
- 需要替换实现的（StreamFn、ExtensionHost、UiBridge）用 trait 对象注入；
  编译期单态化收益明确的内部热路径可用泛型

### 13.5 序列化

- 线格式/持久化类型遵循 [§4.4](#44-线格式与持久化类型的-serde-约定)
- 内部类型序列化从简；`Default` + `#[serde(default)]` 处理向后兼容读取

### 13.6 格式化

- 统一 `cargo fmt`，禁止手动调格式
- import 顺序：标准库 → 第三方 crate → 本 workspace（空行分隔）
- workspace 级 `rustfmt.toml` 统一配置，各 crate 不单独覆盖

---

## 14. 注释与文档规范

### 14.1 模块文档

每个 `.rs` 文件顶部用 `//!` 说明职责：

```rust
//! Agent loop -- stateless turn loop emitting AgentEvents.
```

### 14.2 公共 API 文档

跨 crate 的公开 API 用 `///` 文档注释，说明用途与契约；crate 内部私有项不强制。

### 14.3 移植溯源注释

移植自上游的文件/函数必须标注来源与对应版本，语义有意偏差处重点说明：

```rust
//! Port of `packages/agent/src/agent-loop.ts` @ pi 0.82.1 (2efa728).
//!
//! Intentional differences: none.

// Byte-for-byte port of estimateTokens (compaction.ts); do not "improve" (ADR-0002 §4).
fn estimate_tokens(text: &str) -> u32 { /* ... */ }
```

### 14.4 内联注释

- 注释解释「为什么」，不复述「是什么」
- 与上游对齐的微妙行为（事件顺序、retainedTail 规则、错误码）必须注释指向依据
- 注释与代码同步更新；行为变更时检查溯源注释是否仍成立

### 14.5 注释语言

- 模块文档（`//!`）、公共 API 文档（`///`）、移植溯源注释**必须英文**（与上游代码对照方便）
- 内联注释（`//`）推荐英文，解释复杂对齐逻辑时允许中文

---

## 15. 质量门禁

### 15.1 提交前必须通过

```bash
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo test --workspace          # live 测试默认跳过
```

### 15.2 上游钉死校验

CI 校验 `external/pi` 处于钉死 commit：

```bash
cd external/pi && git rev-parse HEAD
# 期望: 2efa728d2ee90ef597626e96b1e28ef2b279f07c
```

升级 pin 须新开 ADR 并重新生成 fixtures 对拍（ADR-0002 §1）。

### 15.3 提交前检查清单

- [ ] `build` / `clippy -D warnings` / `fmt --check` / `test` 全部通过
- [ ] 行为变更附带对拍断言或 fixtures 更新
- [ ] 线格式变更（JSONL / RPC / settings）有 camelCase 与 serde 形状核对
- [ ] 移植代码有溯源注释，语义偏差已说明
- [ ] 非测试代码无 `unwrap()` / `expect()`
- [ ] 日志与错误中无凭据等敏感信息
- [ ] 未触碰 `external/pi/`
- [ ] 新增 spawn 任务有取消路径；TUI 改动核对终端恢复

### 15.4 Release Profile

发布目标为单一自包含可执行文件（优先 musl + rustls，ADR-0002 §5）：

```toml
[profile.release]
lto = true
codegen-units = 1
strip = true
```

> 不设 `panic = "abort"`：TUI 依赖 panic hook 恢复终端状态（见 §8.5）。

---

## 16. 日志规范

### 16.1 日志库

统一 `tracing` + `tracing-subscriber`，禁止混用 `log` crate。
TUI 模式下日志写文件或 ring buffer（`/debug` 可查），**不直接写 stderr** 干扰渲染。

### 16.2 级别约定

| 级别 | 用途 | 示例 |
|------|------|------|
| `ERROR` | 需人工介入的故障 | session 写盘失败、凭据文件损坏 |
| `WARN` | 降级/重试/可疑行为 | provider 重试、扩展加载失败跳过 |
| `INFO` | 关键生命周期事件 | 启动、mode 进入、session 创建/fork |
| `DEBUG` | 诊断信息 | 事件分发、compaction 决策、token 估算明细 |
| `TRACE` | 详细追踪 | provider payload（须脱敏+开关）、原始键位序列 |

### 16.3 规范

- 结构化字段（`tracing::info!(session_id = %id, "session created")`），不字符串拼接
- 循环/流式热路径不用 `INFO` 及以上级别
- 默认 `INFO`，`PIR_LOG` / `RUST_LOG` 环境变量调整
- provider payload debug 默认关闭，开关语义与上游对齐

---

## 17. 设计决策速查表

| 决策 | 选择 | 依据 |
|------|------|------|
| 行为金标准 | `external/pi` @ `2efa728` / 0.82.1 | ADR-0002 §1，UPSTREAM.md |
| 模块风格 | 无 `mod.rs` | Rust 2018+ 惯例，workspace 统一 |
| 文件对应 | 上游文件名 snake_case 镜像 | 设计文档 §12，交叉检索 |
| provider 解耦 | `StreamFn` 注入 | Agent 可独立测试，faux/proxy 友好 |
| 扩展 | 核心只依赖 `ExtensionHost` trait；实现 Rust/Wasm | ADR-0001，无 JS |
| 错误处理 | 库 `thiserror`；bin `anyhow`；流式失败进事件 | 设计文档 §9、§1 原则 2 |
| 事件顺序 | subscribe 有序屏障；工具结果按源序 | 对拍契约 |
| 取消 | CancellationToken 贯穿 stream/bash | 对齐上游 abort 语义 |
| 线格式 | serde camelCase 字节级对齐 + fixtures 对拍 | session/RPC 与 Pi 互通 |
| Session 存储 | 仅 JSONL + fs2 文件锁 | ADR-0002 §7 |
| token 估算 | 逐字节移植 Pi chars/4 启发式 | ADR-0002 §4 |
| 路径解析 | 单一模块，`~/.pir` / `.pir` / `PIR_` 前缀 | ADR-0001 §2 |
| TUI | 移植 pi-tui 算法 + crossterm；不用 ratatui | 设计文档 §5.1 |
| 终端恢复 | panic hook + 全部退出路径恢复 | §8.5 硬性要求 |
| 日志 | tracing；TUI 下不写 stderr | §16 |
| 发布形态 | 单文件（musl + rustls），Wasm runtime 内嵌 | ADR-0002 §5 |
| 产品 endpoint | settings/env 可配置可关闭 | ADR-0002 §8 |

---

## 附录 A：依赖基线

| 依赖 | 用途 |
|------|------|
| `tokio` | 异步运行时 |
| `reqwest`（rustls） | HTTP（服务单文件部署目标） |
| `clap` | CLI 解析 |
| `serde` / `serde_json` | 序列化 / JSONL / RPC |
| `serde_yaml` | YAML frontmatter（skills/prompts） |
| `jsonschema` | 工具参数 schema 校验 |
| `async-trait` | 异步 trait |
| `thiserror` / `anyhow` | 错误（库 / bin 边界） |
| `tracing` / `tracing-subscriber` | 结构化日志 |
| `crossterm` | TUI 终端 I/O |
| `unicode-width` / `unicode-segmentation` | 字符宽度与 grapheme |
| `fs2` | session 文件锁 |
| `globset` / `ignore` | glob 与忽略规则 |
| `wasmtime` | Wasm 扩展 runtime（嵌入主二进制） |
| `oauth2` | OAuth 流程（自研模块 + 该 crate） |
| `axum` | OAuth 一次性 localhost 回调页 |
| `abi_stable` | L0 动态库扩展插件 ABI |
| （不引 aws-sdk） | Bedrock 手写 SigV4 + reqwest + 自实现 event-stream 解码 |

> 具体版本以 workspace `Cargo.toml` 为准；升级须评估对钉死行为的影响并重新对拍。

## 附录 B：常见反模式

| 反模式 | 错误示例 | 正确做法 |
|--------|---------|---------|
| **凭理解写语义** | 「这里应该是这样」自行实现 compaction 切点 | 查 `external/pi` 对应源码与测试，移植并标注溯源 |
| **流式失败上抛 panic** | `stream.next().await.unwrap()` | 转为 `StreamEvent::Error` + `stopReason` |
| **线格式顺手 snake_case** | `#[serde(rename_all = "snake_case")]` 出 JSONL | camelCase + fixtures 对拍（§4.4） |
| **事件并发乱序** | 工具结果按完成序 emit | 按 toolCall 源序组装（§6.3） |
| **绕过 StreamFn 直连 provider** | agent 内 `use pir_ai::providers::*` | 组装层注入 `StreamFn`（§4.2） |
| **业务代码拼配置路径** | `home_dir().join(".pir/agent")` 散落各处 | 单一路径模块（§10.1） |
| **硬编码键位** | `if key == "ctrl+x"` | 默认 keybindings 表，可配置（§8.4） |
| **TUI 直接写终端** | 组件内 `execute!(stdout(), ...)` | 组件只产 ANSI 行，`Tui` 统一写（§8.2） |
| **测试打真实网络** | 单测里起 reqwest 调 api.anthropic.com | faux provider；live 测试加 env 门禁（§12.4） |
| **日志泄露凭据** | `debug!("headers: {:?}", headers)` | 脱敏；payload debug 开关默认关（§11.2） |
| **在锁内 .await** | `lock.lock().await; fetch().await;` | 取数据后先释放锁（§6.5） |
| **改生成文件** | 手改生成的模型目录 | 改生成器重新生成（§3.2） |
| **改上游对照** | 在 `external/pi` 里修 bug | 永远只读；升级走 ADR（§1.3） |

---

**文档状态**：维护中。规范与 `crates/` 代码同步演进；架构级变更须先过 ADR，
再更新本规范与 [`02-design.md`](./02-design.md)。
