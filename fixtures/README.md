# Pir 对拍 Fixtures

> 设计文档 §10.2 / 需求 §11.1 / 编码规范 §12.3 的落地目录。
> 上游对照：`external/pi` @ `2efa728d2ee90ef597626e96b1e28ef2b279f07c`（0.82.1），钉死，见 `UPSTREAM.md`。
>
> **归一化与 diff 只有一处实现**：`pir-test-support`（`normalize.rs` / `diff.rs`）。
> fixtures 保存**原始字节**；timestamp / uuid / session id / cwd 的剥离在 diff 时进行，不在生成时进行。

## 1. 目录结构

```
fixtures/
├── README.md                # 本文件：runbook + 逐条对拍基准清单
├── generate-fixtures.mjs    # 生成脚本（钉死 commit + 固定 prompt 脚本）
└── generated/
    └── <scenario>/
        ├── session.jsonl    # 真实落盘的 session 文件（file-backed SessionManager）
        └── events.jsonl     # AgentSession 事件 transcript（json 模式同款事件形状）
```

## 2. Runbook（可重复生成）

前置（一次性；`node_modules/` 与 `dist/` 均在 `.gitignore` 内，不触碰红线 G4）：

```bash
cd external/pi
git rev-parse HEAD   # 必须为 2efa728d2ee90ef597626e96b1e28ef2b279f07c
npm ci
npm run build --workspace @earendil-works/pi-tui
npm run build --workspace @earendil-works/pi-ai
npm run build --workspace @earendil-works/pi-agent-core
npm run build --workspace @earendil-works/pi-coding-agent
```

生成（在本仓库根目录）：

```bash
node fixtures/generate-fixtures.mjs            # 全量场景
node fixtures/generate-fixtures.mjs single-turn # 单个场景
```

生成器行为：每个场景在临时目录建独立 cwd / agentDir（**不读写 `~/.pi`**），
用 `fauxProvider()`（`api: "faux"` 固定）+ 固定 prompt 脚本驱动 `createAgentSession`，
导出 file-backed `SessionManager` 的真实 session 文件与 `session.subscribe` 捕获的事件序列。

验证可重复性（抽一条场景）：重新生成同一场景到临时副本，归一化后与仓库内
fixtures diff 应为空：

```bash
cp -r fixtures/generated/single-turn /tmp/single-turn-before
node fixtures/generate-fixtures.mjs single-turn
cargo run -p pir-test-support --example normalize-diff -- \
  /tmp/single-turn-before/session.jsonl fixtures/generated/single-turn/session.jsonl
```

（`normalize-diff` example 见 §4；退出码 0 = 归一化后一致。）

> **`session.jsonl` 是字节级可重复锚点；`events.jsonl` 不是。** 上游 faux
> provider 用 `Math.random` 切 delta（`faux.ts` `splitStringByTokenSize`），
> 每次运行的 delta 边界与数量不同。`events.jsonl` 的对拍粒度是**事件类型
> 序列 + 终止消息内容**（delta 边界不入契约，也不会落盘进 session JSONL）；
> pir 侧 faux 为确定性切块（`pir-test-support/src/faux.rs` 文件头有偏离说明）。

**纪律**：fixtures 变更必须与行为变更同 commit 提交，并在提交信息中说明（编码规范 §12.3）。

## 3. 首批场景（T02 交付）

| 场景 | 脚本要点 | 覆盖契约 |
|------|---------|---------|
| `single-turn` | 单 prompt → 单 text 响应 | header/model_change/thinking_level_change/message 条目；agent_start→turn_start→message_*→turn_end→agent_end→agent_settled 事件序 |
| `tool-calls` | read + bash 两个 toolCall → 真实工具执行 → 收尾 text | toolcall_* 事件序、toolResult 条目、tool_execution_* 事件、双工具源序 |
| `steering-followup` | 流式中 steer、后续流式中 followUp | 排队语义：无工具调用时 steer/followUp 均作为后续 turn 投递（queue_update 事件、turn 序列） |
| `abort` | 流式中 `session.abort()` | aborted assistant 消息落盘（stopReason=aborted）、abort 事件序 |
| `length-truncation` | stopReason=length 收尾 | length 截断消息的持久化形状 |
| `compaction-threshold` | 8192 窗口/4096 reserve/512 keep：三轮问答，阈值触发两轮压缩（split-turn 前缀 + UPDATE 迭代），第三轮 prepare 为空静默 | CompactionEntry（firstKeptEntryId/usage/details/fromHook=false）、compaction_start/end 事件序、tokensBefore 重算、estimatedTokensAfter |
| `compaction-overflow` | 16384 窗口：overflow error（"prompt is too long"）→ 恢复压缩 → 重试成功 | overflow 恢复路径、willRetry=true 事件序、恢复预算一次后重置 |

补齐计划（任务索引）：compaction 场景已随 **T08** 交付；RPC 全命令
transcript（32 命令）随 **T10** 追加，届时本表与生成脚本同步扩展。

## 4. 归一化 / diff 用法

```rust
use pir_test_support::{diff_jsonl, diff_event_sequence, Normalizer};

// session JSONL 对拍（含行序）：
diff_jsonl(expected_fixture, actual_output)?;

// 事件序列对拍（事件类型序）：
diff_event_sequence(expected_events, actual_events)?;
```

CLI 形式（抽验、手工对拍）：`cargo run -p pir-test-support --example normalize-diff -- <expected> <actual>`
—— 各自归一化后 diff，输出首个差异定位（行号 + 上下文）。

归一化规则（`pir-test-support/src/normalize.rs`，全项目唯一实现）：

- `timestamp` 键 → 类型保留常量（数字 → `0`，字符串 → `"<ts>"`）
- id 键（`id`/`parentId`/`fromId`/`firstKeptEntryId`/`toolCallId`/`sessionId`/`responseId`/`parentSession`）
  与任意位置的 uuid → 一致占位符 `<id:N>`（首次出现序）
- 字符串内 ISO-8601 时间戳 → `<ts>`
- 配置的 cwd / agentDir 路径前缀 → `<path>`
- 其余字节保留

## 5. 逐条对拍级基准清单（需求 §11.1）

六份上游文档是字节/行为级对拍基准。下表登记「文档条目 → 对拍锚点」；
锚点状态随任务推进补齐（✅ = 已有锚点，⏳ = 计划任务）。

### 5.1 `docs/session-format.md`（T07 主场）

| 条目 | 锚点 | 状态 |
|------|------|------|
| 文件位置 `sessions/--<path>--/<ts>_<uuid>.jsonl` | T07 单测 + fixtures header | ⏳ T07 |
| Session version（v1→v2→v3 迁移、当前 v3） | T07 迁移用例 | ⏳ T07 |
| Entry base（`id` 8-hex / `parentId` / ISO `timestamp`） | 全部 `generated/*/session.jsonl` | ✅ T02 |
| SessionHeader（含 `parentSession` 变体） | `generated/*/session.jsonl` 首行 | ✅ T02（parentSession 变体 ⏳ T07） |
| SessionMessageEntry（user/assistant/toolResult） | `single-turn` / `tool-calls` / `abort` | ✅ T02 |
| ModelChangeEntry / ThinkingLevelChangeEntry | 各 fixtures 第 2/3 行 | ✅ T02 |
| CompactionEntry（firstKeptEntryId / retainedTail / usage / details / fromHook） | `compaction-threshold` / `compaction-overflow` fixtures | ✅ T08（firstKeptEntryId 形态；retainedTail 读取兼容见 D-012） |
| BranchSummaryEntry / CustomEntry / CustomMessageEntry / LabelEntry / SessionInfoEntry | — | ⏳ T07/T08 |
| Extended messages（bashExecution / custom / branchSummary / compactionSummary） | — | ⏳ T07/T08 |
| Tree Structure / Context Building 算法 | T07 单测 | ⏳ T07 |
| stopReason=length / aborted 持久化形状 | `length-truncation` / `abort` | ✅ T02 |

### 5.2 `docs/rpc.md`（T10 主场）

| 条目 | 锚点 | 状态 |
|------|------|------|
| 协议框架（framing：JSONL 请求/响应/事件） | RPC transcript fixtures | ⏳ T10 |
| 32 命令逐条（prompt/steer/follow_up/abort/new_session/get_state/get_messages/set_model/cycle_model/get_available_models/set_thinking_level/cycle_thinking_level/get_available_thinking_levels/set_steering_mode/set_follow_up_mode/compact/set_auto_compaction/set_auto_retry/abort_retry/bash/abort_bash/get_session_stats/export_html/switch_session/fork/clone 等） | RPC transcript fixtures + 契约测 | ⏳ T10 |
| steer/followUp/abort 事件语义 | `steering-followup` / `abort` 事件 transcript | ✅ T02（SDK 层；RPC 层 ⏳ T10） |

### 5.3 `docs/compaction.md`（T08 主场）

| 条目 | 锚点 | 状态 |
|------|------|------|
| 触发条件 / 切点规则 / split turns | T08 黄金用例（`compaction/golden.json`）+ `compaction_runner_test` + compaction fixtures | ✅ T08 |
| CompactionEntry / BranchSummaryEntry 结构 | `compaction-threshold` / `compaction-overflow` fixtures（CompactionEntry）；BranchSummaryEntry 准备/装填在 T08 黄金单测 | ✅ T08（BranchSummaryEntry 持久化 ⏳ T12/T16） |
| Summary Format 章节模板（Goal/Constraints/Progress/…/Critical Context） | T08 `compaction/prompts/*.txt` 逐字节比对 | ✅ T08 |
| 消息序列化（Message Serialization） | T08 黄金用例（serializeConversation） | ✅ T08 |
| session_before_compact / session_before_tree 扩展语义 | T15 扩展事件对拍 | ⏳ T15 |
| Settings（阈值字段） | T08 用例（reserveTokens/keepRecentTokens）；settings 文件接线 ⏳ T09 | ✅ T08 |

### 5.4 `docs/keybindings.md`（T11/T12 主场）

| 条目 | 锚点 | 状态 |
|------|------|------|
| Key Format 解析 | T11 单测 | ⏳ T11 |
| 全部 action 默认绑定表（12 节逐表） | T12 绑定表快照黄金文件 | ⏳ T12 |
| 自定义配置合并语义 | T09/T12 用例 | ⏳ T12 |

### 5.5 `docs/tmux.md` / 5.6 `docs/terminal-setup.md`（T11/T12 主场，字节序列级）

| 条目 | 锚点 | 状态 |
|------|------|------|
| tmux 推荐配置与 `csi-u` 行为 | T12 终端能力检测用例 | ⏳ T12 |
| 各终端（Kitty/iTerm2/Apple/Ghostty/WezTerm/Alacritty/VS Code/Windows Terminal/xfce4/IntelliJ）设置与转义序列 | T11/T12 VirtualTerminal 帧对拍（去 CSI 2026 抖动） | ⏳ T11/T12 |
