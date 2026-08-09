# D-053：登录/登出后 refresh 加有界信号（上游无界网络等待可挂死登录流）

- **状态**：已回写
- **关联任务**：T13（OAuth/api-key 登录接线）
- **级别**：实现细节偏离
- **发现日期**：2026-08-09

## 原文档约定

- 上游实现：`model-runtime.ts:503-514` `login`/`logout` ——
  `await this.refresh({ allowNetwork: this.modelNetworkEnabled })`，无
  AbortSignal、无超时；`remote-catalog-provider.ts:80-90` 的 `fetchWithRetry`
  也未传 `timeoutMs`。网络异常时（如远程 catalog 端点不可达）该 refresh
  可无限期等待，登录对话框（`showApiKeyLoginDialog`）会一直停在提交态。

## 实际实现与偏离原因

交互式 `/login` 的 api-key 路径：凭据写入成功后，`ModelRuntime::login` 在
`hide_selector()` 之前执行 refresh——刚配置好的 provider 触发远程 catalog
overlay 拉取（`{catalogBaseUrl}/api/models/providers/{id}`）。端点不可达时
（TCP 挂起无超时）登录流无限挂起：对话框停在提交回显、编辑器被遮罩、
仅 Ctrl+C 可退出——与上游同构的无界等待问题在真实环境复现（2026-08-09）。

修复：`login`/`logout` 的 refresh 改为在**有界信号**下执行——

1. 刷新超时上限（`model_refresh_timeout_ms` 解析值，默认
   `DEFAULT_MODEL_REFRESH_TIMEOUT_MS` = 15s，与 create 期刷新同一配置值，
   2026-08-09 审查后统一——此前此处硬编码默认常量，自定义超时不生效）；
2. 联动 `AuthInteraction::signal()`（登录对话框取消令牌）：用户 Ctrl+C 取消
   对话框时立即中止刷新，登录任务返回、对话框关闭、编辑器恢复。

健康网络下行为与上游一致（刷新正常完成、结果不对外暴露）；差异仅在
网络挂起时的恢复路径（上游永久挂死 → 本实现超时内或取消时恢复）。

## 回写位置

- `crates/rpi/src/core/model_runtime.rs` `login`/`logout` 文档注记
  （`bounded_refresh_signal`）

## 测试

- `login_interaction_cancel_aborts_hanging_post_login_refresh`：挂起 catalog
  服务器 + 取消交互信号 → 登录在 5s 内返回成功（refresh 中止不计错）
