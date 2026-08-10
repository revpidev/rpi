# Rpi

用 Rust 1:1 复刻 [Pi](https://github.com/earendil-works/pi) agent harness 的工程仓库。此处「1:1」指行为层对拍一致，扩展为 API 形状同构（Rust/Wasm 重写），命名与包格式为 ADR 钉死的有意差异——三层边界定义见需求文档（维护于独立的私有文档仓库，未随本仓库公开）。

## 仓库结构

| 目录 | 内容 |
|------|------|
| `crates/` | Rust workspace：`rpi-ai`（模型/认证）/ `rpi-agent`（agent 内核）/ `rpi-tui`（终端 UI）/ `rpi`（CLI）/ `rpi-ext-host`（扩展宿主）等 |
| `fixtures/` | 契约与对拍测试 fixtures（生成脚本 + golden 数据） |
| `scripts/` | 开发辅助脚本（上游 pin 校验、catalog 刷新、数据生成） |
| `examples/` | wasm 扩展示例、`models.json` 参考模板 |
| `external/pi/` | 上游 Pi 对照（git submodule，钉死 commit 见 [UPSTREAM.md](./UPSTREAM.md)） |

工程文档（需求 / 设计 / ADR / 编码规范 / 计划 / 对拍清单）维护在独立的
私有文档仓库，不随本仓库发布；本仓库的行为金标准（上游钉死版本）见
[`UPSTREAM.md`](./UPSTREAM.md)，关键差异在代码内以注释标注。

## 上游对照

见 [`UPSTREAM.md`](./UPSTREAM.md)。源码在 `external/pi/`（submodule）。

## 许可证

MIT（与 Pi 相同）。

## 状态

v0.1 全部任务（T01–T16）完成：四层 crate（`rpi-ai` / `rpi-agent` / `rpi-tui` /
`rpi`）+ 扩展宿主（`rpi-ext-host`，L0 Rust 内置/动态库 + L1 Wasm）交付；
v0.11（对照基线提升至 `4181f66`）进行中。
