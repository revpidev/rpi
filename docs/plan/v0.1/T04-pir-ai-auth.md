# T04：pir-ai Auth 基础

- **状态**：未开始
- **里程碑**：M1
- **依赖**：T03
- **上游对照**：`packages/ai/src/auth/*`（resolve.ts、credential store、oauth/anthropic）、`docs/providers.md`
- **需求章节**：§1.2.3（凭据结构兼容）、§5.4（auth 全节）
- **预估**：0.5–1 人月（M1 共 3–4，与 T03 合计）

---

## 目标

实现鉴权解析链与凭据存储，打通 Anthropic OAuth 订阅登录（含 Claude Code 伪装），
使 T10 的 headless 模式可以不依赖环境变量密钥运行。

## 范围

### In

- `CredentialStore`：JSON 文件、权限 0600；credential 判别式 `{type:"api_key",key?,env?} | {type:"oauth",refresh,access,expires,...}`；**JSON 结构与 Pi `auth.json` 兼容**（可手动拷贝迁移登录态，需求 §1.2.3）
- **key 值解析 DSL**（auth.json 与 models.json 通用）：`!cmd` 执行命令取 stdout、`$VAR`/`${VAR}` 插值、`$$`/`$!` 转义
- auth 解析链（顺序与上游一致，**不得调换**）：显式 `options.apiKey` → **credential store（命中即停，拥有 provider）** → ambient（env var / AWS profile / ADC）；OAuth 是 credential 的一种类型而非独立兜底
- `modify` 唯一写路径：按 provider 串行化 read-modify-write + 跨进程文件锁（`fs2`）；`list()` 只返回 `{providerId,type}` 不解析密钥
- token refresh：过期时在 `modify` 锁内双重检查刷新；**刷新失败抛 `ModelsError("oauth")` 且绝不静默回退 env key**（保留 credential 供重登）
- env 变量表（T03/T04 范围 provider；Anthropic 三变量优先级 `ANTHROPIC_AUTH_TOKEN` > `ANTHROPIC_OAUTH_TOKEN` > `ANTHROPIC_API_KEY`；`ANTHROPIC_AUTH_TOKEN` 命中时产生 `Authorization: Bearer` 头（非 `x-api-key`，providers/anthropic.ts:21-27））
- OAuth 框架：PKCE、device code（RFC 8628：默认 5s、slow_down +5s、下限 1s、WSL 时钟漂移文案）、localhost 回调页（一次性本地 HTTP 服务，用 `axum`，已钉死）
- `AuthInteraction` trait：prompt（text/secret/select/manual_code，per-prompt signal 竞速取消）+ notify（links/auth_url/device_code/progress）
- Anthropic OAuth 流程（`/login` `/logout` 的底层能力；slash 命令接线在 T12）+ **Claude Code 身份伪装**（版本 2.1.75：`user-agent` 为 `claude-cli/2.1.75`、`x-app`、beta 头 `claude-code-20250219` 与 `oauth-2025-04-20`、system 前缀 "You are Claude Code, Anthropic's official CLI for Claude."，anthropic-messages.ts:76,897-898,974）+ **工具名 canonical 大小写映射表**（anthropic-messages 适配器侧接线）
- `options.env` 每请求环境覆盖
- 凭据脱敏：不进入 Debug 输出、日志、错误消息（编码规范 §11.1/§11.2）

### Out

- 其余 6 个 OAuth 流程（Codex / Copilot / OpenRouter / Kimi / xAI / Radius，T13）
- provider 自有 login（Bedrock/Vertex/Cloudflare 多字段 prompt，T13）
- 交互式 `/login` UI（T12）

## 开发要点

- 凭据文件创建用显式权限位，不依赖 umask（编码规范 §11.1）
- 凭据 JSON 结构用 fixtures 对拍验证兼容（字段名、嵌套形状逐项核对上游）
- localhost 回调页实现注意端口冲突与超时取消；与 manual_code 竞速取消
- 路径解析走统一路径模块（编码规范 §10.1），本任务若需新增路径常量应落在该模块
- 工具名映射表逐字移植（漏映射会导致 Claude 订阅 OAuth 请求被拒）；canonical 共 17 个名字：Read, Write, Edit, Bash, Grep, Glob, AskUserQuestion, EnterPlanMode, ExitPlanMode, KillShell, NotebookEdit, Skill, Task, TaskOutput, TodoWrite, WebFetch, WebSearch（anthropic-messages.ts:81-99）

## 进度跟踪

- [ ] 设计细化
- [ ] 实现
- [ ] 自测
- [ ] 门禁验收
- [ ] 文档回写

## 自测清单

- [ ] 凭据文件创建后权限为 0600（测试中断言）
- [ ] 解析链优先级测试：显式 key > store（命中即停）> env/ambient；store 命中后不再查 env
- [ ] OAuth 刷新失败：抛错且不回退 env key（测试断言）
- [ ] key DSL：`!cmd` / `$VAR` / 转义各形态
- [ ] 凭据 JSON 与 Pi 样例 fixtures 对拍通过
- [ ] PKCE / device code 流程单测（mock 授权端点，含 RFC 8628 轮询参数）
- [ ] `modify` 串行化与文件锁：并发写不撕裂
- [ ] 伪装与工具名映射：请求头与工具名表 fixtures 比对
- [ ] 脱敏测试：凭据值不出现在 `Debug` / 错误字符串中

## 门禁验收

通用门禁 G1–G7 全过（重点 G4 凭据不泄露、G5 凭据 JSON 形状对拍）。

任务特有标准：

- [ ] Anthropic OAuth 端到端 smoke（真实账号，记录结果；无条件时记录豁免理由）
- [ ] 凭据兼容性验证：Pi 生成的凭据文件可被读取解析（fixtures）
- [ ] 需求 §5.4 条目（本任务范围内）逐条核对有测试锚点

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| — | （暂无） | — |

## 验收记录

（待填写，模板见 `gates.md` §3）
