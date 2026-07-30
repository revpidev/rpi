# T02：对拍基建与关键技术验证

- **状态**：已完成
- **里程碑**：M0
- **依赖**：T01
- **上游对照**：`external/pi` 测试设施、faux provider（`packages/ai/src/providers/faux.ts`）、`docs/rpc.md` / `docs/session-format.md` 样例、`packages/evals/pi-harness.ts`（参考）
- **需求章节**：§11.1、§11.2（二进制体积）；§5.5（faux 行为）
- **预估**：0.5–0.8 人月（M0 共 1–1.5，与 T01 合计）

---

## 目标

交付全项目的对拍基建（fixtures 生成、归一化、diff、faux provider、虚拟终端助手），
并完成 Wasm ABI spike 与二进制体积实测，消除 M0 两大技术不确定性。

## 范围

### In

- `pir-test-support`：
  - faux provider（确定性 stream 事件脚本驱动，编码规范 §12.4）：脚本化响应队列 + 响应工厂、`tokensPerSecond`、usage 4 字符/token 估算、cache 模拟（sessionId 且 cacheRetention≠none）、队列空固定错误文案、`state.callCount`
  - 归一化器：剥离 timestamp / uuid / session id / cwd，其余字节保留（**全项目唯一实现**）
  - diff 工具：归一化后比对事件序列 / JSONL 结构（含**行序**）/ RPC transcript
  - `VirtualTerminal` 帧记录助手（T11/T12 复用）
- fixtures 生成 runbook（`fixtures/README.md`）：在钉死 commit 的 `external/pi` 上用 faux provider + 固定 prompt 脚本跑标准场景，导出 session JSONL 与 RPC transcript 到 `fixtures/`（设计文档 §10.2）
- 首批 fixtures：单轮问答、read/bash 工具调用、steering / follow-up、**abort、length 截断整批失败**（compaction 场景随 T08 补、RPC 全命令 transcript 随 T10 补）
- 逐条对拍级基准清单建立（需求 §11.1）：`session-format.md` / `rpc.md` / `compaction.md` / `keybindings.md` / `tmux.md` / `terminal-setup.md`，登记到 `fixtures/README.md`
- Wasm ABI spike：wasmtime 宿主 + `registerTool` + 一个 dialog 往返 + **一个声明式 UI 组件描述渲染往返**（需求 §9.2 的协议形状验证）
- 二进制体积实测：嵌入 wasmtime 的 release 单文件体积记录（目标 < 50MB，需求 §11.2）

### Out

- 真实 provider 适配器（T03/T13）
- 扩展宿主正式实现（T15，spike 只验证可行性）

## 开发要点

- 归一化规则集中一处实现，后续任务一律复用，禁止各写各的（编码规范 §12.3）
- fixtures 生成必须可重复：固定 commit + 固定脚本 + runbook 记录每一步
- spike 代码放 `pir-ext-host` 的 examples 或独立 bin，验证后保留为后续开发的参考
- 体积实测用 musl + rustls 目标（ADR-0002 §5），结果记录在验收记录中

## 进度跟踪

- [x] 设计细化
- [x] 实现
- [x] 自测
- [x] 门禁验收
- [x] 文档回写

## 自测清单

- [x] 归一化器单测：同一输出两次归一化结果幂等；timestamp/uuid/cwd 变化不影响 diff 结果
- [x] diff 工具对「归一化后相同」的输出判一致，对事件序/结构/行序差异报错并定位
- [x] faux provider 可按脚本产出确定性 `StreamEvent` 序列；cache 模拟与 tokensPerSecond 行为正确
- [x] runbook 按步骤可重新生成 fixtures（抽一条场景验证）
- [x] spike demo 运行成功：Wasm 插件注册工具并被宿主调用、dialog 往返完成、声明式组件描述渲染往返完成

## 门禁验收

通用门禁 G1–G7 全过（G3 说明：本任务交付对拍工具本身，以归一化/diff 自测替代）。

任务特有标准：

- [x] `fixtures/` 下有首批样例且 `fixtures/README.md` runbook 完整可重复
- [x] 归一化 + diff 为 `pir-test-support` 单一实现
- [x] Wasm ABI spike 闭环演示通过（工具注册 + dialog + 组件描述）
- [x] 二进制体积实测数据记录（是否 < 50MB；超标则登记偏离并评估）

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| D-003 | faux provider 确定性化（切块 / 默认 id / 默认 timestamp / 同步工厂；chars/4 usage 估算） | 已关闭 |

## 验收记录

- 验收日期：2026-07-30
- 验收人：实现者自证（单人开发，按 gates.md §1 逐项自证）
- G1 构建/静态检查：通过。`cargo fmt --all -- --check` FMT-OK；`cargo build --workspace` Finished；`cargo clippy --workspace --all-targets -- -D warnings` exit=0（工具链：`.tooling/` 内 1.97.1，与系统 rustc 同版本，见 `crates/pir-ext-host/examples/wasm-spike/README.md` 工具链说明）
- G2 测试：通过（`cargo test --workspace`：63 passed, 0 failed；pir-agent 17 + pir-ai 13 + pir-test-support 27 + fixtures_smoke 6；无 live 测试，非 live 测试不访问网络）
- G3 对拍：通过（本任务交付对拍工具本身，以归一化/diff 自测替代）。证据：
  - 归一化幂等 / id 一致映射 / uuid·cwd·timestamp 剥离：normalize.rs 6 单测 + fixtures_smoke 6 测全过；
  - runbook 可重复性抽验：`node fixtures/generate-fixtures.mjs single-turn` 重生成后 `cargo run -p pir-test-support --example normalize-diff -- <before> <after>` 输出 `OK: inputs are equal after normalization`，exit=0；
  - events.jsonl 对拍粒度说明（上游 delta 切块非确定）已登记：`fixtures/README.md` §2、D-003
- G4 红线：通过。`external/pi` HEAD=2efa728、`git status --porcelain` 为空（npm ci 的 node_modules 与 dist 均 gitignored）；未引入 JS/TS 执行能力（wasmtime 为 Wasm runtime）；未读写 `~/.pi`/`.pi`（fixtures 生成全程临时目录）；无 SQLite；token 估算 4 字符/token（D-003 注明 chars/4，BMP 等价）；非测试代码仅 3 处 expect 均有不变式注释（normalize.rs ×2、vt.rs ×1、faux.rs mutex ×1）；无凭据日志；无范围排除项；grep/find 未涉及；无 session 写代码
- G5 线格式：不适用（本任务未新增线格式类型；fixtures 为上游自身产出格式，faux 复用 T01 已验收的 `pir-ai` 类型）
- G6 文档同步：通过。回写位置：`02-design.md` §3.7（faux 确定性化）、`fixtures/README.md`（runbook + 逐条对拍基准清单 + events.jsonl 粒度）、`crates/pir-ext-host/examples/wasm-spike/README.md`（spike runbook + musl 实测方法）、溯源注释（faux.rs / normalize.rs / 各新模块文件头）
- G7 偏离闭环：通过（D-003 已登记并回写，状态「已回写」；无行为级偏离）
- 任务特有标准：
  - fixtures 首批 5 场景（single-turn / tool-calls / steering-followup / abort / length-truncation）+ runbook ✅
  - 归一化 + diff 单一实现于 `pir-test-support`（normalize.rs / diff.rs，lib.rs 文件头声明禁止另写）✅
  - Wasm ABI spike 闭环：`cargo run -p pir-ext-host --example wasm_spike` 输出 `WASM SPIKE OK`，exit=0（registerTool + dialog + 声明式组件渲染两帧往返）✅
  - 二进制体积实测：**12.4MB（13,001,728 字节）< 50MB** ✅。`CC_x86_64_unknown_linux_musl=gcc RUSTFLAGS="-C linker=rust-lld" cargo build --release -p pir --target x86_64-unknown-linux-musl`，wasmtime 47.0.2（默认 features，`--wasm-smoke` 钩子强制链入），musl 静态二进制运行 `--wasm-smoke` 输出 `wasmtime engine ok` ✅
- 结论：**通过**
