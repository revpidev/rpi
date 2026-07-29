# T02：对拍基建与关键技术验证

- **状态**：未开始
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

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] 归一化器单测：同一输出两次归一化结果幂等；timestamp/uuid/cwd 变化不影响 diff 结果
- [ ] diff 工具对「归一化后相同」的输出判一致，对事件序/结构/行序差异报错并定位
- [ ] faux provider 可按脚本产出确定性 `StreamEvent` 序列；cache 模拟与 tokensPerSecond 行为正确
- [ ] runbook 按步骤可重新生成 fixtures（抽一条场景验证）
- [ ] spike demo 运行成功：Wasm 插件注册工具并被宿主调用、dialog 往返完成、声明式组件描述渲染往返完成

## 门禁验收

通用门禁 G1–G7 全过（G3 说明：本任务交付对拍工具本身，以归一化/diff 自测替代）。

任务特有标准：

- [ ] `fixtures/` 下有首批样例且 `fixtures/README.md` runbook 完整可重复
- [ ] 归一化 + diff 为 `pir-test-support` 单一实现
- [ ] Wasm ABI spike 闭环演示通过（工具注册 + dialog + 组件描述）
- [ ] 二进制体积实测数据记录（是否 < 50MB；超标则登记偏离并评估）

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| — | （暂无） | — |

## 验收记录

（待填写，模板见 `gates.md` §3）
