# T21：Models refresh 事务化与 OAuth 行为

- **状态**：未开始
- **里程碑**：M2
- **依赖**：T17
- **上游对照**：`packages/ai/src/models.ts`（v0.84.0 重构：`context.stored`/`context.publish()`、generation 守卫、两阶段 refresh）、`99e34013d`（minOAuthValidityMs #7168）、`acbdc0d25`（15s 刷新超时 #7508）、`b784c8096`（刷新锁测试更替）、`credential-store.ts`（队列可中断）；测试：`models-runtime.test.ts`（+478）、`github-copilot-oauth.test.ts`（+154）
- **需求章节**：v0.11 需求 R2.5、R2.6；设计 §2.4、§2.5
- **预估**：0.5 人月

---

## 目标

Models refresh 重构为「两阶段 + generation 守卫 + 事务化发布」，OAuth 对齐「5 分钟提前刷新 +
15 秒硬超时 + 全操作可取消」。这是 v0.84.0 标注的核心破坏性变更，接口与时序照搬。

## 范围

### In

- **两阶段 refresh**：phase 1 无条件 restore（`allow_network=false` 也 restore，且在 auth 解析**之前**，失败也能恢复缓存目录）；phase 2 按需 fetch
- **generation 守卫**：`set_provider/delete_provider/clear_providers/refresh` 递增并取消上一代；发布必经 `publish_provider_models()` generation 检查，旧代丢弃；per-provider 串行发布链；快照写入（structuredClone 等价）
- `RefreshModelsContext` 重写：删 `store`，改为只读 `stored` 快照 + `publish({persist, update})`（`persist: None` 删除条目、省略不持久化）；`signal` 等价物必选
- 调用方给取消令牌时 `refresh()` 返回 `{aborted: true}`（provider 不配合 abort 也返回）；`providers` 定向刷新；错误按 provider 收集到 `errors` map
- `ModelsStreamTransforms` → `ModelsRequestTransforms` 重命名（header 变换作用于所有认证请求）
- **OAuth**：剩余 < 5 分钟即锁内刷新（原到期才刷）；`min_oauth_validity_ms` 覆盖（刷新后仍不足抛 `ModelsError::OAuth`）；刷新 15s 硬超时（与调用方取消 select）
- **凭证存储**：全部 auth 操作（read/list/modify/delete/login/logout/refresh 等）接受并遵守取消令牌；`InMemoryCredentialStore` 等价物排队等待被取消立即拒绝、队列尾不阻塞后续；登录后写凭证的竞态处理（mutation 开始前可取消）

### Out

- `rpi auth check`/`print-api-key` CLI 接线（T25）
- ModelRuntime 可用性刷新代际序列化（coding-agent 侧，T23）
- 扩展 `refreshModels` context 的 ABI 面（T27）
- llama 扩展按新 context 重写（T27 随 ABI 一并）

## 开发要点

- 时序是行为契约：「restore 先于 auth 解析」「发布按 provider 串行」用确定性测试锁死（上游 `models-runtime.test.ts` +478 行是蓝本）
- rpi 无 AbortSignal，统一用 `CancellationToken`；注意「必选」语义的类型表达（按值接收而非 Option）
- 15s 超时与调用方取消的组合：`tokio::select!` 语义对齐 `AbortSignal.any`

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] 两阶段 refresh：`allow_network=false` 仍 restore；restore 先于 auth 解析（顺序探针断言）
- [ ] generation 守卫：并发 setProvider + refresh 的旧代发布丢弃；per-provider 串行发布
- [ ] `publish({persist})` 三态（删除/不持久化/持久化）golden
- [ ] 调用方取消 → `{aborted: true}`（含 provider 不配合分支）
- [ ] OAuth：剩余 4 分钟触发刷新 / `min_oauth_validity_ms` 不足抛错 / 刷新 15s 超时 / 队列等待取消不阻塞后续

## 门禁验收

通用门禁 G1–G7 全过（G3：时序对拍；G2 附期望修改清单——刷新阈值语义变更）。

任务特有标准：

- [ ] `RefreshModelsContext` 新旧 API 对照表（上游 CHANGELOG before/after → rpi 等价）
- [ ] auth 操作取消令牌全覆盖清单（上游接口清单逐条核对）

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| （待登记） | | |

## 验收记录

（按 gates §3 模板填写）
