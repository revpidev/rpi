# D-046：产品 endpoint 配置化与 install telemetry Rust 落地差异（T14-W6a）

> 复制本模板为 `D-NNN-<short-slug>.md` 并填写。登记说明见 [README.md](./README.md)。

- **状态**：已关闭
- **关联任务**：T14（W6a）
- **级别**：实现细节偏离
- **发现日期**：2026-08-07

## 原文档约定

- 文档与章节：`docs/adr/0002-baseline-decisions.md` §8（可配置自有 Endpoint）、`docs/01-requirements.md` §10（版本检查 / telemetry 支持配置自有 endpoint、可关闭、`enableInstallTelemetry`(true) / `enableAnalytics`(false opt-in)）、`docs/coding-standards.md` §10.3
- 原文约定：版本检查、安装/更新 telemetry（及同类产品 HTTP 回调）须支持在 settings / 环境变量中配置自有 endpoint，不硬编码仅官方 URL；未配置时可用合理默认或关闭。

## 实际实现与偏离原因

上游（pi 0.82.1 @ 2efa728）把三个产品 endpoint 全部硬编码在 `https://pi.dev`
（version-check.ts:4、interactive-mode.ts:1028、remote-catalog-provider.ts:5），仅
`model-runtime.ts:69 catalogBaseUrl` 是内部选项，无 settings/env 覆盖。ADR-0002 §8 要求
rpi 增加配置面，以下为落地差异（均为新增配置面，不改变任何默认行为）：

1. **统一解析器 `config::resolve_endpoint` / `endpoint_from_env`**：三类 endpoint 共用
   「env > settings > 默认值」优先级；任一级为空串落空到下一级（沿用上游 `||` 语义先例，
   config.ts:506）；任一级取字面量 `off`（trim + ASCII 大小写不敏感）即整体关闭，关闭后
   **不产生任何网络请求**（解析返回 `None`，调用点不发起请求）。优先级口径沿用上游唯一
   先例 `PI_TELEMETRY` 覆盖 `enableInstallTelemetry`（telemetry.ts:10-12）。
2. **新增 env / settings 字段（rpi 专有，上游无对应物）**：`RPI_VERSION_CHECK_URL` /
   `RPI_TELEMETRY_URL` / `RPI_MODEL_CATALOG_URL` 与 settings 键 `versionCheckUrl` /
   `telemetryUrl` / `modelCatalogUrl`（camelCase，G5；settings 形状核对过上游
   settings-manager.ts:83-129，无同名字段冲突）。
3. **版本检查接线**：`update --self`/`--all` 流程（package-manager-cli.ts:475-501 对应物）
   在 settings manager 移交包管理器前解析 endpoint 并传入；endpoint 关闭时与上游
   「endpoint 不可达」同结局（报 `Could not determine latest rpi version.` 退出 1），且不发
   请求。交互模式启动版本检查（interactive-mode.ts:843-847 的 `checkForNewPiVersion` →
   `showNewVersionNotification`）本波次接线：tokio 任务探测、结果经
   `UiCommand::NewVersionAvailable` 入 drain 渲染（与 ThemeChanged/GitBranchChanged/
   ShareCompleted 同模式）；`RPI_SKIP_VERSION_CHECK` / `RPI_OFFLINE` / endpoint `off` 均在
   发起前短路。
4. **install telemetry 移植**（telemetry.ts + interactive-mode.ts:1017-1036）：新增
   `core/telemetry.rs`。`report_install` 为 fire-and-forget GET
   `{endpoint}?version={version}`，UA 头、5s 超时、忽略响应状态与全部错误（上游
   `.then(() => undefined).catch(() => undefined)`）；payload 仅版本号，无任何标识符/路径/
   凭据。上报时机按 `getChangelogForDisplay`（interactive-mode.ts:991-1014）：恢复会话
   （已有消息）不上报；全新安装或版本变化先写 `lastChangelogVersion` 再触发（设置写入
   与上游一致，不受开关影响）。**差异**：上游以「有新 changelog 条目」为更新触发条件，
   changelog 资产属 T15，本波次以「版本不等」近似——任一版本变化上游必然伴随新条目，
   反向（同版本新条目）不发生于发布流程；T15 落地 `getChangelogForDisplay` 时以真实条目
   判定替换。
5. **`enableAnalytics`（默认 false，opt-in）**：上游 0.82.1 无任何 analytics 发送通道
   （`getEnableAnalytics`/`getTrackingId` 仅被 settings 与首次设置向导消费），故 rpi 同样
   只持久化不发送——「关闭时零请求」按构造成立（开启时同样零请求，与上游一致）。
   startup_ui.rs 的遗留注释已同步更正。
6. **远程 catalog**：`model_catalog_endpoint()` 解析器就位（含 `off` 关闭）。运行时消费者
   仍缺——内置 provider 注册（model-runtime.ts:144-150 的 `withRemoteCatalog` 包装）自
   T13 起 deferred（D-038），注册波次落地时以该解析器为唯一入口；本波次不给
   `ModelRuntime` 加无消费者的字段。catalog 的零网络锚点沿用既有
   `offline_refresh_restores_stored_overlay_without_network`（`allowNetwork: false` 只恢复
   缓存不抓取）。
7. **帮助文本**：`--help` env 段新增 `RPI_SKIP_VERSION_CHECK`（上游存在但未写入帮助）与
   三条 endpoint 覆盖变量说明——上游帮助文本的可见增行。
8. **测试纪律**：`RPI_*` 进程环境在测试中只读不写（写仅 `core::environment` 既有测试一处，
   有其私有锁）；env 覆盖逻辑由纯函数 `resolve_endpoint` 承载测试，endpoint 包装函数只测
   默认值/设置值/关闭三条路径，避免并行测试的 env 竞争 flake。

## 影响面

无（纯内部）：不改协议 / session 格式 / 扩展 API / TUI 既有行为。默认配置下（无 env、无
settings 字段）三个 endpoint 与上游逐字节同 URL、同触发条件（第 4 条近似除外）、同
payload。TUI 行为差异仅两处新增：启动版本检查通知块（上游有、rpi 此前缺口的补齐）与
帮助文本增行。

## 处置

- **回写位置**：`docs/02-design.md` §8（endpoint 配置化段落）与 §12（version-check /
  telemetry 映射行）、`docs/plan/v0.1/T14-packages-trust-export.md` 偏离表
- **回写日期**：2026-08-07
- **ADR**：不需要（配置面由 ADR-0002 §8 直接授权）

## 补记（2026-08-09，ADR-0009）

默认值自 ADR-0009 起迁移到 `revpi.dev`（Cloudflare Pages 自部署，见
`deploy/resetpi/`）：`DEFAULT_CATALOG_BASE_URL`、`LATEST_VERSION_URL`、
`DEFAULT_REPORT_INSTALL_URL`、`DEFAULT_SHARE_VIEWER_URL`、changelog 链接五处。
第 1 条「不改变任何默认行为」随之失效——**默认配置下端点 URL 与上游不同**
（行为级偏离，由 ADR-0009 记录）；覆盖链（env > settings > `off` > `RPI_OFFLINE`）
与全部零网络语义不变，显式配置的旧值继续生效。radius 默认 gateway
（`https://radius.pi.dev`，上游托管服务）不迁移。

## 审查修复补记（2026-08-07 审查修复波次）

1. **交互模式测试零网络（M1）**：`init()` 的 install telemetry 与 `run()` 的启动版本
   检查原硬编码生产 transport，27 个 `init()` 测试 + `run_loop_end_to_end_vt_smoke`
   每次 `cargo test` 都向 pi.dev 发真实匿名请求（代理陷阱实测 2 条 `CONNECT`）。现
   `InteractiveUi` 新增 `report_install_transport` / `latest_version_transport` 注入点
   （`set_share_runner` 同模式），测试 harness 统一经
   `test_support::install_noop_product_transports` 换 no-op；新增
   `init_and_run_make_no_product_network_requests` 锚点（计数 0..=1，兼容
   `RPI_OFFLINE`/`RPI_SKIP_VERSION_CHECK` 环境门禁）。生产路径默认值不变。
