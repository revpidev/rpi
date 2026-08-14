# MCP adapter cross-implementation parity harness (design §5.2)

同一 fixture MCP 服务器驱动两侧客户端——上游钉死版 Node `McpServerManager`
（`rpi/external/pi-mcp-adapter` @ `3d953f90`，只读）与本 crate 的 Rust
manager——并 diff 归一化后的帧序列与结果 JSON。任何差异都归因于客户端实现
本身（两侧 fixture 服务器逐字节同构）。

## 运行

```bash
bash scripts/mcp-parity/run-parity-suite.sh         # 一键：依赖安装 + 四腿 + 归档
bash scripts/mcp-parity/run-parity-suite.sh mcp-parity   # 单腿（可多选）
node scripts/mcp-parity/run-mcp-parity.mjs     # 全场景，非零退出码 = 有差异
```

- 依赖装在 `/tmp/rpi-mcp-parity-deps`（`setup-deps.sh` 复制上游
  `package.json` + `package-lock.json` 后 `npm ci`——完整传递闭包钉死
  为上游测试过的版本；含 tsx 与官方 conformance referee）。
  **绝不写入 `rpi/external/`**。
- 报告落 `rpi/fixtures/generated/mcp-parity/`（`parity-report.md` +
  每场景两侧 `parity-<scenario>-{upstream,rpi}.json`），**进 git 作为
  证据链**（归一化剔除运行期易变值，复跑不产生 churn）。

## 场景

| 场景 | 传输 | fixture 行为 |
| --- | --- | --- |
| `stdio` | Node fixture server 子进程 stdin/stdout | 全功能（initialize/tools/resources/prompts/call） |
| `http-streamable` | HTTP POST JSON | `Mcp-Session-Id` 会话 |
| `http-fallback-404/405/406/415` | POST 先回失败码 → legacy SSE 回退 | GET 事件流 + POST /message |
| `http-auth-401` | 401 + `WWW-Authenticate` | needs-auth 路径 |

## 归一化与豁免（diff 稳定性）

- JSON-RPC `id` → `$id`（两侧各自从 0 自增，值相同但语义上视为不可变序列）。
- `clientInfo.name` → `parity-client`：上游 `pi-mcp-<server>` vs rpi
  `rpi-mcp-<server>`（设计 §6 O1 品牌文案豁免）。
- discovery 帧排序豁免：上游 `Promise.all` 并发发出 tools/resources/prompts
  list（server-manager.ts:458-462），到达序随调度抖动（实测逐次不同）；rpi
  顺序发出。JSON-RPC list 请求相互独立，线上等价物是帧**集合**，故比较时对
  连续 discovery 帧按 method 排序，落盘转录保持原序。
- `http-auth-401` 预期差异：上游 401 处理在 connect 内进入 OAuth 发现流
  （本 stub 的 resource_metadata 指向不可达端口 → error）；P0 rpi 侧止于
  needs-auth 连接状态（FR-P0-08 范围）。OAuth 续流的对拍归 TE03。

## 组件

- `fixture-server.mjs`：共享 fixture 服务器（stdio / http × 4 profile），帧
  转录到 `RPI_MCP_FIXTURE_LOG`（`RPI_MCP_FIXTURE_LOG_FRAMES=1` 记完整帧）。
- `upstream-runner.mjs`：上游侧驱动（tsx 直跑钉死 TS 源码）。
- `parity_runner.rs`（crate example）：rpi 侧驱动，同一步骤序列。
- `parity-hooks.mjs`：裸依赖解析到外置目录 + host 包 stub（上游仅类型
  import；`@earendil-works/pi-ai/compat` 的值 import `complete` 提供抛错
  stub——parity 场景不注册 sampling）。
- `setup-deps.sh`：外置依赖安装（复制上游 lockfile 后 `npm ci`，完整
  传递闭包与上游一致）。
- `run-parity-suite.sh`：一键复跑入口（依赖安装 → 四腿 → 归档）；含
  `conformance-baseline.yml`（expected-failures）与
  `normalize-conformance.mjs`（conformance 归档归一化：时间戳/瞬态
  端口/会话 id/retry 抖动 → 标记）。

## OAuth 对拍（TE02 自测项 5 / TE03 前置）

`run-oauth-parity.mjs` + `oauth-stub-server.mjs` +
`oauth-upstream-driver.mjs` + crate example `oauth_parity_runner.rs`：
stub authorization server（RFC 8414 metadata + DCR + /authorize 302 +
/token）记录请求转录，两侧各自完整走授权码 + PKCE 流（上游
`mcp-auth-flow.ts startAuth/completeAuthFromInput`，rpi `oauth.rs
authenticate`），diff 归一化后的 DCR / authorization URL / token 表单参数。

归一化：`code_challenge`/`state`/`code_verifier`/`code` → 标记；回调端口 →
`$port`；stub AS 端口 → `$asport`；`client_name`/`client_uri` → `$client_name`
/`$client_uri`（O1 品牌豁免）；键序无关比较（表单/查询参数无序）。

```bash
node scripts/mcp-parity/run-oauth-parity.mjs   # → oauth-parity.md
```

## 端到端五模式对拍（设计 §5.3）

`run-e2e-parity.mjs`：同一 fixture 配置分别驱动 `pi -p`（上游 CLI +
npm 版 adapter）与 `rpi -p`（本仓库 CLI + native cdylib），各跑 list /
search / describe / call / status 五模式，diff 归一化后的工具结果文本
（取模型回复的 verbatim 围栏块；模型与外围文案差异被剥离）。

环境要求与隔离：上游侧 `PI_CODING_AGENT_DIR` 指向临时 agent dir（fresh
mcp-cache → 两侧同样走 bootstrap-all），npm 包目录**复制**自真实 HOME
（与登录态同策略：只读证据进沙箱，软链会让沙箱内包管理写穿真实
HOME），auth 复制；rpi 侧 `RPI_CODING_AGENT_DIR` 指向装了
`librpi_ext_mcp_adapter.so` + manifest 的临时 agent dir，auth 复制。两侧
都需要可用的模型 provider 登录态。

```bash
node scripts/mcp-parity/run-e2e-parity.mjs   # → e2e-parity.md
```

## conformance Rust driver（O2 收口）

crate example `conformance_driver.rs` 提供与上游 `conformance/driver.sh`
相同的 CLI 契约（`MCP_CONFORMANCE_SCENARIO=<scenario> driver <server-url>`），
由官方 referee（`@modelcontextprotocol/conformance@0.1.16`，随 setup-deps.sh
的上游 lockfile 闭包外置安装于 `/tmp/rpi-mcp-parity-deps`）经 stdio 驱动，
被测客户端即本 crate 的 `McpServerManager`。复跑入口：
`bash scripts/mcp-parity/run-parity-suite.sh conformance`（referee 调用 +
expected-failures baseline `conformance-baseline.yml` + 稳定目录归档 +
归一化）。核心四场景结果留档
`fixtures/generated/mcp-parity/conformance/`（`driver-summary.txt` +
每场景固定目录 `<scenario>/checks.json`，归一化后进 git）：

- `initialize`、`tools_call`、`sse-retry` PASS（sse-retry 含 retry: 时序、
  Last-Event-ID、graceful reconnect 三检查全过）
- `elicitation-sep1034-client-defaults` FAIL（预期：server→client
  elicitation 是 P2 范围，P0 应答 -32601）
- `auth/*` 场景属 TE03（OAuth 矩阵），届时沿用 `baseline-client.yml`

## 本 harness 抓到并已修复的差异

1. `tools/list` capability 守卫缺失：SDK `Client.listTools` 在服务器未声明
   `tools` capability 时不发请求直接返回空表（控制台警告）；rpi 薄客户端
   原先无条件发请求。已对齐（protocol.rs `fetch_all_tools`）。
2. probe 分类三处偏差（`mcp-probe.ts`）：401 envelope 缺 Bearer-challenge
   检查、modern 阶段缺 `unsupported-modern` 回退、`responseKind` 对空
   content-type 输出空串而非 "an untyped response"。已对齐（manager.rs）。
3. SSE 重连调度缺失（SDK `_scheduleReconnection`/`_handleSseStream`）：
   `retry:` 字段驱动的重连延迟、`Last-Event-ID` 回放头、per-request 流
   无响应时的 GET 重开 + 响应 id 重映射。已补齐（protocol/http.rs），
   conformance `sse-retry` 场景由 FAIL 转 PASS。
4. OAuth 授权码流四处断点：callback listener 端口先绑定再构造
   redirect_uri/DCR（原先写死 `localhost:0`）、DCR redirect_uris 同步、
   DCR secret 回传 token 交换、PKCE verifier 读回 fallback。已修复
   （oauth.rs），stub AS 对拍 MATCH。

