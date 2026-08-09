# resetpi.com — pir 产品端点部署（Cloudflare Pages）

pir 的 5 个产品端点默认值自 ADR-0009 起指向 `resetpi.com`。本目录是
Cloudflare Pages 项目：静态目录内容 + Pages Functions，零后端。
`index.html` 为站点首页（开源项目主页，含特性/快速开始/端点说明），
`/` 请求直接命中。

## 端点 → 文件映射

| 端点 | 实现 | 说明 |
|---|---|---|
| `GET /api/models/providers/{id}` | `api/models/providers/*.json`（生成） | 模型目录 overlay，`{"models":[...]}`；静态资产自动带 ETag/Last-Modified，客户端 `If-None-Match` 4h 窗口 revalidate 走 304；未收录 provider 返回 404 = "overlay 不可用" |
| `GET /api/latest-version` | `api/latest-version.json`（生成） | 版本检查 `{"version","packageName","note"}`；发版时更新 |
| `GET /api/report-install` | `functions/api/report-install.js` | telemetry 收口，204；绑定 KV `ANALYTICS` 可记录安装统计 |
| `/session/#{gistId}` | `session/index.html` | share viewer；gist id 在 URL fragment 里（`get_share_viewer_url` 拼 `#`），页面 JS 读取并渲染 gist raw |
| `/changelog` | `changelog.html` | 更新横幅里的 changelog 链接 |

## 生成 + 部署

```bash
# 1. 生成 catalog 与 latest-version（在 pir 仓库根运行；版本默认取 workspace Cargo.toml）
python3 deploy/resetpi/scripts/generate-site.py
#    发版时：python3 deploy/resetpi/scripts/generate-site.py --version 0.2.0 --note "..."

# 2. 部署（首次需 npx wrangler login）
npx wrangler pages project create resetpi
npx wrangler pages deploy deploy/resetpi --project-name=resetpi

# 3. 自定义域（DNS 在 Cloudflare 托管时自动配置）
#    Pages 控制台 → Custom domains → 添加 resetpi.com

# 4.（可选）telemetry 统计
npx wrangler kv namespace create ANALYTICS   # 把 id 填入 wrangler.toml 后重新部署
```

连接 GitHub 仓库后可在 Pages 控制台开自动部署（push 即发布）；
也可以在 Actions 里挂 cron 定期重跑生成脚本，保持 catalog 与仓库同步。

## 客户端侧

默认端点已在代码里改为 resetpi.com（ADR-0009）。仍可逐项覆盖：

```bash
PIR_MODEL_CATALOG_URL=https://resetpi.com        # 或 settings.json modelCatalogUrl
PIR_VERSION_CHECK_URL=https://resetpi.com/api/latest-version
PIR_TELEMETRY_URL=https://resetpi.com/api/report-install
PIR_SHARE_VIEWER_URL=https://resetpi.com/session
# 字面量 off 关闭对应端点；PIR_OFFLINE 全局关闭
```

## 兼容性注意

- **Last-Modified 必须晚于本地 builtin `generated_at`**：`remote_models`
  在 `last_modified <= local_generated_at` 时忽略 overlay（用本地数据）。
  Pages 静态资产的 Last-Modified 是部署时间，天然满足；重新部署即刷新。
- **radius 不在 catalog 内**：radius 是动态 gateway provider（默认
  `https://radius.pi.dev`，上游托管服务，非静态内容），`/api/models/providers/radius`
  返回 404 即"overlay 不可用"语义，radius 用户不受影响。
- **数据源**：catalog 由 `crates/pir-ai/src/providers/data/*.json` 生成，
  与 pir 发版同步更新。
