# Rpi

**终端里的 AI 编程搭档——用 Rust 编写，源自 [Pi](https://github.com/earendil-works/pi)。**

[English](./README.md) · [中文](./README.zh-CN.md)

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](./LICENSE)
[![Language: Rust](https://img.shields.io/badge/Language-Rust-orange?logo=rust&logoColor=white)](./Cargo.toml)
[![Platform: Linux · macOS · Windows](https://img.shields.io/badge/Platform-Linux%C2%B7macOS%C2%B7Windows-lightgrey)]()
[![GitHub stars](https://img.shields.io/github/stars/revpidev/rpi)](https://github.com/revpidev/rpi)
[![GitHub issues](https://img.shields.io/github/issues/revpidev/rpi)](https://github.com/revpidev/rpi)

Rpi 是一个用 Rust 编写的终端 AI 编程助手，源自 [Pi coding agent](https://github.com/earendil-works/pi)：继承了 Pi 的架构，早期行为与上游保持对拍一致；但 rpi 是独立项目，后续发展可能与 Pi 产生偏差。编译产出为单个静态二进制，不依赖 Node、Python 或任何运行时。

## 特性

- ⚡ **Rust 单二进制** —— 静态链接、无运行时依赖；拷到机器上就能跑，启动毫秒级。
- 🖥️ **完整的终端体验** —— 多轮对话、流式输出、代码语法高亮、斜杠命令、Unicode 感知渲染。
- 🛠️ **真正能干活** —— 内置读文件、改代码、执行命令等工具；长对话自动压缩上下文不中断；会话可导出 HTML，或一键生成链接分享。
- ☁️ **38 家模型服务商内置** —— OpenAI、Anthropic、Claude Code、Google、Mistral、DeepSeek、Groq、OpenRouter、Bedrock、Vertex、Codex、Qwen、Kimi 等，模型目录由 `revpi.dev` 在线提供。
- 🔌 **插件可扩展** —— Wasm（L1）/ 原生动态库（L0）两种插件形态；支持 skills、提示模板、主题。
- 🔒 **隐私由你掌控** —— 一切本地运行；每个网络端点都可关闭（见[配置](#配置)）。

## 快速开始

> **注意**：目前尚无预编译二进制与已发布的 crate——请从源码构建（仅需稳定版 Rust 工具链）。

```bash
git clone --recurse-submodules https://github.com/revpidev/rpi.git
cd rpi
cargo build --release
./target/release/rpi --provider anthropic --model claude-sonnet-4-20250514
```

`external/pi` submodule 是对拍用的钉死上游基线，仅开发期使用——**构建和运行 rpi 不需要它**。

在 `~/.rpi/settings.json` 或对应标准环境变量（如 `ANTHROPIC_API_KEY`、`OPENAI_API_KEY`）中配置你的 API key；Anthropic、OpenAI Codex、radius 还支持交互式 OAuth 登录。

## 使用

不带参数运行 `rpi` 进入交互模式，输入 `/help` 查看内置斜杠命令。

常用选项：

| 选项 | 说明 |
|---|---|
| `--provider <id>` / `--model <id>` | 选择服务商与模型（`rpi --list-models` 浏览目录） |
| `--continue` / `--resume` | 继续上一个会话 / 恢复指定会话 |
| `--session <id>` / `--fork <id>` | 打开 / 派生指定会话 |
| `--print` | 非交互单轮，打印回复后退出 |
| `--export <format>` | 导出会话（如 HTML） |
| `--offline` | 本次运行关闭所有网络端点 |
| `rpi <message>` | 非交互一次性提问 |

包管理命令：

```bash
rpi update --self          # 自更新（二进制安装会打印下载指引）
rpi update --models        # 刷新远程模型目录
rpi update --extensions    # 更新扩展（或 --all）
rpi install <source>       # 安装扩展
rpi remove <source>        # 卸载扩展
rpi list                   # 列出已装扩展
rpi config                 # 查看已批准的项目信任决策
```

## 配置

配置位于 `~/.rpi/`（或 `RPI_CODING_AGENT_DIR`）。设置以环境变量驱动，各产品端点均可覆盖或关闭：

| 环境变量 | 默认值 | 用途 |
|---|---|---|
| `RPI_OFFLINE` | — | 任意值即关闭全部网络端点 |
| `RPI_SKIP_VERSION_CHECK` | — | 跳过启动时的更新检查 |
| `RPI_VERSION_CHECK_URL` | `https://revpi.dev/api/latest-version` | 更新探测端点；字面量 `off` 关闭 |
| `RPI_MODEL_CATALOG_URL` | `https://revpi.dev` | 远程模型目录基址；字面量 `off` 关闭 |
| `RPI_TELEMETRY_URL` | `https://revpi.dev/api/report-install` | 安装计数遥测端点；字面量 `off` 关闭 |
| `RPI_SHARE_VIEWER_URL` | `https://revpi.dev/session` | 会话分享查看页；字面量 `off` 关闭 |
| `RPI_CODING_AGENT_DIR` | `~/.rpi` | 配置与状态目录 |
| `RPI_CODING_AGENT_SESSION_DIR` | `~/.rpi/agent/sessions` | 会话存储目录 |

同样的 URL 也可在 `settings.json` 中以 `versionCheckUrl`、`modelCatalogUrl`、`telemetryUrl` 设置；环境变量优先。

## 产品端点（`revpi.dev`）

| 端点 | 用途 |
|---|---|
| `GET /api/models/providers/{id}` | 各服务商的模型目录 overlay（`{"models":[...]}`）；404 表示"无 overlay，用内置数据" |
| `GET /api/latest-version` | 版本检查 `{"version","packageName","note"}`——驱动更新横幅 |
| `POST /api/report-install` | 可选的安装计数遥测（204） |
| `/session/#{gistId}` | 导出会话的分享查看页 |

站点由 [rpi-pages](https://github.com/revpidev/rpi-pages) 仓库部署在 Cloudflare Pages；文档见 <https://revpi.dev/docs>。

## 仓库结构

| 路径 | 内容 |
|---|---|
| `crates/rpi` | CLI、配置、交互模式、内置工具、包管理 |
| `crates/rpi-agent` | Agent 内核（主循环、工具调用、上下文压缩、harness） |
| `crates/rpi-ai` | 模型服务商、认证（API key / OAuth）、远程模型目录 |
| `crates/rpi-tui` | 终端 UI 引擎（渲染、组件、markdown、图片） |
| `crates/rpi-ext-host` | 扩展宿主：L1 Wasm + L0 原生动态库 |
| `crates/rpi-ext-sdk` | 编写扩展用的 SDK crate |
| `fixtures/` | 契约/对拍测试 fixtures（生成脚本 + golden 数据） |
| `scripts/` | 开发脚本（上游 pin 校验、catalog 刷新、数据生成） |
| `external/pi` | 上游 Pi 对照（git submodule，对拍用钉死基线，见 `UPSTREAM.md`） |

## 与 Pi 的关系

Rpi 起步于对 Pi 的 Rust 移植，早期开发期将行为钉死于特定上游 commit 进行对拍验证（钉死基线见 [`UPSTREAM.md`](./UPSTREAM.md)）。项目独立演进：对拍一致是起点而非承诺，两个项目各自发展后行为可能出现偏差。rpi 与 Pi 均为 MIT 许可证。

## 开发

```bash
cargo build --workspace
cargo test --workspace     # 全部单元 + 契约测试；不访问真实网络
cargo test -p rpi-ai --test model_catalog --test compat_matrix
scripts/verify-upstream.sh # 确认 external/pi 停留在钉死 commit
```

## 状态

v0.1（T01–T16）已完成：四层 crate（`rpi-ai` / `rpi-agent` / `rpi-tui` / `rpi`）+ 扩展宿主（L0 原生 + L1 Wasm）交付。v0.11 进行中（对照基线提升至 `4181f66` / v0.84.1+）。

## 许可证

[MIT](./LICENSE)——与 Pi 相同。
