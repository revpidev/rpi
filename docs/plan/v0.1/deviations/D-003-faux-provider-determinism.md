# D-003：faux provider 确定性化（切块 / id / 时间戳 / 工厂签名）

- **状态**：已关闭
- **关联任务**：T02
- **级别**：实现细节偏离
- **发现日期**：2026-07-30

## 原文档约定

- 文档与章节：`docs/02-design.md` §3.7（faux provider）、`coding-standards.md` §12.4
- 原文约定：faux provider 为「脚本化响应队列 + 响应工厂 + `tokensPerSecond` + usage 4 字符/token 估算 + cache 模拟 + `state.callCount`」，移植上游 `packages/ai/src/providers/faux.ts`。

## 实际实现与偏离原因

`rpi-test-support/src/faux.rs` 在移植时做了四处确定性化（上游对应行为均带 `Math.random` / `Date.now()` 非确定性）：

1. **delta 切块**：上游 `splitStringByTokenSize` 用 `Math.random` 在 min–max 间取 token 大小；rpi 改为 min..=max 确定性循环。delta 边界不落盘（session JSONL 只存终态消息），不入对拍契约。
2. **默认 id**：上游 `randomId()`（`Date.now() + Math.random`）；rpi 用线程局部计数器（`tool:1`、`tool:2`…），每个测试线程内确定。
3. **`faux_assistant_message` 默认 timestamp**：上游 `Date.now()`；rpi 默认 `0`（归一化器本就剥离 timestamp）。
4. **响应工厂签名**：上游允许 async 工厂；rpi 为同步 `Fn` 闭包（测试场景无需异步）。

另：usage 估算的 `estimateTokens` 上游按 UTF-16 code unit 计数 / 4，rpi 按 Unicode scalar（chars）计数 / 4——BMP 文本（fixtures 只用 BMP）结果一致。

偏离原因：测试基建需要可重复性；非确定性切块使 fixtures 的 events transcript 无法复现（已在 `fixtures/README.md` §2 注明 events.jsonl 仅以事件类型序列 + 终止消息为对拍粒度，session.jsonl 为字节级锚点）。

## 影响面

无（纯内部测试基建）。session 格式 / 协议 / 扩展 API / TUI 行为均不受影响。

## 处置

- **回写位置**：`docs/02-design.md` §3.7（补确定性化说明）；`fixtures/README.md` §2（events.jsonl 对拍粒度说明）；`rpi-test-support/src/faux.rs` 文件头注释
- **回写日期**：2026-07-30
- **ADR**：不需要
