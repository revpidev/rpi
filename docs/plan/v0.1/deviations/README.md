# 偏离管理（Deviations）

> 记录开发过程中实现与原始文档（`01-requirements.md` / `02-design.md` / ADR /
> `coding-standards.md`）之间的所有偏离。偏离必须**登记在此目录**并**回写原始文档**，
> 两者齐备才算闭环（门禁 G7）。

---

## 1. 偏离分级

| 级别 | 定义 | 处置 |
|------|------|------|
| **实现细节偏离** | 不影响行为契约：模块内部结构、私有 API、crate 内文件拆分、依赖选型微调 | 登记 + 回写原始文档，随任务门禁闭环 |
| **行为级偏离** | 影响对拍契约：事件序、线格式、session JSONL、compaction/token 估算、RPC 语义、TUI 行为、CLI/slash 行为 | **不允许直接落地**。须先立 ADR 转入「有意差异」（需求文档 §1.5 第 3 层），再登记回写 |

拿不准级别时按行为级处理。

## 2. 登记流程

1. 复制 [`TEMPLATE.md`](./TEMPLATE.md) 为 `D-NNN-<short-slug>.md`（NNN 从 001 起递增）；
2. 填写偏离内容，状态初始为 `待回写`；
3. 在下表登记一行；
4. 回写原始文档（在原文对应章节更新描述，保持文档与实现一致）；
5. 在偏离文件中填「回写位置」，状态改为 `已回写`；
6. 任务门禁验收时逐条核对（gates.md G7）。

## 3. 偏离登记表

| ID | 任务 | 级别 | 摘要 | 回写位置 | ADR | 状态 |
|----|------|------|------|----------|-----|------|
| D-001 | T01 | 实现细节 | session 条目类型单一来源化（`pir-agent::session`，合并 coding-agent 与 harness 两套定义） | `02-design.md` §4.1、§12 | 不需要 | 已回写 |
| D-002 | T01 | 实现细节 | TS 类型系统特性的 Rust 表达（声明合并折叠、compat 条件类型合并、AgentTool trait 化、Api 开放联合 newtype 化） | `02-design.md` §3.2、§4.1 | 不需要 | 已回写 |
| D-003 | T02 | 实现细节 | faux provider 确定性化（切块 / 默认 id / 默认 timestamp / 同步工厂；chars/4 usage 估算） | `02-design.md` §3.7、`fixtures/README.md` §2 | 不需要 | 已关闭 |
| D-004 | T03 | 实现细节 | ApiStream trait 形状 → ProviderStreams（同步返回事件流，含 stream_simple） | `02-design.md` §3.3 | 不需要 | 已回写 |
| D-005 | T03 | 实现细节 | 适配器 HTTP 层 reqwest 直连替代官方 SDK 的可观测差异（SDK 头/超时/严格 SSE 解析文案/metadata.raw 来源/错误前缀范围） | `02-design.md` §3.3 | 不需要 | 已回写 |
| D-006 | T03 | 实现细节 | 校验/解析层差异（jsonschema 单路径、models.json serde+手工 pass、错误措辞≠TypeBox） | `01-requirements.md` §5.5、`02-design.md` §3.6 | 不需要 | 已回写 |
| D-007 | T03 | 实现细节 | sanitize_surrogates 在 Rust 侧为恒等（String 无孤立代理） | `02-design.md` §3.6 | 不需要 | 已回写 |
| D-008 | T04 | 实现细节 | auth 存储与 key DSL 的 Rust 落地差异（fs2 无 stale/compromised、jitter 随机源、`!cmd` 仅 unix、快照保序方案、resolve_headers 形状） | `02-design.md` §3.5、`01-requirements.md` §5.4 | 不需要 | 已回写 |
| D-009 | T04 | 实现细节 | OAuth 框架的 Rust 落地差异（时钟抽象、测试缝、回调服务分支、错误明细近似、token JSON 严格化、竞速实现） | `02-design.md` §3.5、`01-requirements.md` §5.4 | 不需要 | 已回写 |
| D-010 | T05 | 实现细节 | agent_loop 与 Agent 的 Rust 落地差异（before/after 钩子回传与错误通道、流无终止事件合成、JoinError 合成、details null 省略、Message 错误变体、continue_run 命名等 11 项） | `02-design.md` §4.4 | 不需要 | 已回写 |
| D-011 | T06 | 实现细节 | 内置工具层 Rust 落地差异（ToolContext 形状、image/kamadak-exif 替代 Photon、自实现 Myers diff、OutputAccumulator 同步 API、on_data Vec<u8>、trackDetachedChildPid 未移植、~/.pir/bin 等 12 项） | `02-design.md` §6.5、`coding-standards.md` 附录 A | 不需要 | 已回写 |
| D-012 | T07 | 实现细节 | SessionManager 与路径模块 Rust 落地差异（retainedTail 展开采 session-format.md/harness 行为、随机源自实现、serde default 修正、list/listAll 留 T12、typed 联合体降级边界 4 项、同步 IO 等 9+2 项） | `02-design.md` §6.3、§8，`01-requirements.md` §6.6 | 不需要 | 已回写 |
| D-013 | T08 | 实现细节 | compaction 移植 Rust 落地差异（算法层落 pir-agent::compaction + 触发接线 pir::core::compaction_runner、StreamOptions.reasoning 字段、session 共享函数下沉 3 项） | `02-design.md` §4.4、§6.4、§12 | 不需要 | 已回写 |
| D-014 | T09 | 实现细节 | settings 与资源加载 Rust 落地差异（同步写盘/fs2 flock、Settings 保序 map 与类型收窄、serde_yaml/TypeBox/SyntaxError 引擎级文案、description 截断按 Unicode scalar、sourceInfo 归 resource_loader、TUI 件下沉 T11/T12、extensions/packages 占位边界等 11 项） | `02-design.md` §6.7、§12 | 不需要 | 已回写 |
| D-015 | T10 | 实现细节 | headless 模式 Rust 落地差异（clap→手写解析器、provider 生态 T13 子集、--resume picker/子命令/--export 占位、docs 路径=exe dir、session_env 动态 cell、资源枚举确定性排序、SessionManager::list 提前等 7 项） | `02-design.md` §6.1、§6.3、§6.6、§12 | 不需要 | 已回写 |

## 4. 状态定义

| 状态 | 含义 |
|------|------|
| `待回写` | 已登记，尚未回写原始文档 |
| `已回写` | 已回写原始文档，待任务门禁确认 |
| `已关闭` | 门禁验收通过，偏离闭环 |
