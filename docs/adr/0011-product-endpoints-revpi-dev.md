# ADR-0011：产品端点域名 resetpi.com → revpi.dev

- **状态**：已采纳
- **日期**：2026-08-10
- **关联**：[ADR-0009](./0009-product-endpoints.md)（端点自托管决策，
  本 ADR 仅替换其域名部分）、[ADR-0010](./0010-rename-pir-to-rpi.md)（项目改名 rpi）
- **取代**：ADR-0009 中 5 个端点默认值的域名部分；部署内容与覆盖链不变

## 背景

ADR-0009 决定 5 个产品端点默认值自托管在 `resetpi.com`（Cloudflare Pages，
部署内容 `deploy/revpi/`）。域名于 2026-08-10 更换为 **revpi.dev**（同一
Cloudflare 账号，zone `revpi.dev`，NS 指向 Cloudflare）。项目名 rpi 与域名
revpi.dev 对应关系更直接（`revpi` ↔ rpi + 项目性质），且 `.dev` 后缀与
开发者工具定位一致。

## 决策

5 个产品端点默认值域名整体从 `resetpi.com` 改为 `revpi.dev`，路径结构不变：

| 端点 | 默认（改后） |
|---|---|
| 远程模型目录 base | `https://revpi.dev` → `/api/models/providers/{id}` |
| 版本检查 | `https://revpi.dev/api/latest-version` |
| install telemetry | `https://revpi.dev/api/report-install` |
| share viewer | `https://revpi.dev/session/` |
| changelog | `https://revpi.dev/changelog` |

部署方式不变：Cloudflare Pages 项目 `revpi`（`https://revpi.pages.dev`），
静态托管 + Pages Functions，构建脚本与路由配置同 ADR-0009 方案
（`deploy/revpi/` 目录，wrangler 4.x 需在目录内执行部署以识别 functions）。

旧域名 `resetpi.com` 的 Pages 项目 `resetpi` 保留在线（过渡期兼容），
但不作为默认端点。

## 后果

- 客户端默认端点跟随代码发布切换；旧版本二进制仍请求 resetpi.com，过渡期
  两个项目并行在线，无断裂。
- `resetpi.com` zone 与 Pages 项目可待过渡期结束后下线（删除项目/zone 前
  确认无流量）。
- 文档与部署说明同步更新（README、UPSTREAM、需求/设计、D-046）。
