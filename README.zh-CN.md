# Rpi

**终端里的 AI 编程搭档——用 Rust 编写，源自 [Pi](https://github.com/earendil-works/pi)。**

[English](./README.md) · [中文](./README.zh-CN.md)

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](./LICENSE)
[![Language: Rust](https://img.shields.io/badge/Language-Rust-orange?logo=rust&logoColor=white)](./Cargo.toml)
[![Platform: Linux · macOS · Windows](https://img.shields.io/badge/Platform-Linux%C2%B7macOS%C2%B7Windows-lightgrey)]()
[![Build](https://img.shields.io/github/actions/workflow/status/revpidev/rpi/build.yml)](https://github.com/revpidev/rpi/actions/workflows/build.yml)

Rpi 是一个用 Rust 编写的终端 AI 编程助手，源自 [Pi coding agent](https://github.com/earendil-works/pi)：继承了 Pi 的架构，行为持续与钉死的上游版本对拍验证——当前行为基线为 **Pi v1.4.0**（`4a080252f`，见 [`UPSTREAM.md`](./UPSTREAM.md)）。rpi 是独立项目，随着两者演进可能出现偏差。编译产出为单个静态二进制，不依赖 Node、Python 或任何运行时。

当前发布线为 **v0.1.7**，即最新稳定版（2026-10-19）。使用下方脚本安装，已有安装用 `rpi update` 更新（见[状态](#状态)）。

## 特性

- ⚡ **Rust 单二进制** —— 静态链接、无运行时依赖；拷到机器上就能跑，启动毫秒级。
- 🖥️ **完整的终端体验** —— 多轮对话、流式输出、语法高亮、斜杠命令、Unicode 感知渲染、内联图片、LaTeX 与 Mermaid，以及 regular/fullscreen 两种 TUI 模式。
- 🛠️ **真正能干活** —— 内置 `read` / `bash` / `edit` / `write` 工具（另有可选的 `grep` / `find` / `ls`），长对话自动压缩上下文，会话可导出 HTML 或生成分享链接。
- 🧰 **内建 agent 能力** —— `codemode`（在沙箱中运行 JavaScript，组合工具与非 LLM 模型）、`tool_search`（BM25 工具发现）、内建 MCP 客户端（stdio 与 streamable HTTP，支持 OAuth）。
- ☁️ **42 家模型服务商内置** —— OpenAI、Anthropic、Claude Code、OpenAI Codex、Google、Vertex、Bedrock、Azure、Mistral、DeepSeek、Groq、OpenRouter、Qwen Token Plan、Kimi、Meta、MiniMax、xAI、Xiaomi MiMo、Cloudflare 等，模型目录由 `revpi.dev` 在线提供。
- 🔌 **插件可扩展** —— L1 Wasm / L0 原生动态库两种插件形态；七款第一方插件（`rpi-subagents`、`rpi-smart-fetch`、`rpi-statusline`、`rpi-ask-user-question`、`rpi-todo`、`rpi-plan-mode`、`rpi-usage`），并支持 skills、提示模板与主题。
- 🔒 **隐私由你掌控** —— 一切本地运行；每个网络端点都可关闭（见[配置](#配置)）。

## 快速开始

### 安装（预编译二进制）

macOS / Linux（POSIX sh；自动区分 glibc / musl）：

```bash
curl -fsSL https://revpi.dev/install.sh | sh
```

安装脚本自动检测操作系统与架构，下载匹配的 Release 资产并做 SHA-256 校验（针对下载损坏的完整性校验），默认安装到 `~/.local/bin`（可用 `--prefix <dir>` 覆盖）。脚本与资产均可从官网（revpi.dev）下载——GitHub 不可直连时安装脚本会自动回退到官网镜像，也可以随时从 GitHub Releases 手动下载资产。若 `~/.local/bin` 不在 `PATH` 中，请添加：

```bash
export PATH="$HOME/.local/bin:$PATH"   # 追加到 ~/.profile、~/.bashrc 或 ~/.zshrc
```

Windows（PowerShell；默认安装到 `%LOCALAPPDATA%\Programs\rpi`，可用 `-Prefix` 覆盖）：

```powershell
irm https://revpi.dev/install.ps1 | iex
```

两个安装器都会在安装目录尚未成为当前生效的 `rpi` 时，询问是否把该目录加入 PATH。macOS/Linux 的交互式安装（stdout 是终端、`/dev/tty` 可用、未设置 `CI`/`RPI_NONINTERACTIVE`、`TERM` 不是 `dumb`）会询问一次（默认是），并向检测到的 profile 写入带 `# rpi` 标记、带幂等 `case` 守卫的块：fish（`fish_add_path`）、zsh、bash（优先 `.bashrc`，其次 `.bash_profile`/`.bash_login`，最后 `.profile`）或 POSIX 的 `~/.profile`。不支持的 shell 只打印手动说明；root/系统级安装以及安装目录不在 `$HOME` 下时打印准确的手动命令。设置 `CI`/`RPI_NONINTERACTIVE`、stdout 被重定向或没有控制终端时会跳过提示。若存在 pty 但没有上述标记，支持 `read -t` 的 shell 会在 `RPI_INSTALL_PROMPT_TIMEOUT` 秒（默认 60，视为“否”）后超时；用 `RPI_INSTALL_ADD_PATH=0`（或 `--no-add-path`）保证不等待，用 `--add-path`/`RPI_INSTALL_ADD_PATH=1` 直接写入。Windows 上安装器会把目录前插到 HKCU 用户 PATH，保留 `%VAR%` 与 registry value kind，去重并广播环境变更；若机器级 PATH 中的 rpi 仍会优先，会给出警告。可用 `--add-path` / `--no-add-path`（Unix）、`-AddPath` / `-NoAddPath`（Windows）或 `RPI_INSTALL_ADD_PATH=1|0`。

```bash
# Unix：非交互直接加入 PATH
curl -fsSL https://revpi.dev/install.sh | RPI_INSTALL_ADD_PATH=1 sh
curl -fsSL https://revpi.dev/install.sh | sh -s -- --add-path
```

```powershell
# Windows：非交互直接加入 PATH
$env:RPI_INSTALL_ADD_PATH = 1; irm https://revpi.dev/install.ps1 | iex
```

手动安装：从 [GitHub Releases](https://github.com/revpidev/rpi/releases) 下载 `rpi-<version>-<target>.tar.gz`（Windows 为 `.zip`）及其 `.sha256` 校验文件，核对校验和后把 `rpi` 二进制解压到 `PATH` 中的任意目录。

### 从源码构建

仅需稳定版 Rust 工具链；workspace MSRV 为 **1.95**（edition 2024）：

```bash
git clone --recurse-submodules https://github.com/revpidev/rpi.git
cd rpi
cargo build --release
./target/release/rpi --provider anthropic --model claude-sonnet-4-20250514
```

`external/` 下的 submodule（`pi`、`pi-subagents`、`rpiv-mono`、`agent-smart-fetch`）是钉死的上游对照，仅用于对拍验证与开发脚本——**构建和运行 rpi 不需要它们**。

在 `~/.rpi/agent/settings.json` / `models.json` 或对应标准环境变量（如 `ANTHROPIC_API_KEY`、`OPENAI_API_KEY`）中配置 API key。Anthropic、OpenAI（ChatGPT）、OpenAI Codex、GitHub Copilot、OpenRouter、Kimi、Meta、xAI 与 Radius 支持通过 `/login` 交互式 OAuth 登录。

Kimi Code 使用 OpenAI Chat Completions，官方 base URL 为 `https://api.kimi.com/coding/v1`。`models.json` 或扩展服务商配置中的自定义 `baseUrl` 必须显式包含完整的 `/v1` 路径段。请将旧的 `https://api.kimi.com/coding` 改为 `https://api.kimi.com/coding/v1`，或使用代理本身明确的 `/v1` URL；rpi 不会自动追加后缀、改写 URL 或回退到旧传输协议。

### 更新

```bash
rpi update              # 更新 rpi 本体（稳定通道）
rpi update --rc         # 更新到最新预发布版本（RC 通道）
rpi update --all        # rpi 本体 + 已安装扩展
rpi update --models     # 刷新远程模型目录
```

重新运行安装脚本也可以更新已有安装。

### 卸载

```bash
rpi self-uninstall           # 删除二进制与安装清单；保留 ~/.rpi
rpi self-uninstall --purge   # 连同 ~/.rpi（会话、认证、设置）一起删除
```

手动清理遗留物：未加 `--purge` 卸载时，`~/.rpi/` 目录需要自行删除；Windows 上运行中的二进制无法删除自身，`self-uninstall` 会打印需要手动删除的准确路径（`rpi.exe`、`rpi.install.json`）。Unix 安装器写入的 `# rpi` PATH 块会被自动移除（symlink profile 和 symlink 安装目录会通过可执行文件路径与 manifest 的逻辑 `installPath` 匹配）；没有标记的手动 PATH 行不会被改动。Windows 上如安装器加入过用户 PATH，请手动删除该条目。

## 使用

不带参数运行 `rpi` 进入交互模式，输入 `/help` 查看内置斜杠命令。

常用选项：

| 选项 | 说明 |
|---|---|
| `--provider <id>` / `--model <id>` | 选择服务商与模型（`rpi --list-models` 浏览目录）；`--provider` 必须配合 `--model` |
| `--continue` / `--resume` | 继续上一个会话 / 恢复指定会话 |
| `--session <path\|id>` / `--session-id <id>` / `--fork <path\|id>` | 打开、创建或派生指定会话 |
| `--print` | 非交互单轮，打印回复后退出 |
| `--mode text\|json\|rpc` | 输出模式；JSON/RPC 线协议见 [docs/json-rpc.md](./docs/json-rpc.md) |
| `--tui-mode regular\|fullscreen` | 选择终端 UI 模式（默认 fullscreen） |
| `--export <file> [output.html]` | 将会话文件导出为 HTML |
| `--tools` / `--exclude-tools` / `--no-tools` | 工具 allowlist/denylist，支持 `*` 模式与 `+name` / `-name` 调整；`--tools` 默认保留 MCP 工具，除非条目以 `mcp__` 开头 |
| `--no-mcp` | 本次运行关闭内建 MCP 客户端 |
| `--offline` | 本次运行关闭所有网络端点 |
| `--use-theme <name[/name]>` | 本次运行使用的初始交互主题 |
| `@<file>` | 将文件（文本或图片）附加到初始消息 |

包管理、MCP 与认证命令：

```bash
rpi install <source>       # registry 名称、github:、npm:、git:、https://、ssh:// 或本地路径
rpi remove <source>        # 卸载扩展（别名 uninstall）
rpi list                   # 列出已安装扩展
rpi update [self|--all|--extensions|--models] [--rc]
rpi config                 # 资源配置 TUI（Tab 切换全局/项目作用域）
rpi mcp add|remove|list|login|logout ...   # 在会话外管理 MCP 服务器
rpi auth check|print-api-key|print-bearer-token ...  # 查看 provider 凭据
```

### 内置工具与扩展

默认启用的工具为 `read`、`bash`、`edit`、`write`。`grep`、`find`、`ls` 可用但默认关闭；通过 `--tools read,bash,edit,write,grep,find,ls` 或 `defaultTools` 启用。

四个隐藏的内建扩展提供 agent 级能力：

| 内建扩展 | 能力 |
|---|---|
| `builtin:codemode` | `codemode` 工具：在 QuickJS（wasmtime）沙箱中运行 JavaScript，调用工具并运行 classifier / image 模型。见 [docs/codemode.md](./docs/codemode.md)。 |
| `builtin:tool-search` | `tool_search` 工具：用 BM25 发现可调用工具，包括延迟加载的 MCP 工具。 |
| `builtin:mcp` | 读取 `~/.rpi/agent/mcp.json` 与 `<cwd>/.rpi/mcp.json`：`mcp__*` 工具、资源工具、`/mcp` 与 OAuth。`rpi-mcp-adapter` 已退役，内建客户端是受支持的路径。 |
| `builtin:llama.cpp` | llama.cpp router 集成（`/llama`）与原生 decision-model classifier。 |

七款第一方扩展以 `.rpix` 形式随宿主版本 lockstep 发布：`rpi-subagents`（委派、后台运行、worktree）、`rpi-smart-fetch`（浏览器指纹 web fetch）、`rpi-statusline`（脚本化 footer）、`rpi-ask-user-question`（结构化提问）、`rpi-todo`（任务列表）、`rpi-plan-mode`（只读规划与批准流）、`rpi-usage`（provider 配额 footer）。

### 权限模式与 plan mode

`Shift+Tab` 循环会话权限模式；`plan` 模式为只读并隐藏写入工具。第一方 `rpi-plan-mode` 扩展额外提供 `/plan`、`write_plan`，以及批准后回到默认模式的对话框。

## 配置

配置位于 `~/.rpi/agent/`（或 `RPI_CODING_AGENT_DIR`）；项目级状态位于 `<cwd>/.rpi/`。设置以环境变量驱动，各产品端点均可覆盖或关闭：

| 环境变量 | 默认值 | 用途 |
|---|---|---|
| `RPI_OFFLINE` | — | 任意真值即关闭全部网络端点 |
| `RPI_SKIP_VERSION_CHECK` | — | 跳过启动时的更新检查 |
| `RPI_VERSION_CHECK_URL` | `https://revpi.dev/api/latest-version` | 更新探测端点；字面量 `off` 关闭 |
| `RPI_MODEL_CATALOG_URL` | `https://revpi.dev` | 远程模型目录基址；字面量 `off` 关闭 |
| `RPI_REGISTRY_URL` | `https://revpi.dev` | 扩展 registry 基址；字面量 `off` 关闭 |
| `RPI_TELEMETRY_URL` | `https://revpi.dev/api/report-install` | 安装计数遥测端点；字面量 `off` 关闭 |
| `RPI_TELEMETRY` | — | 强制开启/关闭遥测（`1/true/yes` 或 `0/false/no`） |
| `RPI_SHARE_VIEWER_URL` | `https://revpi.dev/session` | 会话分享查看页；字面量 `off` 关闭 |
| `RPI_CODING_AGENT_DIR` | `~/.rpi/agent` | 配置与状态目录 |
| `RPI_CODING_AGENT_SESSION_DIR` | `~/.rpi/agent/sessions` | 会话存储目录 |
| `RPI_PACKAGE_DIR` | 可执行文件目录 | 覆盖 package 根目录（Nix/Guix store 路径） |
| `RPI_PROGRAM_STATUS` | 自动探测 | `1`/`0` 强制开启/关闭 OSC 7501 程序状态上报 |
| `RPI_HYPERLINKS` / `RPI_IMAGE_PROTOCOL` / `RPI_TRUE_COLOR` | 自动探测 | 终端能力覆盖 |

同样的 URL 也可在 `settings.json` 中以 `versionCheckUrl`、`modelCatalogUrl`、`telemetryUrl`、`registryUrl` 设置；环境变量优先。

## 产品端点（`revpi.dev`）

| 端点 | 用途 |
|---|---|
| `GET /api/models/providers/{id}` | 各服务商的模型目录 overlay（`{"models":[...]}`）；404 表示"无 overlay，用内置数据" |
| `GET /api/latest-version` / `GET /api/latest-rc-version.json` | 稳定 / RC 版本检查（`{"version","packageName","note"}`）——驱动更新横幅与 `--rc` |
| `POST /api/report-install` | 可选的安装计数遥测（204） |
| `GET /api/extensions/index.json` | 扩展索引，供目录页与 `rpi install` 使用 |
| `GET /api/extensions/{name}.json` | 单扩展版本矩阵，供 `rpi install` 解析版本 |
| `GET /api/extensions/allowlist.json` | 扩展下载代理的仓库白名单 |
| `GET /extensions/download/{owner}/{repo}/{tag}/{file}` | `.rpix` 资产代理（仅白名单仓库） |
| `GET /oauth/client.json`、`GET /oauth/{callbackId}/client.json` | MCP OAuth 的 Client ID Metadata Documents（`clientRegistration: "cimd"`） |
| `/releases/download/v{ver}/{file}` | 官网上的 GitHub Release 资产代理 |
| `/session/#{gistId}` | 导出会话的分享查看页 |
| `/extensions` / `/changelog` / `/docs` | 插件目录、更新记录与文档页 |

站点由 [rpi-pages](https://github.com/revpidev/rpi-pages) 仓库部署在 Cloudflare Pages；用户文档见 <https://revpi.dev/docs>。

## 仓库结构

| 路径 | 内容 |
|---|---|
| `crates/rpi` | CLI、配置、交互/print/JSON/RPC 模式、内置工具、包管理、自更新 |
| `crates/rpi-agent` | Agent 内核（主循环、工具调用、上下文压缩、harness） |
| `crates/rpi-ai` | 模型服务商、认证（API key / OAuth）、远程模型目录、图片/classifier |
| `crates/rpi-tui` | 终端 UI 引擎（渲染、组件、markdown、图片、regular/fullscreen） |
| `crates/rpi-codemode` | `codemode` 工具的 QuickJS-via-wasmtime 沙箱 |
| `crates/rpi-mcp` | MCP 协议客户端（stdio、streamable HTTP、OAuth），供 `builtin:mcp` 使用 |
| `crates/rpi-durable` | 实验性 durable 存储/运行时栈；**不进入默认 CLI 路径** |
| `crates/rpi-ext-host` / `rpi-ext-sdk` | 扩展宿主（L1 Wasm + L0 原生动态库）与 guest SDK |
| `crates/rpi-ext-*` | 七款第一方扩展，以 `.rpix` 发布（与 workspace 版本 lockstep） |
| `crates/rpi-test-*` | Faux provider、测试环境辅助与原生插件 fixture |
| `fixtures/` | 契约/对拍测试 fixtures（生成脚本 + golden 数据） |
| `scripts/` | 开发脚本（上游 pin 校验、对拍 harness、catalog 刷新、发布工具） |
| `external/` | 钉死的上游 submodule：`pi`（v1.4.0）、`pi-subagents`（v0.76.1）、`rpiv-mono`（v2.13.8）、`agent-smart-fetch`（v0.3.17）——见 `UPSTREAM.md` |
| `vendor/syntect` | 构建期文法重写使用的 syntect 补丁 fork（见 `vendor/syntect/PATCHES.md`） |
| `changes/` | 各版本发布说明（changelog 单一事实源） |
| `docs/` | 公开协议/参考文档（`json-rpc.md`、`codemode.md`） |

## 与 Pi 的关系

Rpi 起步于对 Pi 的 Rust 移植，开发期将行为钉死于特定上游版本进行对拍验证（当前与历史基线见 [`UPSTREAM.md`](./UPSTREAM.md)）。项目独立演进：对拍一致是起点而非承诺，两个项目各自发展后行为可能出现偏差。rpi 与 Pi 均为 MIT 许可证。

## 开发

```bash
cargo build --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace     # 全部单元 + 契约测试；不访问真实网络
scripts/verify-upstream.sh # 确认 external/ 下各 submodule 停留在钉死 commit
```

workspace 使用 edition 2024、MSRV 1.95。CI 负责构建发布资产；强制质量门禁在本地执行（见 [`AGENTS.md`](./AGENTS.md) 与 `RELEASING.md`）。

## 状态

| 版本 | 日期 | 主线 |
|---|---|---|
| v0.1 | 2026-08 | 四层核心（`rpi-ai` / `rpi-agent` / `rpi-tui` / `rpi`）+ 扩展宿主（L0 + L1） |
| v0.11 | 2026-08 | JSON/RPC delta-only 线格式、流终止语义、settings 深合并、fullscreen TUI、LaTeX/Mermaid |
| v0.1.3 | 2026-09 | 性能与并发治理 |
| v0.1.4 | 2026-09 | Pi v0.85.0 对齐；交互式自定义 UI ABI；`rpi-ask-user-question`；RC 更新通道 |
| v0.1.5 | 2026-09 | Pi v0.86.1 对齐；`rpi-todo`；prompt cache warming；验证式剪贴板链 |
| v0.1.6 | 2026-10 | Pi v1.0.0 对齐；内建 `codemode` 与 MCP；`rpi-plan-mode` / `rpi-usage`；权限模式与 usage-provider 框架 |
| **v0.1.7** | **2026-10** | **Pi v1.1.0 对齐；issue #57–#71；移除 `rpi-mcp-adapter`；实验性 `rpi-durable` 栈；`rpi-subagents` v0.76.1** |

**当前发布线：v0.1.7。** 最新已发布稳定版为 v0.1.7。使用 `rpi update`（或 `rpi update --all` 同时更新七款锁步插件）；`rpi update --rc` 选择预发布通道。各版本发布说明见 [`changes/`](./changes) 与 [`CHANGELOG.md`](./CHANGELOG.md)。

## 许可证

[MIT](./LICENSE)——与 Pi 相同。