# T23：主路径会话行为簇

- **状态**：未开始
- **里程碑**：M2
- **依赖**：T18、T19、T22
- **上游对照**：`32850ef7c`+`e56893f4c`+`8eda4f5b2`+`3852cb2b8`（length-stop 恢复链四连）、`97f0ccdd9`（settings 深合并 #7572）、`b0e05b442`（工具图片规范化）、`b04faa2da`+`7443...`+`7153...`（--model 歧义 / #7366）、`8f9e76974`+`c6eb6281a`+`a077fff0b`（可用性刷新代际）、`d2be68dbe`+`8ee220782`+`57cde8690`+`7cf90c1d1`+`2c79ce453`+`4d68d9355`（凭证串行化）、`e741cb05c`（credential-resolved baseUrl）、`agent-session-runtime.ts`（teardown 先 abort #7022）、`6ca423447`（事件总线泄漏）、`4e64de695`（bash PI_* 软化 #7128）、`523b5a491`+`d4eaf052b`（find 相对化）、`46b53b995`（fetchWithRetry）、`da66636cc`（symlink session 目录）、`4f4762f06`（AI_AGENT）；回归蓝本：`7253-manual-compact-during-response`、`7150-rpc-prompt-during-compaction`、`7027-credential-refresh-hang`
- **需求章节**：v0.11 需求 R3.4（全部 12 条）、R3.2.5（AI_AGENT）；设计 §5.3
- **预估**：0.6 人月

---

## 目标

coding-agent 主路径的 12 条会话行为修正整体落地。核心难点是 length-stop 恢复链
（四个上游 commit 相互依赖），必须作为一个不可分割单元实现。

## 范围

### In

按设计 §5.3 的实现顺序：

1. **length-stop 恢复链**（R3.4.1 + R3.4.2，四 commit 整体移植）：`is_recoverable_length()` 判定（T19 提供）→ 自动 compaction → **单次重试**；`_overflow_recovery_attempted` 在 length stop 后不重置；compaction 进行中 `prompt()` 返回错误（原静默丢失）；`compaction_end` 发出**前**清 abort controller 使 queued prompts 可提交；手动/自动 compaction 竞态修复；TUI 截断提示改中性文案（接线属 T28 之后的交互层，本任务改文案常量与逻辑）
2. **settings 递归深合并**（R3.4.3）：对象递归、其余覆盖；#7572 场景（项目级局部 `retry.provider` + 全局其他字段）测试锁死
3. **工具结果图片统一规范化**（R3.4.4）：`normalize_tool_result_images()` 挂 `after_tool_call`，在扩展 `tool_result` hook **之后**执行；`images.autoResize` 可关；失败保留原图
4. **--model 精确 ID 歧义**（R3.4.5）：多 provider 命中 → 唯一已认证优先，0 或 >1 认证报歧义错误；`/model`、`/scoped-models` 走缓存快照即时渲染
5. **ModelRuntime 可用性刷新代际序列化**（R3.4.6）：代际计数防 stale 发布；强制刷新不被 stalled 阻塞；`/model` 失败列出每个失败 catalog
6. **凭证操作串行化**（R3.4.7）：login/logout/setRuntimeApiKey/removeRuntimeApiKey 串行 map；`CredentialSynchronizationError`；读取前重载；锁 convoy 消除
7. **credential-resolved baseUrl 保留**（R3.4.8）：`_getRequiredRequestAuth`/`_getSummarizationRequestAuth` 等价物返回带 baseUrl 覆盖的 model
8. **teardown 先 abort 持久化**（R3.4.9）：先 `await session.abort()` 再发 `session_shutdown`；session 发现支持 symlink 目录
9. **扩展事件总线退订**（R3.4.10）：`invalidate()` 统一退订
10. **杂项**（R3.4.11/R3.4.12）：bash `PI_*` 提示软化文案；find 相对化重写（trailing separator、Windows `[/\\]`）；管理 HTTP `fetch_with_retry()`（408/425/429/5xx + 总超时预算，仅 version-check/catalog/managed-tool/package）
11. **`AI_AGENT=pir`** 子进程环境（R3.2.5，按 APP_NAME 派生惯例）

### Out

- `/settings` 热切换 UI 模式（T32）
- 扩展 API 面对外暴露（scopedModels 等，T27）
- auth CLI 命令（T25）

## 开发要点

- 恢复链四 commit 单独一个 PR/一个验收单元；配并发竞态测试（compaction 中 prompt、manual vs auto 竞态）
- settings 深合并改动影响面大（所有 settings 消费方），先跑全量既有测试确定期望变化清单
- 凭证串行化注意与 T21 的 credential-store 取消语义叠加

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] 恢复链：可恢复截断 → compact + 单次重试事件序对拍；不可恢复（output ≥ limit）不触发；重试仅一次
- [ ] compaction 期间 prompt 拒绝 + queued prompt 在 `compaction_end` 前可提交（移植 `7253`/`7150` 回归）
- [ ] settings 深合并 #7572 场景 golden；全量 settings 消费方回归
- [ ] 图片规范化：内置工具/扩展注入/扩展替换三路径 + autoResize 关闭 + 失败保留原图
- [ ] --model 歧义三态（唯一认证/零认证/多认证）golden
- [ ] 凭证串行：并发 login/logout 不丢更新；`CredentialSynchronizationError` 路径；`7027-credential-refresh-hang` 移植
- [ ] teardown 先 abort（#7022 场景：进行中回合的工具结果持久化到 outgoing session）
- [ ] find 相对化（trailing separator）、fetch_with_retry 状态码表、AI_AGENT 环境断言

## 门禁验收

通用门禁 G1–G7 全过（G3 强制：恢复链事件序 fixtures；G2 附期望修改清单——settings 合并与文案变更）。

任务特有标准：

- [ ] 需求 R3.4 十二条逐条核对表（上游 commit + 测试锚点）
- [ ] 恢复链四 commit 作为一个验收单元，验收记录附并发竞态测试输出

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| （待登记） | | |

## 验收记录

（按 gates §3 模板填写）
