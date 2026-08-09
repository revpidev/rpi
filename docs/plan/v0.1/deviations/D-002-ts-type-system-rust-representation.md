# D-002：TS 类型系统特性的 Rust 表达（类型契约锁定层的表征适配）

> 复制本模板为 `D-NNN-<short-slug>.md` 并填写。登记说明见 [README.md](./README.md)。

- **状态**：已回写
- **关联任务**：T01
- **级别**：实现细节偏离
- **发现日期**：2026-07-30

## 原文档约定

- 文档与章节：`docs/02-design.md` §3.2（rpi-ai 核心类型）、§4.1（rpi-agent 分层表）；`docs/coding-standards.md` §2.3（公开类型命名保留上游拼写）
- 原文约定：核心类型「镜像 Pi」；`rpi-agent` 的 `types` 模块含「AgentEvent / AgentTool / AgentMessage（含扩展消息文本格式常量）」。

## 实际实现与偏离原因

T01 锁定类型契约时，上游 TS 的四个类型系统特性在 Rust 中无对应物，做了如下表征适配
（均保持线格式兼容，行为语义不变）：

1. **声明合并折叠**：coding-agent 用 `declare module` 把 `bashExecution` / `custom` /
   `branchSummary` / `compactionSummary` 合并进 agent 包的 `AgentMessage`。Rust 无声明
   合并，四个自定义消息类型直接折叠进 `rpi-agent::messages::AgentMessage` 联合
   （`messages.rs` 溯源注释已注明）。
2. **条件类型合并**：`Model.compat` 上游是按 `api` 分支的条件类型（4 套 compat 接口）。
   四套接口的重名字段类型完全一致，合并为单个平铺 `ModelCompat`（全字段 Optional +
   缺省省略），两方向线兼容；字段适用性由 `api` 在适配器层约束。
3. **泛型接口 trait 化**：`AgentTool<TParameters, TDetails>`（含 `execute` 回调的
   interface）转为 `async_trait AgentTool`，泛型参数以 `serde_json::Value` 取代以保持
   对象安全；`execute` 的「throw 表失败」转为 `Err(AgentError)`。
4. **开放联合 newtype 化**：`Api = KnownApi | (string & {})` 转为 `ApiKind(String)`
   newtype + 10 个已知常量，自定义 API 字符串保持可行（线格式同上游）。

另：`StopReason`/`StreamEvent`/`AgentEvent` 等枚举的 `type`/`role` 标签用
单变体标记枚举（如 `AssistantRole`）承载，使每个消息 struct 独立序列化时自带
上游字面值标签（上游序列化对象总含 `role`）；`AgentEvent::MessageUpdate` 的
`assistant_message_event` 加了 `Box`（`clippy::large_enum_variant`，serde 形状不变）。

## 影响面

无（纯内部）。线格式形状由 T01 快照测试锁定（`rpi-ai/src/types.rs`、
`rpi-agent/src/{types,messages}.rs` tests，30 例）。

## 处置

- **回写位置**：`docs/02-design.md` §3.2、§4.1
- **回写日期**：2026-07-30
- **ADR**：不需要
