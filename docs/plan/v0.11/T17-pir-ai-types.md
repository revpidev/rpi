# T17：rpi-ai 消息类型与请求选项扩展

- **状态**：未开始
- **里程碑**：M1
- **依赖**：—
- **上游对照**：`packages/ai/src/types.ts`（`AssistantMessage`/`ToolCall`/`StopReason`/`DeferredHandle`）、`packages/agent/src/proxy.ts`（`02bd2d1c6`）
- **需求章节**：v0.11 需求 R2.1、R2.2.1（类型占位部分）、R2.8、R4.1.4；设计 §2.1
- **预估**：0.2 人月

---

## 目标

把上游 v0.84 的消息/工具调用/选项类型扩展同步进 rpi-ai 与 rpi-agent 的序列化契约，
为 M1–M3 所有后续任务解锁类型基础。**只加字段与类型，不改任何运行时行为。**

## 范围

### In

- `StopReason` 新增 `Deferred`（`Pending` 若 v0.1 未落地则一并补齐）；serde 字面值与上游逐一核对
- `AssistantMessage` 新增可选字段：`raw_stop_reason`（`rawStopReason`）、`end_turn`（`endTurn`）、`deferred`（`DeferredHandle` 仅类型占位：`provider/model_id/api/id/expires_at/poll_after_ms/data: serde_json::Value`）；`skip_serializing_if` 语义对齐上游 optional
- `ToolCall` 新增 `namespace: Option<String>`，序列化/proxy/replay 全链路保留
- `Model`/`StreamOptions` 新增 `sampling_params`（JSON map；合并语义实现属 T20，本任务仅类型与透传通道）
- `OAuthAuth` 新增 `is_subscription` 元数据字段
- `StreamOptions` 拆分为 `ProviderRequestOptions`（取消令牌/telemetry_context 占位/api_key/自定义 fetch 通道/headers/timeout/retry）+ `StreamOptions`（R2.8.1）；`telemetry_context` 仅占位，不实现管线（G4 红线）
- proxy `toolcall_end` 事件帧携带完整 `ToolCall` 对象并合并回 partial（R4.1.4，`02bd2d1c6`）
- rpi-ext-sdk/host 的对应类型同步（仅类型面；行为接线属 T27）

### Out

- deferred 请求生命周期（`fetch_deferred`/`cancel_deferred`，[DEFER] 需求 R2.2.1）
- 各 provider 对 `raw_stop_reason`/`namespace` 的实际填充（T19/T20）
- `sampling_params` 的请求体合并逻辑（T20）
- JSON/RPC 线格式变更（T18）

## 开发要点

- 这是纯类型任务：所有既有测试应保持绿；新增字段的黄金序列化用例与上游 `types.ts` 形状逐个核对
- proxy 帧变更影响 RPC/JSON 事件序列化，与 T18 交接时在 `json_event` 转换层汇合
- `ProviderRequestOptions` 拆分是签名级变更，编译器会带出全部调用点——逐点核对，不改变语义

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] 新字段 serde round-trip 黄金用例：含字段/缺省/null 三态与上游 JSON 形状一致（camelCase、无多余 null）
- [ ] `DeferredHandle` 形状与上游 `types.ts` 逐字段核对（含 `data` 任意 JSON）
- [ ] proxy `toolcall_end` 帧携带完整 toolCall 的序列化对拍（faux 驱动）
- [ ] `ProviderRequestOptions`/`StreamOptions` 拆分后全 workspace 编译，调用点语义不变（既有测试全绿）

## 门禁验收

通用门禁 G1–G7 全过（G3：类型序列化对拍；G4 重点：deferred 仅类型占位、telemetry 仅字段占位、无 session v4 概念）。

任务特有标准：

- [ ] 上游 `types.ts` @ `4181f66` 的字段清单 → rpi 类型「逐字段映射表」附验收记录
- [ ] 既有测试零失败、零期望修改（本任务不应改变任何行为）

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| （待登记） | | |

## 验收记录

（按 gates §3 模板填写）
