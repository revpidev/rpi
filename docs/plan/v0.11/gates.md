# 任务门禁验收标准（Gates）— v0.11

> 本文档是 Pir 开发计划 v0.11 所有任务共用的门禁验收文档，在 [v0.1 gates](../v0.1/gates.md) 基础上按新基线修订。
> 每个任务完成后，必须按本文档逐条验收并填写验收记录，方可标记为 `已完成`。

---

## 1. 验收流程

同 v0.1 gates §1：实现完成 → 自测清单全过 → 逐条核对本门禁 → 填写验收记录 → 状态置「已完成」。

## 2. 通用门禁（G1–G7）

### G1 构建与静态检查

```bash
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

全部通过，无警告（含测试目标）。

### G2 测试

```bash
cargo test --workspace
```

- 全部通过；live 测试在未设 `PIR_LIVE_TEST=1` 时默认跳过且不得失败。
- **回归红线**：v0.1 既有测试不得因本版本变更而失败；确因上游行为变更需要修改既有测试期望的，必须在验收记录中逐条列出「旧期望 → 新期望 + 上游 commit 依据」。
- 移植上游的新增测试意图用例命名与上游对应（编码规范 §12.2）。

### G3 对拍门禁（行为类任务强制）

- 涉及行为契约的任务必须跑归一化 diff 对拍；**本版本 fixtures 基线为新版上游（`4181f66`）**，由 `fixtures/generate-*.mjs` 脚本重新生成，禁止复用 v0.1 旧基线。
- 逐条对拍级基准文档在 v0.1 基础上新增/更新：
  - `docs/json.md` / `docs/rpc.md`（上游已改写：message_update delta-only、删除 start/done/error delta 类型）；
  - 上游新增回归测试目录 `packages/coding-agent/test/regressions/`（`7290-json-stream-linear`、`7253-manual-compact-during-response`、`7150-rpc-prompt-during-compaction` 等）为移植蓝本；
  - TUI 逐帧黄金文件以 T28 重录基线为准。
- 纯基建/内部重构类任务需在验收记录中说明「G3 不适用」的理由。

### G4 红线检查

逐条确认（v0.1 红线继续有效，以下按新基线更新/新增）：

- [ ] `external/pi/` 无任何改动（`cd external/pi && git status --porcelain` 为空，且 HEAD 为 `4181f66e6b3ccbef760c2966ecd8b596b926fec6`）
- [ ] 未引入 JS/TS 执行能力（无 Deno/Node/QuickJS/sidecar；**Mermaid 走 Rust 移植，不嵌入 JS 引擎**）
- [ ] 未读写 `~/.pi` / `.pi`
- [ ] Session 主路径存储仍为 JSONL **v3**（coding-agent 主路径格式上游未变）；**未引入 session v4 lane 格式、lanes/records/facts 等概念**（需求 §1.2 [DEFER]）
- [ ] 未引入 SQLite 等其他存储后端
- [ ] **未实现 [DEFER] 项**：无 server/protocol/client 远程栈、无 deferred 请求生命周期（仅类型占位）、无 telemetry 管线（仅 `telemetry_context` 字段占位）、无 harness v2 运行时、无 evals 对应物
- [ ] token 估算算法与常量未偏离钉死版 Pi
- [ ] 非测试代码无 `unwrap()` / `expect()`（有不变式注释的除外）
- [ ] 日志/错误消息中无 API key、token 等凭据
- [ ] grep/find 未引入外部 rg/fd 二进制下载机制
- [ ] session 文件写入未加文件锁（锁仅限 auth/settings/trust）
- [ ] `--alt` 兼容映射保留；settings 旧键 `uiMode` 按上游语义**忽略回退默认**（不做迁移）
- [ ] `message_update` 事件不含累积 `message`/`partial` 字段（线格式契约，T18 后常态检查）

### G5 线格式与序列化

- [ ] 新增/变更字段为 camelCase，serde 形状与上游逐个核对（本版本重点：`rawStopReason`/`endTurn`/`deferred`/`namespace`/`samplingParams`/`isSubscription`、CompactResult、message_update delta）
- [ ] 新增可选字段 `skip_serializing_if` 行为与上游 optional 语义一致（不出现多余 `null`）
- [ ] 对应 fixtures 对拍通过（与 G3 合并执行）

### G6 文档同步

- [ ] 移植代码有溯源注释（上游文件路径 + `4181f66`；T29 的 mermaid 移植额外标注 grok-build 源文件 commit 哈希与 Apache-2.0 归因）
- [ ] 实现偏离已回写到 v0.11 需求/设计文档对应位置
- [ ] 升级改变了 v0.1 基线文档（`docs/01-requirements.md`/`02-design.md`）仍有效的描述时，在 v0.11 文档中增补勘误而非直接改 v0.1 文档（v0.1 文档冻结存档）

### G7 偏离闭环

- [ ] 本任务关联的所有偏离均已登记到 `deviations/`（一事一记，编号 D-051 起，登记表已更新）
- [ ] 每条偏离状态为 `已回写` 或 `已关闭`；行为级偏离有对应 ADR 编号
- [ ] 任务文件「偏离记录」一节已列出关联偏离 ID

## 3. 验收记录（任务文件中填写，模板）

沿用 v0.1 gates §3 模板（G1–G7 + 结论），G2 需附「修改既有测试期望清单」。

## 4. 进度标识维护

同 v0.1 gates §4：验收通过后更新任务文件状态与本目录 `index.md` §3 索引表。
