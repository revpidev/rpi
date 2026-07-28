# Pir

用 Rust 1:1 复刻 [Pi](https://github.com/earendil-works/pi) agent harness 的工程仓库。此处「1:1」指行为层对拍一致，扩展为 API 形状同构（Rust/Wasm 重写），命名与包格式为 ADR 钉死的有意差异——三层边界定义见 [需求文档 §1.5](./docs/01-requirements.md)。

## 文档

| 文档 | 说明 |
|------|------|
| [可行性分析](./docs/00-feasibility.md) | 规模、分模块可行性、风险、工作量 |
| [需求规格](./docs/01-requirements.md) | 功能 1:1 需求与验收里程碑 |
| [架构设计](./docs/02-design.md) | Crate 划分、核心设计、路线图 |
| [UPSTREAM](./UPSTREAM.md) | 钉死的 Pi commit（0.82.1 / `2efa728`） |
| [ADR-0001](./docs/adr/0001-extension-and-config-dir.md) | 扩展=Rust/Wasm；配置=`~/.pir` |
| [ADR-0002](./docs/adr/0002-baseline-decisions.md) | 版本钉死、TUI、token、单文件、JSONL、endpoint、MIT |

## 上游对照

见 [`UPSTREAM.md`](./UPSTREAM.md)。源码在 `external/pi/`。

## 许可证

MIT（与 Pi 相同）。

## 状态

调研与基线决策已完成，可进入 M0 工程骨架。
