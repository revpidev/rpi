# ADR-0009：产品端点默认值迁移到 resetpi.com（Cloudflare Pages 自部署）

- **状态**：已采纳（端点域名部分已被 [ADR-0011](./0011-product-endpoints-revpi-dev.md) 取代为 revpi.dev）
- **日期**：2026-08-09
- **关联**：[`01-requirements.md`](../01-requirements.md) §8（产品 endpoint）、
  [D-046](../plan/v0.1/deviations/D-046-product-endpoints-telemetry-rust-notes.md)、
  ADR-0002（基线决策，含端点可配置）、部署内容 `deploy/resetpi/`

## 背景

rpi 的 5 个产品端点默认值全部指向上游 `pi.dev`（earendil-works/pi 运营）：

| 端点 | 默认（改前） | 用途 |
|---|---|---|
| 远程模型目录 | `https://pi.dev` → `/api/models/providers/{id}` | 38 内置 provider 的目录 overlay（D-052 起运行时消费） |
| 版本检查 | `https://pi.dev/api/latest-version` | 更新探测 |
| install telemetry | `https://pi.dev/api/report-install` | 匿名安装上报 |
| share viewer | `https://pi.dev/session/` | /share 输出链接 |
| changelog | `https://pi.dev/changelog` | 更新横幅链接 |

rpi 是独立分发的 fork（ADR-0001/0002 已把环境变量与命名从 PI_* 迁移到 RPI_*），
把产品回调用在自托管域名上与原上游解耦：内容可控、统计归己、不受上游端点
策略影响。端点自 T14（D-046）起已全部可配置（env > settings > 默认），本次
只改默认值，覆盖链不变。

## 决策

5 个端点默认值改为 `resetpi.com`（`https://resetpi.com` 为 base），内容用
**Cloudflare Pages 静态托管 + Pages Functions** 部署（`deploy/resetpi/`）：

- 模型目录：37 个静态 JSON（构建脚本从 `crates/rpi-ai/src/providers/data/*.json`
  生成，`{"models":[...]}` 平铺）；Pages 静态资产自带 ETag/Last-Modified，
  客户端 `If-None-Match` 4h revalidate 语义不变；未收录 provider 的 404 =
  "overlay 不可用"语义。
- 版本检查：静态 `api/latest-version.json`（发版时更新）。
- telemetry：Pages Function 204（可选 KV 记录）。
- share viewer：静态页读 URL fragment（客户端拼 `{base}#{gistId}`）渲染 gist。
- changelog：静态页。

**radius 默认 gateway 保持 `https://radius.pi.dev`**：radius 是动态 gateway
服务（OAuth + `/v1/config`），需要真实后端，非静态内容可覆盖；radius 用户
仍依赖上游托管服务，其余用户不受影响。

## 影响与回退

- 影响面：4 个常量 + changelog 链接 + `--help` 文本 + 相关测试断言；
  覆盖链（env/settings/`off`/`RPI_OFFLINE`）不变，旧配置的显式覆盖继续生效。
- 与上游对拍：默认值不再与上游一致（行为级偏离，随 D-046 端点可配置性
  已有覆盖机制，不另立偏离表）。
- 回退：恢复常量默认值或设置 `RPI_*_URL=https://pi.dev/...` 即可。
