# T04：pir-ai Auth 基础

- **状态**：未开始
- **里程碑**：M1
- **依赖**：T03
- **上游对照**：`packages/ai/src/auth/*`、credential store 格式、OAuth 流程实现
- **需求章节**：§1.2.3（凭据结构兼容）、§5.3（auth 解析链、`/login` `/logout`）
- **预估**：0.5–1 人月（M1 共 3–4，与 T03 合计）

---

## 目标

实现鉴权解析链与凭据存储，打通 Anthropic OAuth 订阅登录，使 T10 的 headless
模式可以不依赖环境变量密钥运行。

## 范围

### In

- `CredentialStore`：JSON 文件、权限 0600、api_key 与 oauth token 分条目；**JSON 结构与 Pi 兼容**（可手动拷贝迁移登录态，需求 §1.2.3）
- auth 解析链：环境变量 → credential store → OAuth（`resolve_auth(provider, model)` → headers / signer，设计文档 §3.5）
- OAuth 框架：PKCE、device code、localhost 回调页（一次性本地 HTTP 服务，用 `axum`，已钉死）
- Anthropic OAuth 流程（`/login` / `/logout` 的底层能力；slash 命令接线在 T12）
- token refresh 与过期处理
- 凭据脱敏：不进入 Debug 输出、日志、错误消息（编码规范 §11.1/§11.2）

### Out

- 其余 provider 的 OAuth（Codex / Copilot 等，T13）
- 交互式 `/login` UI（T12）

## 开发要点

- 凭据文件创建用显式权限位，不依赖 umask（编码规范 §11.1）
- 凭据 JSON 结构用 fixtures 对拍验证兼容（字段名、嵌套形状逐项核对上游）
- localhost 回调页实现注意端口冲突与超时取消
- 路径解析走统一路径模块（编码规范 §10.1），本任务若需新增路径常量应落在该模块

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] 凭据文件创建后权限为 0600（测试中断言）
- [ ] 解析链优先级测试：env 覆盖 store、store 覆盖 OAuth 的语义与上游一致
- [ ] 凭据 JSON 与 Pi 样例 fixtures 对拍通过
- [ ] PKCE / device code 流程单测（mock 授权端点）
- [ ] token refresh：过期自动刷新、刷新失败语义与上游一致
- [ ] 脱敏测试：凭据值不出现在 `Debug` / 错误字符串中

## 门禁验收

通用门禁 G1–G7 全过（重点 G4 凭据不泄露、G5 凭据 JSON 形状对拍）。

任务特有标准：

- [ ] Anthropic OAuth 端到端 smoke（真实账号，记录结果；无条件时记录豁免理由）
- [ ] 凭据兼容性验证：Pi 生成的凭据文件可被读取解析（fixtures）

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| — | （暂无） | — |

## 验收记录

（待填写，模板见 `gates.md` §3）
