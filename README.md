# Rpi

用 Rust 1:1 复刻 [Pi](https://github.com/earendil-works/pi) agent harness 的工程仓库。此处「1:1」指行为层对拍一致，扩展为 API 形状同构（Rust/Wasm 重写），命名与包格式为 ADR 钉死的有意差异——三层边界定义见 [需求文档 §1.5](../rpi-docs/01-requirements.md)。

## 仓库结构

项目拆分为三个独立 git 仓库（本地平级目录）：

| 仓库 | 目录 | 内容 |
|------|------|------|
| **rpi** | 本仓库 | Rust 源码（workspace、crates、fixtures、`external/pi` 上游对照、`UPSTREAM.md` 钉死记录） |
| **rpi-docs** | `../rpi-docs/` | 需求 / 设计 / ADR / 编码规范 / 计划 / 对拍清单 |
| **rpi-pages** | `../rpi-pages/` | 官网 revpi.dev（Cloudflare Pages 站点 + 产品端点） |

> 跨仓库相对链接（`../rpi-docs/...`）在三个仓库平级排列时可直接浏览；
> 远程地址配置后可将这些链接改为绝对 URL。

## 文档

文档已迁移至 [rpi-docs 仓库](../rpi-docs/README.md)，常用入口：

| 文档 | 说明 |
|------|------|
| [可行性分析](../rpi-docs/00-feasibility.md) | 规模、分模块可行性、风险、工作量 |
| [需求规格](../rpi-docs/01-requirements.md) | 功能 1:1 需求与验收里程碑 |
| [架构设计](../rpi-docs/02-design.md) | Crate 划分、核心设计、路线图 |
| [编码规范](../rpi-docs/coding-standards.md) | Rust workspace 工程规范 |
| [UPSTREAM](./UPSTREAM.md) | 钉死的 Pi commit（`4181f66` / v0.84.1+） |
| [ADR-0001](../rpi-docs/adr/0001-extension-and-config-dir.md) | 扩展=Rust/Wasm；配置=`~/.rpi` |
| [ADR-0002](../rpi-docs/adr/0002-baseline-decisions.md) | 版本钉死、TUI、token、单文件、JSONL、endpoint、MIT |
| [ADR-0009](../rpi-docs/adr/0009-product-endpoints.md) | 产品端点默认值 revpi.dev（Cloudflare Pages 自部署，rpi-pages 仓库） |
| [ADR-0010](../rpi-docs/adr/0010-rename-pir-to-rpi.md) | 项目改名 rpi（crate/env/目录/ABI 全量） |
| [Extension ABI](../rpi-docs/extension-abi.md) | wasm（L1）/ 原生动态库（L0）扩展 ABI v1 |
| [Parity Checklist](../rpi-docs/parity-checklist.md) | 协议 / session 格式 / 扩展 API / TUI 四类对拍证据（T15 冻结） |

## 上游对照

见 [`UPSTREAM.md`](./UPSTREAM.md)。源码在 `external/pi/`。

## 许可证

MIT（与 Pi 相同）。

## 状态

v0.1 全部任务（T01–T16）完成：四层 crate（`rpi-ai` / `rpi-agent` / `rpi-tui` /
`rpi`）+ 扩展宿主（`rpi-ext-host`，L0 Rust 内置/动态库 + L1 Wasm）交付；
v0.11（对照基线提升至 `4181f66`）进行中。
Parity Freeze 对拍清单与进度索引见
[rpi-docs](../rpi-docs/README.md)（[parity-checklist.md](../rpi-docs/parity-checklist.md)、
[plan/v0.1/index.md](../rpi-docs/plan/v0.1/index.md)、
[plan/v0.11/index.md](../rpi-docs/plan/v0.11/index.md)）。
