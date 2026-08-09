# T22：rpi-agent 循环微行为与 compaction 契约

- **状态**：未开始
- **里程碑**：M2
- **依赖**：T17
- **上游对照**：`0f5286d8a`（shouldStopAfterTurn #7367）、`1eb988cfe`（terminate #7715）、`1532c9994`（reset 抛错 #7717）、`44289550a`（compaction 契约收紧）、`80ef7ff0f`（branch summary timestamp）
- **需求章节**：v0.11 需求 R4.1、R4.2、R4.3.2（rename_file 预留）；设计 §3
- **预估**：0.25 人月

---

## 目标

Agent 循环四项微行为与 compaction 契约收紧；harness 层明确不动（保持 v1 语义），
仅在文件系统抽象预留 `rename_file()`。

## 范围

### In

- `AgentOptions::should_stop_after_turn`：turn 结束后、轮询队列前调用；回调第二参数带取消令牌
- `BeforeToolCallResult::terminate`：与 `block: true` 组合；整批工具结果全部 terminate 时跳过后续模型调用直接结束回合（agent-loop 判定逻辑）
- `Agent::reset()` 活跃 run 期间返回错误（原静默清状态）
- `CompactionResult` → `CompactResult`：删 `first_kept_entry_id`；`retained_tail` 必填；`extract_file_operations` 去掉 `from_hook` 检查；cut-point 只认 `branch_summary`；branch summary 函数 timestamp 接受 `number|string` 等价（Rust：epoch ms 或 ISO 字符串）
- `FileSystem` 等价抽象新增 `rename_file()`（原子发布预留；唯一跟进的 v4 元素）
- `harness/` 模块文档标注：上游 v2 为 scaffold，rpi 保持 v1 语义（引用 `harness-v2.md` §20）

### Out

- session v4 lane 存储、三后端、conformance 套件（[DEFER]，需求 R4.3）
- harness v2 运行时/事件总线/遥测 schema（[DEFER]，需求 R4.4）
- length-stop 恢复链的会话侧接线（T23）

## 开发要点

- `CompactResult` 是重命名 + 字段变更：全 workspace 调用点由编译器带出，注意 session JSONL 序列化形状是否受影响（若有，走 G2 期望清单 + fixtures 重新生成）
- terminate 判定是「整批全部 terminate 才提前停」，部分 terminate 不生效——边界用例锁死
- `rename_file()` 预留即实现到 trait + 各实现（本地 fs），但不要求调用方

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] should_stop_after_turn：turn 结束触发、队列非空不继续、取消令牌传播
- [ ] terminate：整批终止提前结束回合；部分 terminate 仍继续模型调用
- [ ] reset 活跃期报错 / 空闲期正常
- [ ] CompactResult 契约：cut-point 只认 branch_summary、retained_tail 必填编译期保证、extract_file_operations 不再滤 from_hook
- [ ] rename_file 原子性（同目录 rename）冒烟

## 门禁验收

通用门禁 G1–G7 全过（G4 重点：无 session v4 概念、无 harness v2 运行时）。

任务特有标准：

- [ ] 需求 R4.1 四条 + R4.2 逐条核对表（上游 commit + 测试锚点）
- [ ] v0.1 harness 对拍测试（T16 交付物）保持全绿——证明「保持 v1 语义」未被破坏

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| （待登记） | | |

## 验收记录

（按 gates §3 模板填写）
