# Rpi

**The AI coding partner in your terminal — written in Rust, derived from [Pi](https://github.com/earendil-works/pi).**

[English](./README.md) · [Chinese](./README.zh-CN.md)

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](./LICENSE)
[![Language: Rust](https://img.shields.io/badge/Language-Rust-orange?logo=rust&logoColor=white)](./Cargo.toml)
[![Platform: Linux · macOS · Windows](https://img.shields.io/badge/Platform-Linux%C2%B7macOS%C2%B7Windows-lightgrey)]()
[![Build](https://img.shields.io/github/actions/workflow/status/revpidev/rpi/build.yml)](https://github.com/revpidev/rpi/actions/workflows/build.yml)

Rpi is a terminal AI coding agent written in Rust, derived from the [Pi coding agent](https://github.com/earendil-works/pi). It inherits Pi's architecture, and behavior is continuously verified against a pinned upstream release — the current behavioral baseline is **Pi v1.4.0** (`4a080252f`, see [`UPSTREAM.md`](./UPSTREAM.md)). Rpi is an independent project and may diverge from Pi as both evolve. It compiles to a single static binary with no Node, Python, or other runtime dependency.

The current release line is **v0.1.7**, the latest stable release (2026-10-19). Install with the script below, or update an existing install with `rpi update` (see [Status](#status)).

## Highlights

- ⚡ **Single Rust binary** — statically linked, no runtime required; copy it to any machine and it starts in milliseconds.
- 🖥️ **A complete terminal experience** — multi-turn conversation, streaming output, syntax highlighting, slash commands, Unicode-aware rendering, inline images, LaTeX and Mermaid, and regular/fullscreen TUI modes.
- 🛠️ **Gets real work done** — built-in `read` / `bash` / `edit` / `write` tools (plus optional `grep` / `find` / `ls`), long-conversation context compaction, HTML session export, and share links.
- 🧰 **Built-in agent surfaces** — `codemode` (sandboxed JavaScript that composes tools and non-LLM models), `tool_search` (BM25 tool discovery), and a built-in MCP client for stdio and streamable-HTTP servers with OAuth.
- ☁️ **42 model providers built in** — OpenAI, Anthropic, Claude Code, OpenAI Codex, Google, Vertex, Bedrock, Azure, Mistral, DeepSeek, Groq, OpenRouter, Qwen Token Plan, Kimi, Meta, MiniMax, xAI, Xiaomi MiMo, Cloudflare, and more, with a remote model catalog served from `revpi.dev`.
- 🔌 **Extensible** — L1 Wasm and L0 native dynamic-library plugins; seven first-party extensions (`rpi-subagents`, `rpi-smart-fetch`, `rpi-statusline`, `rpi-ask-user-question`, `rpi-todo`, `rpi-plan-mode`, `rpi-usage`), plus skills, prompt templates, and themes.
- 🔒 **Privacy under your control** — everything runs locally; every network endpoint can be disabled (see [Configuration](#configuration)).

## Quick start

### Install (prebuilt binary)

macOS / Linux (POSIX sh; glibc vs musl is detected automatically):

```bash
curl -fsSL https://revpi.dev/install.sh | sh
```

The installer detects your OS/architecture, downloads the matching release archive, verifies its SHA-256 (an integrity check against corrupted downloads), and installs to `~/.local/bin` (override with `--prefix <dir>`). Both the scripts and the release assets are served from the official site — when GitHub is unreachable the installer automatically falls back to the revpi.dev mirror, and you can always download assets manually from GitHub Releases. If `~/.local/bin` is not on your `PATH`, add it:

```bash
export PATH="$HOME/.local/bin:$PATH"   # append to ~/.profile, ~/.bashrc or ~/.zshrc
```

Windows (PowerShell; installs to `%LOCALAPPDATA%\Programs\rpi`, override with `-Prefix`):

```powershell
irm https://revpi.dev/install.ps1 | iex
```

Both installers offer to add the install directory to your PATH when it is not already the active `rpi`. On macOS/Linux an interactive install (terminal stdout, usable `/dev/tty`, no `CI`/`RPI_NONINTERACTIVE`, `TERM` not `dumb`) asks once (default yes) and writes a marked `# rpi` block with an idempotent `case` guard to the detected profile: fish (`fish_add_path`), zsh, bash (`.bashrc`, then `.bash_profile`/`.bash_login`, then `.profile`), or the POSIX `~/.profile`. Unsupported shells only print manual guidance; root/system-wide or outside-`$HOME` prefixes print the exact manual command. The prompt is skipped when `CI`/`RPI_NONINTERACTIVE` is set, stdout is redirected, or no controlling terminal exists. If a pty is attached without those markers, shells with `read -t` time out after `RPI_INSTALL_PROMPT_TIMEOUT` seconds (default 60, treated as "no"); set `RPI_INSTALL_ADD_PATH=0` (or `--no-add-path`) to guarantee no wait, and `--add-path`/`RPI_INSTALL_ADD_PATH=1` to write without prompting. On Windows the installer prepends to the HKCU user PATH, preserves `%VAR%` entries and the registry value kind, removes duplicates, broadcasts `WM_SETTINGCHANGE`, and warns if a machine-level `rpi` still wins in a new terminal. Use `--add-path` / `--no-add-path` (Unix), `-AddPath` / `-NoAddPath` (Windows), or set `RPI_INSTALL_ADD_PATH=1|0` for non-interactive runs.

```bash
# Unix: non-interactive opt-in
curl -fsSL https://revpi.dev/install.sh | RPI_INSTALL_ADD_PATH=1 sh
curl -fsSL https://revpi.dev/install.sh | sh -s -- --add-path
```

```powershell
# Windows: non-interactive opt-in
$env:RPI_INSTALL_ADD_PATH = 1; irm https://revpi.dev/install.ps1 | iex
```

Manual install: download `rpi-<version>-<target>.tar.gz` (`.zip` on Windows) and its `.sha256` sidecar from [GitHub Releases](https://github.com/revpidev/rpi/releases), verify the checksum, and unpack the `rpi` binary anywhere on your `PATH`.

### Build from source

A stable Rust toolchain is the only requirement; the workspace MSRV is **1.95** (edition 2024):

```bash
git clone --recurse-submodules https://github.com/revpidev/rpi.git
cd rpi
cargo build --release
./target/release/rpi --provider anthropic --model claude-sonnet-4-20250514
```

The `external/` submodules (`pi`, `pi-subagents`, `rpiv-mono`, `agent-smart-fetch`) are pinned upstream references used for parity verification and development scripts — they are **not** required to build or run rpi.

Set your API key via the standard environment variable for your provider (e.g. `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`), or in `~/.rpi/agent/settings.json` / `models.json`. Anthropic, OpenAI (ChatGPT), OpenAI Codex, GitHub Copilot, OpenRouter, Kimi, Meta, xAI, and Radius support interactive OAuth login through `/login`.

### Updating

```bash
rpi update              # update rpi itself (stable channel)
rpi update --rc         # update to the latest release candidate (pre-release channel)
rpi update --all        # rpi + installed extensions
rpi update --models     # refresh the remote model catalog
```

Re-running the install script also updates an existing installation.

### Uninstalling

```bash
rpi self-uninstall           # removes the binary and install manifest; keeps ~/.rpi
rpi self-uninstall --purge   # also deletes ~/.rpi (sessions, auth, settings)
```

Manual leftovers: if you uninstalled without `--purge`, delete `~/.rpi/` yourself once you no longer need it; on Windows the running binary cannot delete itself, so `self-uninstall` prints the exact paths (`rpi.exe`, `rpi.install.json`) to remove by hand. The Unix installer's marked `# rpi` PATH block is removed automatically (symlinked profiles and symlinked install directories are matched through the executable path and the manifest's logical `installPath`); manual PATH lines without the marker are left alone. On Windows remove the user PATH entry manually if the installer added it.

## Usage

Run `rpi` with no arguments to start the interactive mode; type `/help` for the built-in slash commands.

Common options:

| Option | Description |
|---|---|
| `--provider <id>` / `--model <id>` | Choose provider and model (run `rpi --list-models` to browse the catalog); `--provider` requires `--model` |
| `--continue` / `--resume` | Continue the last session or resume a named one |
| `--session <path\|id>` / `--session-id <id>` / `--fork <path\|id>` | Open, create, or fork a specific session |
| `--print` | Non-interactive single turn, prints the reply and exits |
| `--mode text\|json\|rpc` | Output mode; JSON/RPC follow the wire contract in [docs/json-rpc.md](./docs/json-rpc.md) |
| `--tui-mode regular\|fullscreen` | Select the terminal UI mode (fullscreen is the default) |
| `--export <file> [output.html]` | Export a session file to HTML |
| `--tools` / `--exclude-tools` / `--no-tools` | Tool allowlist/denylist with `*` patterns and `+name` / `-name` adjustments; `--tools` keeps MCP tools unless an entry starts with `mcp__` |
| `--no-mcp` | Disable the built-in MCP client for this run |
| `--offline` | Disable all network endpoints for this run |
| `--use-theme <name[/name]>` | Initial interactive theme for this run |
| `@<file>` | Attach a file (text or image) to the initial message |

Package, MCP, and auth commands:

```bash
rpi install <source>       # registry name, github:, npm:, git:, https://, ssh://, or a local path
rpi remove <source>        # remove an extension (alias: uninstall)
rpi list                   # list installed extensions
rpi update [self|--all|--extensions|--models] [--rc]
rpi config                 # resource-configuration TUI (Tab switches global/project scope)
rpi mcp add|remove|list|login|logout ...   # manage MCP servers outside a session
rpi auth check|print-api-key|print-bearer-token ...  # inspect provider credentials
```

### Built-in tools and extensions

The default active tools are `read`, `bash`, `edit`, and `write`. `grep`, `find`, and `ls` are available but off by default; enable them with `--tools read,bash,edit,write,grep,find,ls` or `defaultTools`.

Four hidden built-in extensions provide agent-level surfaces:

| Built-in | Surface |
|---|---|
| `builtin:codemode` | The `codemode` tool: sandboxed JavaScript (QuickJS via wasmtime) that calls tools and runs classifier/image models. See [docs/codemode.md](./docs/codemode.md). |
| `builtin:tool-search` | The `tool_search` tool: BM25 discovery of callable tools, including deferred MCP tools. |
| `builtin:mcp` | MCP servers from `~/.rpi/agent/mcp.json` and `<cwd>/.rpi/mcp.json`: `mcp__*` tools, resource tools, `/mcp`, and OAuth. `rpi-mcp-adapter` is retired — the built-in client is the supported path. |
| `builtin:llama.cpp` | llama.cpp router integration (`/llama`) and native decision-model classifiers. |

Seven first-party extensions ship as `.rpix` archives in lockstep with the host release: `rpi-subagents` (delegation, background runs, worktrees), `rpi-smart-fetch` (fingerprinted web fetch), `rpi-statusline` (script-driven footer), `rpi-ask-user-question` (structured questions), `rpi-todo` (task list), `rpi-plan-mode` (read-only planning and approval), and `rpi-usage` (provider quota footer).

### Permission modes and plan mode

`Shift+Tab` cycles the session permission mode; `plan` mode is read-only and hides write tools. The first-party `rpi-plan-mode` extension adds `/plan`, `write_plan`, and an approval dialog that returns the session to the default mode.

## Configuration

Configuration lives in `~/.rpi/agent/` (or `RPI_CODING_AGENT_DIR`); project-local state lives in `<cwd>/.rpi/`. Settings are environment-variable-driven, and the product endpoints can each be overridden or disabled:

| Env var | Default | Purpose |
|---|---|---|
| `RPI_OFFLINE` | — | Any truthy value disables all network endpoints |
| `RPI_SKIP_VERSION_CHECK` | — | Skip the startup update check |
| `RPI_VERSION_CHECK_URL` | `https://revpi.dev/api/latest-version` | Update probe endpoint; literal `off` disables |
| `RPI_MODEL_CATALOG_URL` | `https://revpi.dev` | Remote model catalog base URL; literal `off` disables |
| `RPI_REGISTRY_URL` | `https://revpi.dev` | Extension registry base URL; literal `off` disables |
| `RPI_TELEMETRY_URL` | `https://revpi.dev/api/report-install` | Install-count telemetry endpoint; literal `off` disables |
| `RPI_TELEMETRY` | — | Force telemetry on/off (`1/true/yes` or `0/false/no`) |
| `RPI_SHARE_VIEWER_URL` | `https://revpi.dev/session` | Session share viewer; literal `off` disables |
| `RPI_CODING_AGENT_DIR` | `~/.rpi/agent` | Config/state directory |
| `RPI_CODING_AGENT_SESSION_DIR` | `~/.rpi/agent/sessions` | Session storage directory |
| `RPI_PACKAGE_DIR` | executable directory | Override the package root (Nix/Guix store paths) |
| `RPI_PROGRAM_STATUS` | auto-detect | `1`/`0` forces OSC 7501 program-status reporting on/off |
| `RPI_HYPERLINKS` / `RPI_IMAGE_PROTOCOL` / `RPI_TRUE_COLOR` | auto-detect | Terminal capability overrides |

The same URLs can be set as `versionCheckUrl`, `modelCatalogUrl`, `telemetryUrl`, and `registryUrl` in `settings.json`; environment variables take precedence.

## Product endpoints (`revpi.dev`)

| Endpoint | Purpose |
|---|---|
| `GET /api/models/providers/{id}` | Per-provider model catalog overlay (`{"models":[...]}`); 404 means "no overlay, use built-in data" |
| `GET /api/latest-version` / `GET /api/latest-rc-version.json` | Stable / RC version check (`{"version","packageName","note"}`) — powers the update banner and `--rc` |
| `POST /api/report-install` | Optional install-count telemetry (204) |
| `GET /api/extensions/index.json` | Extension index used by the directory page and `rpi install` |
| `GET /api/extensions/{name}.json` | Per-extension version matrix used by `rpi install` |
| `GET /api/extensions/allowlist.json` | Repository allowlist for the extension download proxy |
| `GET /extensions/download/{owner}/{repo}/{tag}/{file}` | `.rpix` asset proxy (allowlisted repositories only) |
| `GET /oauth/client.json`, `GET /oauth/{callbackId}/client.json` | Client ID Metadata Documents for MCP OAuth (`clientRegistration: "cimd"`) |
| `/releases/download/v{ver}/{file}` | GitHub Release asset proxy on the official site |
| `/session/#{gistId}` | Share viewer for exported sessions |
| `/extensions` / `/changelog` / `/docs` | Directory, changelog, and documentation pages |

The site is deployed from the [rpi-pages](https://github.com/revpidev/rpi-pages) repository on Cloudflare Pages; user docs live at <https://revpi.dev/docs>.

## Repository layout

| Path | Contents |
|---|---|
| `crates/rpi` | CLI, config, interactive/print/JSON/RPC modes, built-in tools, package management, self-update |
| `crates/rpi-agent` | Agent core (loop, tool calling, compaction, harness) |
| `crates/rpi-ai` | Model providers, auth (API keys / OAuth), remote model catalog, images/classifiers |
| `crates/rpi-tui` | Terminal UI engine (rendering, components, markdown, images, regular/fullscreen) |
| `crates/rpi-codemode` | QuickJS-via-wasmtime sandbox for the `codemode` tool |
| `crates/rpi-mcp` | MCP protocol client (stdio, streamable HTTP, OAuth) used by `builtin:mcp` |
| `crates/rpi-durable` | Experimental durable storage/runtime stack; **not** on the default CLI path |
| `crates/rpi-ext-host` / `rpi-ext-sdk` | Extension host (L1 Wasm + L0 native dynamic libraries) and the guest SDK |
| `crates/rpi-ext-*` | Seven first-party extensions shipped as `.rpix` (lockstep with the workspace version) |
| `crates/rpi-test-*` | Faux provider, test environment helpers, and a native-plugin fixture |
| `fixtures/` | Contract/parity test fixtures (generator scripts + golden data) |
| `scripts/` | Dev scripts (upstream pin verification, parity harnesses, catalog refresh, release tooling) |
| `external/` | Pinned upstream submodules: `pi` (v1.4.0), `pi-subagents` (v0.76.1), `rpiv-mono` (v2.13.8), `agent-smart-fetch` (v0.3.17) — see `UPSTREAM.md` |
| `vendor/syntect` | Patched syntect fork used by the build-time grammar rewrite (see `vendor/syntect/PATCHES.md`) |
| `changes/` | Per-version release notes (the changelog single source of truth) |
| `docs/` | Public protocol/reference docs (`json-rpc.md`, `codemode.md`) |

## Relationship to Pi

Rpi started as a Rust port of Pi, and behavior is pinned against an upstream release for parity verification (see [`UPSTREAM.md`](./UPSTREAM.md) for the current and historical baselines). The project is independent: parity with Pi is a starting point, not a guarantee — behavior may diverge as both projects evolve. Both rpi and Pi are MIT-licensed.

## Development

```bash
cargo build --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace     # all unit + contract tests; no live network access
scripts/verify-upstream.sh # confirm every external/ submodule is at its pinned commit
```

The workspace uses edition 2024 and MSRV 1.95. CI builds release artifacts; the mandatory quality gates are local (see [`AGENTS.md`](./AGENTS.md) and `RELEASING.md`).

## Status

| Version | Date | Main line |
|---|---|---|
| v0.1 | 2026-08 | Four-layer core (`rpi-ai` / `rpi-agent` / `rpi-tui` / `rpi`) + extension host (L0 + L1) |
| v0.11 | 2026-08 | Delta-only JSON/RPC wire format, stream-termination semantics, settings deep-merge, fullscreen TUI, LaTeX/Mermaid |
| v0.1.3 | 2026-09 | Performance and concurrency governance |
| v0.1.4 | 2026-09 | Pi v0.85.0 parity; interactive custom-UI ABI; `rpi-ask-user-question`; RC update channel |
| v0.1.5 | 2026-09 | Pi v0.86.1 parity; `rpi-todo`; cache warming; verified clipboard chain |
| v0.1.6 | 2026-10 | Pi v1.0.0 parity; built-in `codemode` and MCP; `rpi-plan-mode` / `rpi-usage`; permission modes and usage-provider framework |
| **v0.1.7** | **2026-10** | **Pi v1.1.0 parity; issues #57–#71; `rpi-mcp-adapter` removal; experimental `rpi-durable` stack; `rpi-subagents` v0.76.1** |

**Current line: v0.1.7.** The latest published stable release is v0.1.7. Use `rpi update` (or `rpi update --all` to also update the seven lockstep extensions); `rpi update --rc` opts into the pre-release channel. Release notes for every version are in [`changes/`](./changes) and [`CHANGELOG.md`](./CHANGELOG.md).

## License

[MIT](./LICENSE) — same as Pi.