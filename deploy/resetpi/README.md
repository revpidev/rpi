# revpi.dev — rpi 产品端点部署（Cloudflare Pages）

rpi 的 5 个产品端点默认值自 ADR-0009 起指向 `revpi.dev`（域名变更见
ADR-0011）。本目录是 Cloudflare Pages 项目（项目名 `revpi`）：静态目录内容 +
Pages Functions，零后端。
`index.html` 为站点首页（开源项目主页，含特性/快速开始/端点说明），
`/` 请求直接命中。

## 端点 → 文件映射

| 端点 | 实现 | 说明 |
|---|---|---|
| `GET /api/models/providers/{id}` | `api/models/providers/*.json`（生成） | 模型目录 overlay，`{"models":[...]}`；静态资产自动带 ETag/Last-Modified，客户端 `If-None-Match` 4h 窗口 revalidate 走 304；未收录 provider 返回 404 = "overlay 不可用"（由 `functions/api/models/providers/[id].json.js` 代理实现，SPA fallback 会兜底 index.html，函数层以 content-type 区分） |
| `GET /api/latest-version` | `api/latest-version.json`（生成） | 版本检查 `{"version","packageName","note"}`；发版时更新；无后缀路径由 `_redirects` rewrite |
| `GET /api/report-install` | `functions/api/report-install.js` | telemetry 收口，204；绑定 KV `ANALYTICS` 可记录安装统计 |
| `/session/#{gistId}` | `session/index.html` | share viewer；gist id 在 URL fragment 里（`get_share_viewer_url` 拼 `#`），页面 JS 读取并渲染 gist raw |
| `/changelog` | `changelog.html` | 更新横幅里的 changelog 链接 |

## 生成 + 部署

```bash
# 1. 生成 catalog 与 latest-version（在 rpi 仓库根运行；版本默认取 workspace Cargo.toml）
python3 deploy/resetpi/scripts/generate-site.py
#    发版时：python3 deploy/resetpi/scripts/generate-site.py --version 0.2.0 --note "..."

# 2. 部署（首次需 npx wrangler login）
npx wrangler pages project create revpi --production-branch main
#    注意：wrangler 4.x 的 Pages Functions 目录取「当前工作目录/functions」，
#    必须在 deploy/resetpi 目录内执行部署（仓库根目录部署会丢失 functions）
cd deploy/resetpi && npx wrangler pages deploy . --project-name=revpi --branch main

# 3. 自定义域（DNS 在 Cloudflare 托管时自动配置）
#    Pages 控制台 → Custom domains → 添加 revpi.dev

# 4.（可选）telemetry 统计
npx wrangler kv namespace create ANALYTICS   # 把 id 填入 wrangler.toml 后重新部署
```

连接 GitHub 仓库后可在 Pages 控制台开自动部署（push 即发布）；
也可以在 Actions 里挂 cron 定期重跑生成脚本，保持 catalog 与仓库同步。

## 客户端侧

默认端点已在代码里改为 revpi.dev（ADR-0009/0011）。仍可逐项覆盖：

```bash
RPI_MODEL_CATALOG_URL=https://revpi.dev        # 或 settings.json modelCatalogUrl
RPI_VERSION_CHECK_URL=https://revpi.dev/api/latest-version
RPI_TELEMETRY_URL=https://revpi.dev/api/report-install
RPI_SHARE_VIEWER_URL=https://revpi.dev/session
# 字面量 off 关闭对应端点；RPI_OFFLINE 全局关闭
```

## 兼容性注意

- **Last-Modified 必须晚于本地 builtin `generated_at`**：`remote_models`
  在 `last_modified <= local_generated_at` 时忽略 overlay（用本地数据）。
  Pages 静态资产的 Last-Modified 是部署时间，天然满足；重新部署即刷新。
- **radius 不在 catalog 内**：radius 是动态 gateway provider（默认
  `https://radius.pi.dev`，上游托管服务，非静态内容），`/api/models/providers/radius`
  返回 404 即"overlay 不可用"语义，radius 用户不受影响。
- **旧域名 resetpi.com 过渡期兼容**：ADR-0011 前部署的旧项目 `resetpi` 在
  Pages 上保留在线，旧版本二进制仍可命中；确认无流量后可下线。
- **数据源**：catalog 由 `crates/rpi-ai/src/providers/data/*.json` 生成，
  与 rpi 发版同步更新。
