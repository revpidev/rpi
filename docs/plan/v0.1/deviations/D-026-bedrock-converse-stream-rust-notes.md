# D-026：bedrock-converse-stream 适配器 `@aws-sdk`/`@smithy` 反推与 reqwest 直连差异

> 复制本模板为 `D-NNN-<short-slug>.md` 并填写。登记说明见 [README.md](./README.md)。

- **状态**：已回写
- **关联任务**：T13（W2）
- **级别**：实现细节偏离
- **发现日期**：2026-08-06

## 原文档约定

- 文档与章节：`docs/02-design.md` §3.3、§14；`docs/01-requirements.md` §5.2
- 原文约定：Bedrock 接入为「手写 SigV4 + reqwest + 自实现 event-stream 解码（不引 aws-sdk）」；
  行为锚点为 SigV4 vs bearer 双鉴权、region 解析顺序、header 白名单、cachePoint 1h、
  interleaved thinking、`EMPTY_TEXT_PLACEHOLDER`、自适应 thinking 模型族清单。

## 实际实现与偏离原因

上游 `bedrock-converse-stream.ts` 委托 `@aws-sdk/client-bedrock-runtime` + `@smithy`，
传输层在 TS 源码中不可考（来源空白，设计文档 §3.3）。本次按钉死版 node_modules 内的
SDK 源码反推并落地于 `crates/pir-ai/src/api/bedrock_converse_stream.rs`（+ `api/bedrock/`
子模块 `sigv4.rs` / `event_stream.rs`）。反推定案：

- 请求线格式（`client-bedrock-runtime` schemas_0.js + `@smithy/core` HttpBindingProtocol +
  `@aws-sdk/core` AwsRestJsonProtocol 核对）：`POST {endpoint}/model/{extendedEncodeURIComponent(modelId)}/converse-stream`，
  `content-type: application/json`，body 为 rest-json1（camelCase、schema 成员序）；
  `modelId` 是 http label 进 path 不进 body。
- SigV4（`@smithy/signature-v4` 逐函数移植）：`x-amz-content-sha256` 默认设置且参与签名
  （applyChecksum 默认 true）、`ALWAYS_UNSIGNABLE_HEADERS` + `proxy-*`/`sec-*` 过滤、
  canonical path 段归一化后整体二次编码（`%2F` 还原）、signing service 为 `bedrock`。
  确定性由 AWS 官方文档 IAM 签名向量 + 算法复算向量钉死。
- event-stream（`@smithy/core` eventstream-codec）：12 字节 prelude + 头 + 载荷 +
  双 CRC32（IEEE）帧格式，跨 chunk 重组（getChunkedStream 语义），错误文案对齐。
- 错误名解析顺序（`loadRestJsonErrorCode`）：`x-amzn-errortype` 头 > body `code` >
  body `__type`，`sanitizeErrorCode`（`,`/`:`/`#` 截断）逐行移植；流内 exception 帧的
  camelCase 成员名首字母大写后映射 `BEDROCK_ERROR_PREFIXES`。

与上游的可观测差异：

1. **HTTP 层 reqwest 直连**：SDK 的 `amz-sdk-invocation-id` / `amz-sdk-request` /
   SDK `user-agent` 头不发送。
2. **重试**：上游由 SDK standard 模式默认重试（2 次额外尝试，且不读 `maxRetries`）；
   pir 走共享 `retry_provider_request`，`StreamOptions::max_retries` 未设置时同样
   默认 2 次额外尝试（W7 审查补漏前默认 0）；显式设置 `max_retries` 时 pir 生效
   而上游忽略——此为残留差异。
2a. **错误文本**：非 2xx 响应的原始 body 经 `format_bedrock_error` 以
   `{status}: {body}` 形式 surfaced（对齐上游 `formatBedrockError` 经
   `normalizeProviderError` 取 `$response.body` 的行为；W7 审查补漏前 body 被
   丢弃）；`mapStopReason` 空串边缘对齐 JS 假值语义（`Error` 无消息）。
3. **凭据链仅 env**：只支持 `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` /
   `AWS_SESSION_TOKEN`（外加 `AWS_BEDROCK_SKIP_AUTH=1` 假凭据与 bearer token）。
   profile 文件 / SSO / IMDS / `~/.aws/config` 不解析；`BedrockOptions::profile`
   仅为对齐上游形状而保留、不参与凭据解析。无可用 SigV4 凭据时报
   `Could not load credentials from any providers`。
4. **region 兜底**：上游在「ambient AWS_PROFILE 且无显式 region」时把 region 交给 SDK
   profile 链；pir 不读 profile 配置，回退 `us-east-1`（与上游非 Node 兜底分支一致）。
   endpoint 推导由 SDK endpoint ruleset 收敛为
   `bedrock-runtime.{region}.amazonaws.com`（`cn-*` 为 `.amazonaws.com.cn`），
   FIPS/dualstack 变体不推导。
5. **proxy / HTTP1 开关**：`resolveHttpProxyUrlForTarget` 代理 agent 与
   `AWS_BEDROCK_FORCE_HTTP1` 是 Node SDK request-handler 旋钮，未移植。
6. **on_payload 形状**：与其他 pir 适配器一致，见 camelCase wire JSON；但保留 `modelId`
   字段（对应 SDK command input），其被消费为 path label 后从 body 剥离——经
   `on_payload` 替换 `modelId` 的能力与上游一致。
7. **图像块 base64 直通**：上游 `atob` 解码后 SDK 再编码（线上恒等）；pir 直接透传，
   非法 base64 变为服务端报错而非本地 `atob` 抛错。
8. **`PI_CACHE_RETENTION` → `PIR_CACHE_RETENTION`**（沿用 D-021 钉死的 env 前缀约定），
   `resolve_cache_retention` 复用 anthropic 适配器同款 helper。
9. **`bedrock-converse-stream.lazy.ts` 无对应物**（pir 适配器静态链接）。
10. 新增依赖 `sha2` / `hmac`（手写 SigV4 的 HMAC-SHA256/SHA256，见
    `coding-standards.md` 附录 A 新增行）；event-stream 的 CRC32 为模块内自实现，未引 crate。

## 影响面

无（纯内部）：线格式与签名算法按 SDK 源码反推对齐，事件序 / stopReason / usage 语义与
上游适配器一致；上述差异均为传输层可观测细节，不影响对拍契约。凭据链缺口（第 3、4 条）
是环境能力缺口而非行为契约变更，已在模块头注释与契约测试中显式标注。

## 处置

- **回写位置**：`docs/02-design.md` §3.3（Bedrock 反推落地说明）；`coding-standards.md`
  附录 A（新增 `sha2` / `hmac` 依赖行）
- **回写日期**：2026-08-06
- **ADR**：不需要
