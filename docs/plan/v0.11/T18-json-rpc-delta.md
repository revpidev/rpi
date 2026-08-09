# T18：JSON/RPC delta 线格式与 stdout 背压

- **状态**：未开始
- **里程碑**：M1
- **依赖**：T17
- **上游对照**：`a4475344f`（#7290/#7394）：`packages/coding-agent/src/modes/json-event.ts`（新）、`print-mode.ts`、`rpc/rpc-mode.ts`、`rpc/rpc-client.ts`、`docs/json.md`、`docs/rpc.md`、`test/regressions/7290-json-stream-linear.test.ts`
- **需求章节**：v0.11 需求 R3.1；设计 §5.1
- **预估**：0.3 人月

---

## 目标

JSON/RPC 模式的 `message_update` 事件改为 delta-only（去除累积 `message` 与
`assistantMessageEvent.partial`），print/rpc 共用单一转换点，并实现 stdout 背压。
**这是全版本第一个交付项：不落地则所有 JSON/RPC 集成对拍全红。**

## 范围

### In

- 新增 `rpi::modes::json_event`（对应 `json-event.ts` 的 `toJsonEvent()`）：print-mode 与 rpc-mode 共用的唯一转换点
- `message_update` 只发 `assistantMessageEvent` 增量 delta（`contentIndex` + `delta`）；删除累积 `message` 与 `partial` 字段
- 契约文档同步：`message_start` 给初始消息、`message_end.message` 为权威终态；`start`/`done`/`error` delta 类型从 RPC 文档表删除
- `RpcClient`（rpi-rpc）事件类型同步为 delta 形态；`collect_events`/`prompt_and_wait` 等辅助按上游调整
- **stdout 背压**：事件订阅写出路径统一经 `wait_for_raw_stdout_backpressure` 等价物（print/rpc 两模式）
- fixtures 重新生成：`fixtures/generate-fixtures.mjs` 对新版上游（`4181f66`）重跑 print/json/rpc 场景；`fixtures/README.md` 口径更新
- RPC 32 命令契约测试的 `message_update` 期望全部改写为 delta 形态

### Out

- `message_end` 权威终态之外的客户端拼装辅助（上游也无，消费方自行拼装）
- RemoteSession/pi-client 相关（[DEFER]，需求 §1.2）

## 开发要点

- 先改转换层与测试期望，再重新生成 fixtures；两步分开提交，便于审查「期望改写」是否都有上游依据
- 背压实现注意 rpi 的写出路径（tokio AsyncWrite 水位 / 同步写缓冲），语义对齐上游「等待可写再发下一事件」而非丢弃/合并事件
- 上游回归 `7290-json-stream-linear`（线性输出量断言）移植为 rpi 集成测试

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] print/json 模式流式输出与新版 fixtures 归一化 diff 一致（delta-only）
- [ ] RPC 模式 32 命令契约测试全过（`message_update` 期望为 delta 形态）
- [ ] `7290-json-stream-linear` 移植测试通过：大输出场景事件字节量随内容线性增长（非二次方）
- [ ] 背压：慢消费管道（`| sleep` 或限速 reader）下无事件丢失、无 unbounded 内存增长
- [ ] `message_end.message` 权威终态与 delta 拼装结果一致（一致性断言）

## 门禁验收

通用门禁 G1–G7 全过（G3 强制：fixtures 必须对新版上游重新生成；G4 重点：`message_update` 无累积字段；G5 重点：delta 线格式 camelCase 核对）。

任务特有标准：

- [ ] 「旧期望 → 新期望 + 上游 commit 依据」清单（G2 回归红线豁免的唯一合法路径）
- [ ] `docs/json.md`/`docs/rpc.md`（上游新版）条目 → rpi 测试锚点映射表

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| （待登记） | | |

## 验收记录

（按 gates §3 模板填写）
