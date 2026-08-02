# D-012：SessionManager 与路径模块的 Rust 落地差异

> 复制本模板为 `D-NNN-<short-slug>.md` 并填写。登记说明见 [README.md](./README.md)。

- **状态**：已回写
- **关联任务**：T07
- **级别**：实现细节偏离（逐条分析见下；T07-D1 经核实不影响对拍契约，不定行为级）
- **发现日期**：2026-08-02

## 原文档约定

- 文档与章节：`docs/02-design.md` §6.3（SessionManager）、§8（配置与路径）；`docs/01-requirements.md` §6.2、§6.6；`docs/coding-standards.md` 附录 A（依赖基线）
- 原文约定：SessionManager 行为对齐钉死版 `session-manager.ts`；路径解析单一模块对齐 `config.ts`；「写回时不丢数据」（需求 §6.6）；依赖选型以附录 A 基线为准

## 实际实现与偏离原因

T07 落地 `crates/pir/src/core/session_manager.rs`（行为）+ `crates/pir/src/config.rs`（路径模块）+ `crates/pir-ai/src/utils/uuid.rs`（id 生成）+ `crates/pir-agent/src/harness.rs`（SessionStorage trait 设计，T16 用）。与原文档/上游钉死版的差异逐项如下（任务书偏离表 T07-D1～D9 合并登记为本条）：

1. **retainedTail 形态展开进 context**（任务书 T07-D1）：compaction 转 context 消息时，`retainedTail` 形态展开为保留消息——采用上游自有基准文档 `docs/session-format.md`:327-342（"acts as a self-contained checkpoint"）与 harness `session.ts:123-127` 的行为；coding-agent 钉死版 `session-manager.ts` 的 `buildContextEntries` 只认 `firstKeptEntryId`，不展开。**不定行为级的理由**：主路径只写 `firstKeptEntryId` 形态（写纪律未变），`retainedTail` 仅出现在 harness 产物中（ADR-0003 §1 已定读取兼容）；fixtures 对拍语料无 compaction 条目，对拍契约不受影响。这是 D-001 单一来源化后 harness 形态必须选择一种读取语义时的自洽选择。
2. **随机源自实现**（T07-D2）：不引 `rand`/`uuid` crate（附录 A 基线外），`pir-ai/src/utils/uuid.rs` 自实现 uuidv7（与上游 `uuid.ts` 逐字节同构：48bit 大端时间戳、版本/变体位、同 ms 序列自增）/ uuidv4 + xorshift64* 非安全 PRNG（时间+pid 播种，遵循 `provider_retry.rs` 先例）。id 仅为标识符非凭据，会话内唯一性由 100 次碰撞重试兜底。
3. **字段级 serde 修正**（T07-D3）：`SessionHeader.cwd` 加 `#[serde(default)]`（古董 session 无 cwd）；`CustomMessageEntry.content` 加 `#[serde(default)]`（上游 `content ?? []`）。
4. **范围裁剪**（T07-D4）：`list`/`listAll`（session 发现列举，/resume UI 用）留 T12；上游 >512MB 文件测试未移植（V8 string 上限无 Rust 对应，流式读已由扫描上限/畸形行测试覆盖）。
5. **合法 JSON 非对象行加载即丢弃**（T07-D5）：如 `42`、`"s"`、`[...]` 行。上游 `JSON.parse` 成功即保留、迁移重写时写回；Rust typed 联合体无法表达，永久丢弃。仅影响人为损坏文件。
6. **header 发现要求完整 typed SessionHeader**（T07-D6）：上游发现路径只 duck-check `type==="session"` + `id` 为 string；Rust 要求完整可反序列化 header，缺字段文件不被 `find_most_recent_session` 发现（显式 `open` 的全量加载路径不受影响）。
7. **`find_most_recent_session` 微差**（T07-D7）：上游任一 `statSync` 抛错则整个发现返回 `null`，Rust 跳过该文件继续（更健壮）；`.jsonl` 这类隐藏文件名上游 `endsWith` 命中、Rust `extension()` 返回 `None` 不命中。均为极端边缘。
8. **未知字段数字格式化微漂移**（T07-D8）：Raw 保留为 JSON 语义级（与上游同级——上游 `JSON.parse`/`stringify` 同样规范化空白与数字），但 serde_json 无 `arbitrary_precision`：`1e2`/`100.0` 输出为 `100.0`（JS 输出 `100`）；>2^53 整数 Rust 保留精度（JS 截断）。key 顺序由 `preserve_order` 保障。
9. **形状不合法的已知条目降级为 Raw**（T07-D9）：已知 type 但字段形状不合法（如 message 条目缺 `parentId`）的条目降级为 Raw 保留、退出 context/model 推导；上游 duck-typing 会部分容忍（个别场景上游直接抛 TypeError）。降级条目写回不丢数据。

另有两项实现形态注记（不影响行为契约）：SessionManager 为同步 IO（TUI/CLI 调用方需自行 `spawn_blocking`）；`append_message` 运行期拒绝 compaction/branch summary 消息（上游仅 TS 类型层拒绝，属强化）。

## 影响面

- session 格式：仅第 1、5、8 条涉及，均为读取兼容/边缘场景，不改变主路径写出的字节形状（fixtures 归一化 diff 对拍通过）
- 其余：无（纯内部）

## 处置

- **回写位置**：`docs/02-design.md` §6.3（Rust 落地注记）、§8（路径模块已落地说明）；`docs/01-requirements.md` §6.6（降级策略的 typed 联合体边界注记）
- **回写日期**：2026-08-03
- **ADR**：不需要
