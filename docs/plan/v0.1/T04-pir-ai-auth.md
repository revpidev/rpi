# T04：pir-ai Auth 基础

- **状态**：已完成
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

## 设计细化（2026-07-31）

T03 已交付：`resolve_provider_auth` 解析链（显式 key → store 命中即停 → ambient）、
`InMemoryCredentialStore`、`Credential`/`CredentialStore`/`AuthContext` 类型、
`options.env` 覆盖、anthropic-messages 适配器侧的 Claude Code 伪装头与
`to/from_claude_code_name` 工具名映射。本任务在其上补齐：

模块划分（均在 `pir-ai`，文件名镜像上游，无 `mod.rs`）：

- `auth/config_value.rs` — 移植 `packages/coding-agent/src/core/resolve-config-value.ts`：
  `!cmd`（`/bin/sh -c`，10s 超时，stdout trim，空/失败 → `None`，进程级结果缓存）、
  `$VAR`/`${VAR}` 插值、`$$`/`$!` 转义；含 `get_config_value_env_var_name(s)`、
  `is_command_config_value`、`resolve_config_value[_uncached|_or_throw]`、`resolve_headers` 全集
  （models.json 侧 T09 复用）。
- `auth/file_store.rs` — 移植 `coding-agent/src/core/auth-storage.ts`（`AuthStorage` +
  `FileAuthStorageBackend`）：JSON 文件 0600（显式权限位 + 写后 chmod，不依赖 umask）、
  父目录 0700、`fs2` 跨进程排他锁（10 次指数回退重试 100ms→10s，对齐 proper-lockfile
  retries；stale 检测 fs2 无对应物，编码规范 §9.2 已钉死 fs2，不算偏离）、进程内按
  provider 互斥串行、内存快照（reload 失败保留旧快照）、`read()` 对 `api_key.key`
  跑 DSL 解析、`list()` 只读元数据绝不执行 `!cmd`。默认路径解析（`~/.pir/agent/auth.json`）
  属统一路径模块（T09），本任务 store 仅接受显式路径。
- `auth/env_keys.rs` — 移植 `env-api-keys.ts`：静态 env 映射表整表数据级移植（T13 复用）
  + anthropic 三变量特例；`find_env_keys` / `get_env_api_key`；vertex ADC / bedrock
  ambient `<authenticated>` 分支随 T13（provider 自有 login 范围，本任务 Out）。
- `auth/helpers.rs` — `env_api_key_auth` 移植（stored key 优先 → envVars 顺序兜底，
  含 `login` secret prompt）。
- `auth/interaction.rs` — `AuthPrompt`（text/secret/select/manual_code + per-prompt
  `CancellationToken` 竞速取消）、`AuthEvent`（info/auth_url/device_code/progress）、
  `AuthInteraction` trait（`prompt` 返回 `Result<String, ModelsError>`，取消即 Err）。
- `auth/oauth/pkce.rs` — `oauth2` crate（附录 A 已批）：32 字节随机 verifier、
  S256 challenge，base64url 无 padding。
- `auth/oauth/device_code.rs` — 移植 `device-code.ts` 轮询框架：默认 5s、slow_down
  优先用服务端 interval 否则 +5s、下限 1s、WSL 时钟漂移文案逐字、`CancellationToken`
  取消文案 "Login cancelled"。
- `auth/oauth/callback_page.rs` — `oauth-page.ts` HTML 逐字移植 + axum 一次性回调服务
  （`127.0.0.1:53692/callback`；host 可由 `PIR_OAUTH_CALLBACK_HOST` 覆盖——ADR-0001 §2
  统一 `PIR_` 前缀，非偏离）；404/400/state mismatch/成功四种响应与上游一致。
- `auth/oauth/anthropic.rs` — 移植 `auth/oauth/anthropic.ts`：常量逐字（CLIENT_ID 为
  base64 串解码、AUTHORIZE_URL/TOKEN_URL/CALLBACK_PORT 53692/SCOPES）、`state=verifier`、
  `expires = now + expires_in*1000 - 5min`、manual_code 与回调竞速（回调赢则取消
  prompt）、token 交换/刷新 POST JSON 30s 超时、错误消息格式对齐上游、
  `to_auth` → `api_key = access`。
- trait 扩展：`ApiKeyAuth` 增加 `login`/`check`（默认实现保持 T03 现有实现兼容），
  `OAuthAuth` 增加 `login`；anthropic `ApiKeyAuth.resolve` 特例：stored key 优先，
  `ANTHROPIC_AUTH_TOKEN` → `Authorization: Bearer` 头，`ANTHROPIC_OAUTH_TOKEN` /
  `ANTHROPIC_API_KEY` → `apiKey`（providers/anthropic.ts:9-38）。
- 脱敏（编码规范 §11.1）：`ApiKeyCredential`/`OAuthCredential`/`Credential`/`ModelAuth`/
  `AuthResult` 去掉 derive Debug，手写 redacted Debug（secret 值输出 `[redacted]`）；
  同步修 T03 测试里对 Debug 形状的依赖。
- 依赖：workspace 增加 `fs2`、`oauth2`、`axum`（附录 A 已批三者）。
- fixtures：`fixtures/generated/auth/auth.json` 样例（与上游 `JSON.stringify(data, null, 2)`
  输出字节对齐，含 api_key + oauth 两种条目），serde 反序列化/序列化双向上拍。

测试意图移植（编码规范 §12.2，同名 Rust 测试）：

- `coding-agent/test/resolve-config-value.test.ts` → `config_value.rs` 单测
- `coding-agent/test/auth-storage.test.ts` → `file_store.rs` 单测（含 0600 断言、并发写不撕裂）
- `ai/test/anthropic-auth-token.test.ts` → anthropic env 解析链单测（Bearer 头优先级）
- `ai/test/anthropic-oauth.test.ts` → `oauth/anthropic.rs` 单测（mock HTTP 端点）
- `ai/test/oauth-device-code.test.ts` → `oauth/device_code.rs` 单测

## 进度跟踪

- [x] 设计细化
- [x] 实现
- [x] 自测
- [x] 门禁验收
- [x] 文档回写

## 自测清单

- [x] 凭据文件创建后权限为 0600（测试中断言）——`file_store.rs::auth_file_is_created_with_0600_and_parent_dir_with_0700`
- [x] 解析链优先级测试：显式 key > store（命中即停）> env/ambient；store 命中后不再查 env——`resolve.rs`：`explicit_api_key_wins_over_stored_credential`（显式路径 store 零读取）、`stored_key_wins_and_env_is_never_consulted`（命中后零 env 调用）、`keyless_stored_credential_still_resolves_env_through_handler`（命中即停边界）、`ambient_env_resolves_when_store_is_empty`
- [x] OAuth 刷新失败：抛错且不回退 env key（测试断言）——`resolve.rs::oauth_refresh_failure_errors_without_env_fallback`（Err code=oauth、credential 保留、env 变量在场也不回退）；锁内双检查 `oauth_refresh_is_double_checked_under_the_store_lock`
- [x] key DSL：`!cmd` / `$VAR` / 转义各形态——`config_value.rs` 11 个单测（模板解析/转义/命令执行与缓存/or_throw 文案/headers）；`file_store.rs::dsl_fixture_keys_resolve_on_read`
- [x] 凭据 JSON 与 Pi 样例 fixtures 对拍通过——`fixtures/generated/auth/auth.json`（已用 node 验证 === `JSON.stringify(parse, null, 2)`）；`file_store.rs::pi_auth_json_fixture_parses`、`serialization_matches_fixture_bytes`
- [x] PKCE / device code 流程单测（mock 授权端点，含 RFC 8628 轮询参数）——`oauth/pkce.rs` 4 测（含 RFC 7636 向量）、`oauth/device_code.rs` 6 测（默认 5s/slow_down +5s/服务端 interval/下限 1s/WSL 文案/取消）、`oauth/anthropic.rs` 10 测（mock token 端点）
- [x] `modify` 串行化与文件锁：并发写不撕裂——`file_store.rs::concurrent_same_provider_modifies_are_serialized`、`serializes_concurrent_modifications`、`does_not_write_after_lock_acquisition_failure_and_recovers_on_retry`
- [x] 伪装与工具名映射：请求头与工具名表 fixtures 比对——T03 已落地并验收（`anthropic_messages.rs::test_build_request_headers_oauth`、`test_convert_messages_oauth_tool_names`、`test_build_params_oauth_system_and_identity`）；本任务集成链 `tests/auth_oauth_resolve.rs` 验证 OAuth credential → `to_auth` → api_key（适配器侧 `sk-ant-oat` Bearer 伪装由 T03 锚点覆盖）
- [x] 脱敏测试：凭据值不出现在 `Debug` / 错误字符串中——`types.rs::debug_output_redacts_credential_secrets`（key/refresh/access/api_key/Authorization 头值全部 `[redacted]`）；`oauth_refresh_failure_errors_without_env_fallback` 断言错误消息不含 env key 值

## 门禁验收

通用门禁 G1–G7 全过（重点 G4 凭据不泄露、G5 凭据 JSON 形状对拍）。

任务特有标准：

- [x] Anthropic OAuth 端到端 smoke（真实账号，记录结果；无条件时记录豁免理由）——**豁免**：无真实 Anthropic 订阅账号且测试纪律禁止外网访问（编码规范 §12.4）；mock 端点已覆盖请求/响应线格式与状态机（`oauth/anthropic.rs` 10 测 + `callback_page.rs` 9 测），login/refresh 请求体、错误形状与上游逐字对齐。有账号条件时按 `oauth/anthropic.rs` 常量直跑即可。
- [x] 凭据兼容性验证：Pi 生成的凭据文件可被读取解析（fixtures）——`pi_auth_json_fixture_parses`（api_key + oauth 含 extra 字段 `accountId`）、`serialization_matches_fixture_bytes`（双向字节对拍）
- [x] 需求 §5.4 条目（本任务范围内）逐条核对有测试锚点——映射表：

| 需求 §5.4 条目 | 测试锚点 |
|----------------|----------|
| 解析顺序 + 命中即停 + 锁内双检查刷新 + 刷新失败不回退 | `resolve.rs` 7 测（见上） |
| CredentialStore 契约（read/list/modify/delete、modify 唯一写路径、跨进程锁、list 元数据不解析） | `file_store.rs` 15 测 + `list_never_executes_configured_commands` |
| credential 判别式与 auth.json 兼容（0600） | fixture 对拍 3 测 + 0600 断言 |
| env 变量表 + Anthropic 三变量优先级（Bearer vs x-api-key） | `anthropic_auth.rs` 3 测、`env_keys.rs` 4 测（含 `static_mapping_table_matches_upstream` 整表核对） |
| key 值解析 DSL | `config_value.rs` 11 测 |
| OAuth：anthropic（PKCE + 回调/manual_code 竞速） | `oauth/anthropic.rs` 10 测 + `callback_page.rs` 9 测 + `pkce.rs` 4 测 |
| device 轮询 RFC 8628 参数 | `oauth/device_code.rs` 6 测 |
| AuthInteraction 交互协议 | `interaction.rs`（类型 + serde 2 测）；竞速取消由 `login_resolves_through_the_manual_code_prompt_and_aborts_it_after_settling`、`prompt_cancellation_propagates` 锚定 |
| `options.env` 每请求覆盖 | T03 `resolve.rs` overlay 实现 + `stored_credential_wins_over_all_env_vars` / `uses_credential_scoped_environment_before_process_env` |
| （范围外，T13）其余 6 OAuth 流程、provider 自有 login、vertex/bedrock ambient | — |

## 偏离记录

| 偏离 ID | 摘要 | 状态 |
|---------|------|------|
| D-008 | auth 存储与 key DSL 的 Rust 落地差异（fs2 无 stale/compromised、jitter 随机源、`!cmd` 仅 unix、快照保序方案、resolve_headers 形状） | 已回写 |
| D-009 | OAuth 框架的 Rust 落地差异（时钟抽象、测试缝、回调服务分支、错误明细近似、token JSON 严格化、竞速实现） | 已回写 |

## 验收记录

- 验收日期：2026-07-31
- 验收人：kimi（单人开发，逐项自证）
- G1 构建/静态检查：通过（`cargo build --workspace` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo fmt --all -- --check` 全部 exit 0，无警告）
- G2 测试：通过（workspace 384 passed, 0 failed；T04 新增 70：config_value 11、file_store 15、env_keys 4、helpers 3、anthropic_auth 3、interaction 2、resolve 7、pkce 4、device_code 6、callback_page 9、oauth/anthropic 10、types 脱敏 1、集成 1，部分文件含子断言；无 live 测试；无真实网络）
- G3 对拍：通过（`fixtures/generated/auth/auth.json` 与上游 `JSON.stringify(data, null, 2)` 输出字节对拍——已用 node 交叉验证；`pi_auth_json_fixture_parses` / `serialization_matches_fixture_bytes` / `dsl_fixture_keys_resolve_on_read` 三测锚定；需求 §5.4→测试锚点映射表见上「任务特有标准」）
- G4 红线：通过（`external/pi` 无改动、HEAD `2efa728d2ee90ef597626e96b1e28ef2b279f07c`；无 JS 执行能力；未读写 `~/.pi`/`.pi`；session 存储未动；token 估算未动；新增非测试代码无 `unwrap()`/`expect()`（逐文件扫描确认）；日志仅一处 bind 错误 warn 无凭据；凭据类型全部 redacted Debug；无范围排除项引入；锁仅用于 auth 文件）
- G5 线格式：通过（auth.json camelCase 判别式 `{type:"api_key"|"oauth"}` 与上游逐项核对 + 字节对拍；`CredentialInfo`/`AuthEvent` camelCase serde）
- G6 文档同步：通过（新模块全部含溯源注释（上游文件路径 + `@ pi 0.82.1 (2efa728)`）；D-008/D-009 回写 `02-design.md` §3.5 与 `01-requirements.md` §5.4；本任务文件设计细化/自测锚点/验收记录已填写）
- G7 偏离闭环：通过（D-008、D-009 均「已回写」，登记表已更新；均为实现细节级，无需 ADR）
- 结论：**通过**
