# Rpi

用 Rust 1:1 复刻 [Pi](https://github.com/earendil-works/pi) agent harness 的工程仓库。此处「1:1」指行为层对拍一致，扩展为 API 形状同构（Rust/Wasm 重写），命名与包格式为 ADR 钉死的有意差异——三层边界定义见 [需求文档 §1.5](./docs/01-requirements.md)。

## 文档

| 文档 | 说明 |
|------|------|
| [可行性分析](./docs/00-feasibility.md) | 规模、分模块可行性、风险、工作量 |
| [需求规格](./docs/01-requirements.md) | 功能 1:1 需求与验收里程碑 |
| [架构设计](./docs/02-design.md) | Crate 划分、核心设计、路线图 |
| [UPSTREAM](./UPSTREAM.md) | 钉死的 Pi commit（0.82.1 / `2efa728`） |
| [ADR-0001](./docs/adr/0001-extension-and-config-dir.md) | 扩展=Rust/Wasm；配置=`~/.rpi` |
| [ADR-0002](./docs/adr/0002-baseline-decisions.md) | 版本钉死、TUI、token、单文件、JSONL、endpoint、MIT |
| [ADR-0009](./docs/adr/0009-product-endpoints.md) | 产品端点默认值迁移 resetpi.com（Cloudflare Pages 自部署，`deploy/revpi/`） |
| [ADR-0010](./docs/adr/0010-rename-pir-to-rpi.md) | 项目改名 rpi（crate/env/目录/ABI 全量） |
| [ADR-0011](./docs/adr/0011-product-endpoints-revpi-dev.md) | 产品端点域名 resetpi.com → revpi.dev（ADR-0009 域名部分取代） |
| [Extension ABI](./docs/extension-abi.md) | wasm（L1）/ 原生动态库（L0）扩展 ABI v1 |
| [Parity Checklist](./docs/parity-checklist.md) | 协议 / session 格式 / 扩展 API / TUI 四类对拍证据（T15 冻结） |

## 上游对照

见 [`UPSTREAM.md`](./UPSTREAM.md)。源码在 `external/pi/`。

## 许可证

MIT（与 Pi 相同）。

## 状态

v0.1 全部任务（T01–T16）完成：四层 crate（`rpi-ai` / `rpi-agent` / `rpi-tui` /
`rpi`）+ 扩展宿主（`rpi-ext-host`，L0 Rust 内置/动态库 + L1 Wasm）交付，
Parity Freeze 对拍清单见 [docs/parity-checklist.md](./docs/parity-checklist.md)，
进度索引见 [docs/plan/v0.1/index.md](./docs/plan/v0.1/index.md)。
