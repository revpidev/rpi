# D-020：harness 层（T16）Rust 落地差异

> 复制本模板为 `D-NNN-<short-slug>.md` 并填写。登记说明见 [README.md](./README.md)。

- **状态**：已回写
- **关联任务**：T16
- **级别**：实现细节偏离
- **发现日期**：2026-08-06

## 原文档约定

- 文档与章节：`docs/02-design.md` §4.5（compaction 共享常量）、§12 模块映射表（harness 行）；`docs/plan/v0.1/T16-agent-harness.md`「范围/开发要点」
- 原文约定：
  1. T08/T16 计划口径：「compaction 共享常量与算法由 T08 统一落地 `rpi-agent::compaction`，T16 harness 复用」（设计 §4.5、T16 任务文件「复用 T08/T09 的共享常量与算法」）。
  2. §12 映射：`packages/agent/src/harness/*` → `crates/rpi-agent/src/harness/*`。

## 实际实现与偏离原因

1. **harness compaction 变体分歧（对计划口径的勘误）**：实测钉死版上游（`node --experimental-strip-types` 直跑源码验证），harness `compaction.ts` 的 `prepareCompaction` 与 coding-agent 版**并不相同**——空 summarize 集不提前返回（返回 defined、toSummarize=0、retainedTail=2），且准备结果带 `retainedTail`。T08 统一的 `rpi_agent::compaction::prepare_compaction` 是 coding-agent 变体。处置：LLM 调用/估算/切点/文件操作仍复用 T08 模块，但 harness 变体的 `prepare_harness_compaction`（`harness/compaction.ts:640-713` 忠实移植）落在 `crates/rpi-agent/src/harness/agent_harness.rs:431`；`extract_file_operations` 为此升 `pub(crate)`。
2. **SessionStorage 写方法 `&mut self` → `&self` + tokio Mutex**：上游 Session 门面读写一体；Rust 初版 trait 骨架（T07 期）写方法取 `&mut self`，经 `Arc<dyn SessionStorage>` 不可达。两存储实现的可变状态包入 `tokio::sync::Mutex`；JSONL 写路径持锁跨 `append_file().await` 以保证并发下文件行序==内存序（先例：`rpi::core::agent_session.rs:1874`）。
3. **SessionManager::build_index leaf 重放兼容**：主路径原对所有记录一律 `leaf_id = 自身 id`；harness 产物以 `leaf` 记录收尾时两实现 leaf 重建分歧。修复：build_index 遇 `type=="leaf"` 记录按 `targetId` 推进（null 清空），与 harness `leafIdAfterEntry`（jsonl-storage.ts:134-136）对齐。主路径自身永不写 leaf 记录，既有行为与对拍契约不变（T07 测试零回归）。
4. **harness 自带 skills/prompt-templates/system-prompt 独立移植**：上游 harness 版与 coding-agent 版本为双份（文案/签名不同，如 harness `formatSkillsForSystemPrompt` 无 read-tool 门控、`parseCommandArgs` 仅空格/Tab 切分）；且依赖方向禁止 `rpi-agent → rpi`。落 `harness/{skills,prompt_templates,system_prompt}.rs`。
5. **依赖落位**：`reqwest`（stream_proxy）/`serde_yaml`+`ignore`（skills/templates）/`unicode-normalization`（edit fuzzy/路径变体）/`base64`（read 图片）/`libc`（进程树击杀）加入 `rpi-agent` 依赖表——全部在编码规范附录 A 基线内，仅落位 crate 不同。
6. **惯用法转换**（与 D-002/D-010 同类）：AbortSignal→CancellationToken；错误族 thiserror 枚举化（code 字面值锚定）；`on` hook 结果以 `HarnessHookResult` 枚举承载、事件键为 type tag 字符串；subscribe 屏障用 agent.rs 同款按订阅序逐个 inline await；TS 泛型收敛（`TContext` 默认 `()`）；`SessionRepo::fork` 的 `SessionForkOptions & TCreateOptions` 交叉类型拆为固有方法（全选项）+ trait 方法（全量拷贝）。
7. **env/tools/truncate/proxy 的局部等价**：bash 超时 `Option<u64>` 整秒（校验消息保留上游原文，0.01s 实为 1s）；`&[u8]` 取代 `string | Uint8Array`；truncate 的 Buffer 快路径/孤立 surrogate 机制在 Rust 恒等省略；`formatSize` 用整数算术复刻 `toFixed(1)` 半进舍入；jsdiff→手写 Myers O(ND)（11 组逐字节 golden 锁定）；proxy SSE 未复用 `rpi-ai::api::sse`（其按空行派发+event 字段，proxy.ts 是逐 `data: ` 行独立成事件），私有增量 UTF-8 解码器镜像 `TextDecoder{stream:true}`；proxy 跳过 contentIndex 视为协议错误（JS 数组 hole 无对应物）；file-mutation-queue 的 `WeakMap<ExecutionEnv>` 以 env 地址键控全局 map 替代。
8. **失败路径适配**：钉死的 `StreamFn`/loop hook 形状不可失败，无法如上游从 `runAgentLoop` throw；改为 per-run 失败格 + stream wrapper 入口闸门合成与 `createFailureMessage` 完全相同的错误消息走正常事件路径，可观察结果（持久化消息、事件序列、prompt resolve 形状）与上游一致，由「hook 失败落成持久化错误消息」测试锚定。
9. **`AgentHarnessOptions.model` 非可选**（2026-08-06 审查补登）：上游声明类型本就为 `model: Model<any>` 非空（types.ts:935），但 agent-harness.ts:741/:822 有两个防御性 `No model set` guard；Rust 构造上不可达，属 API 简化。
10. **`CustomMessageEntry` 线序**（2026-08-06 审查修复）：struct 字段序改为 `display` 在 `details` 前，含 details 时与 harness 写序（session.ts:301-309）逐字节一致；coding-agent 主路径写序（session-manager.ts:1178-1186，id/parentId/timestamp 居尾）无法用单一 struct 表达，JSON 键序无语义、对拍归一化按解析值比较，注释注明。

## 审查修复轮（2026-08-06，验收后追加）

对照逐行审查报告修复并复验：
- `read.rs` 对抗性 limit 的 `i64` 溢出 → saturating 运算 + 7 组对抗用例；
- `edit_diff.rs` 全轨迹 Myers O(D²) 空间 → checkpoint 分治回溯 O(D log D) 空间（2×4000 全异行 ~1.1GB→~4MB），与 jsdiff 逐字节一致由 11 golden + 132,496 穷举对锁定（middle-snake 路线经反例证明不可能逐字节一致，见模块头）；
- `file_mutation_queue.rs` `Box::leak` 永久泄漏 → 按需 GC 的 Arc 注册表 + RAII guard，模块头失实表述修正；
- `session_facade.rs` append 三段式非原子 → facade `append_lock` 串行化（跨实例单写者约束已文档化）+ 64 任务并发链完整性测试；
- `agent_harness.rs` save_point 不再传 signal（对齐 agent-harness.ts:554）；
- `parse_iso8601_ms` 接受可选毫秒与时区变体（jsonl_repo 排序面）；`read_text_lines` 凑够 maxLines 早停（FIFO 测试直证）；bash 首帧 leading-edge（对齐 lastUpdateAt=0 哨兵）；
- 补 4 个失败路径测试（drain requeue / idle 抛错 / 二次失败聚合 / flush 失败覆盖）+ 27 个 harness 测试全量 timeout 护栏；nodejs_env_test 临时目录 Drop 清理、proxy_test 断连噪音、prompt_templates 自引用断言收紧。

## 影响面

- session 格式：无变更（leaf 修复仅为读取兼容增强，主路径写出格式不变）；互通对拍（`rpi/tests/parity_harness_interop_test.rs`）锁定 harness↔主路径双向一致。
- 协议 / 扩展 API / TUI 行为：无。

## 处置

- **回写位置**：`docs/02-design.md` §6.4（compaction 落地注记补 T16 变体勘误）、§12（harness 映射行补 T16 注记）；`docs/plan/v0.1/T16-agent-harness.md`（设计细化记录与本偏离登记）
- **回写日期**：2026-08-06
- **ADR**：不需要
