# Rpi

**The AI coding partner in your terminal — written in Rust, derived from [Pi](https://github.com/earendil-works/pi).**

[English](./README.md) · [中文](./README.zh-CN.md)

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](./LICENSE)
[![Language: Rust](https://img.shields.io/badge/Language-Rust-orange?logo=rust&logoColor=white)](./Cargo.toml)
[![Platform: Linux · macOS · Windows](https://img.shields.io/badge/Platform-Linux%C2%B7macOS%C2%B7Windows-lightgrey)]()
[![Build](https://img.shields.io/github/actions/workflow/status/revpidev/rpi/build.yml)](https://github.com/revpidev/rpi/actions/workflows/build.yml)

Rpi is a terminal AI coding agent written in Rust, derived from the [Pi coding agent](https://github.com/earendil-works/pi). It inherits Pi's architecture, and early behavior is kept in parity with upstream — but rpi is an independent project, and the two may diverge as it evolves. It compiles to a single static binary with no Node, Python, or other runtime dependency.

## Highlights

- ⚡ **Single Rust binary** — statically linked, no runtime required; copy it to any machine and it starts in milliseconds.
- 🖥️ **A complete terminal experience** — multi-turn conversation, streaming output, syntax highlighting, slash commands, Unicode-aware rendering.
- 🛠️ **Gets real work done** — built-in tools (read files, edit code, run commands), long-conversation context compaction that never interrupts the flow, HTML session export, and one-click share links.
- ☁️ **38 model providers built in** — OpenAI, Anthropic, Claude Code, Google, Mistral, DeepSeek, Groq, OpenRouter, Bedrock, Vertex, Codex, Qwen, Kimi, and more, with a remote model catalog served from `revpi.dev`.
- 🔌 **Extensible** — plugins as Wasm (L1) or native dynamic libraries (L0); skills, prompt templates, and themes.
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

Manual install: download `rpi-<version>-<target>.tar.gz` (`.zip` on Windows) and its `.sha256` sidecar from [GitHub Releases](https://github.com/revpidev/rpi/releases), verify the checksum, and unpack the `rpi` binary anywhere on your `PATH`.

### Build from source

A stable Rust toolchain is the only requirement:

```bash
git clone --recurse-submodules https://github.com/revpidev/rpi.git
cd rpi
cargo build --release
./target/release/rpi --provider anthropic --model claude-sonnet-4-20250514
```

The `external/pi` submodule is the pinned upstream baseline used for parity reference and development scripts — it is **not** required to build or run rpi.

Set your API key via the standard environment variable for your provider (e.g. `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`), or in `~/.rpi/settings.json`. Anthropic, OpenAI Codex, and radius also support interactive OAuth login.

### Updating

```bash
rpi update --self   # download, verify, and replace the binary in place
```

Re-running the install script also updates an existing installation.

### Uninstalling

```bash
rpi self-uninstall           # removes the binary and install manifest; keeps ~/.rpi
rpi self-uninstall --purge   # also deletes ~/.rpi (sessions, auth, settings)
```

Manual leftovers: if you uninstalled without `--purge`, delete `~/.rpi/` yourself once you no longer need it; on Windows the running binary cannot delete itself, so `self-uninstall` prints the exact paths (`rpi.exe`, `rpi.install.json`) to remove by hand.

## Usage

Run `rpi` with no arguments to start the interactive mode; type `/help` for the built-in slash commands.

Common options:

| Option | Description |
|---|---|
| `--provider <id>` / `--model <id>` | Choose provider and model (run `rpi --list-models` to browse the catalog) |
| `--continue` / `--resume` | Continue the last session or resume a named one |
| `--session <id>` / `--fork <id>` | Open or fork a specific session |
| `--print` | Non-interactive single turn, prints the reply and exits |
| `--tui-mode regular\|fullscreen` | Select the terminal UI mode: regular (inline) or fullscreen (alt-screen) |
| `--export <format>` | Export a session (e.g. HTML) |
| `--offline` | Disable all network endpoints for this run |
| `rpi <message>` | One-shot prompt in non-interactive mode |

Package management commands:

```bash
rpi update --self          # self-update to the latest release
rpi update --models        # refresh the remote model catalog
rpi update --extensions    # update extensions (or --all)
rpi install <source>       # install an extension
rpi remove <source>        # remove an extension
rpi list                   # list installed extensions
rpi config                 # review approved project trust decisions
```

## Configuration

Configuration lives in `~/.rpi/` (or `RPI_CODING_AGENT_DIR`). Settings are environment-variable-driven; the product endpoints can each be overridden or disabled:

| Env var | Default | Purpose |
|---|---|---|
| `RPI_OFFLINE` | — | Any value disables all network endpoints |
| `RPI_SKIP_VERSION_CHECK` | — | Skip the startup update check |
| `RPI_VERSION_CHECK_URL` | `https://revpi.dev/api/latest-version` | Update probe endpoint; literal `off` disables |
| `RPI_MODEL_CATALOG_URL` | `https://revpi.dev` | Remote model catalog base URL; literal `off` disables |
| `RPI_TELEMETRY_URL` | `https://revpi.dev/api/report-install` | Install-count telemetry endpoint; literal `off` disables |
| `RPI_SHARE_VIEWER_URL` | `https://revpi.dev/session` | Session share viewer; literal `off` disables |
| `RPI_CODING_AGENT_DIR` | `~/.rpi` | Config/state directory |
| `RPI_CODING_AGENT_SESSION_DIR` | `~/.rpi/agent/sessions` | Session storage directory |

The same URLs can be set as `versionCheckUrl`, `modelCatalogUrl`, `telemetryUrl` in `settings.json`; environment variables take precedence.

## Product endpoints (`revpi.dev`)

| Endpoint | Purpose |
|---|---|
| `GET /api/models/providers/{id}` | Per-provider model catalog overlay (`{"models":[...]}`); 404 means "no overlay, use built-in data" |
| `GET /api/latest-version` | Version check `{"version","packageName","note"}` — powers the update banner |
| `POST /api/report-install` | Optional install-count telemetry (204) |
| `/session/#{gistId}` | Share viewer for exported sessions |

The site is deployed from the [rpi-pages](https://github.com/revpidev/rpi-pages) repository on Cloudflare Pages; docs live at <https://revpi.dev/docs>.

## Repository layout

| Path | Contents |
|---|---|
| `crates/rpi` | CLI, config, interactive mode, built-in tools, package management |
| `crates/rpi-agent` | Agent core (loop, tool calling, compaction, harness) |
| `crates/rpi-ai` | Model providers, auth (API keys / OAuth), remote model catalog |
| `crates/rpi-tui` | Terminal UI engine (rendering, components, markdown, images) |
| `crates/rpi-ext-host` | Extension host: L1 Wasm + L0 native dynamic libraries |
| `crates/rpi-ext-sdk` | SDK crate for writing extensions |
| `fixtures/` | Contract/parity test fixtures (generator scripts + golden data) |
| `scripts/` | Dev scripts (upstream pin verification, catalog refresh, data generation) |
| `external/pi` | Upstream Pi checkout (git submodule, pinned parity baseline — see `UPSTREAM.md`) |

## Relationship to Pi

Rpi started as a Rust port of Pi, and during early development its behavior was pinned against a specific upstream commit for parity verification (see [`UPSTREAM.md`](./UPSTREAM.md) for the pinned baseline). The project is independent: parity with Pi is a starting point, not a guarantee — behavior may diverge as both projects evolve. Both rpi and Pi are MIT-licensed.

## Development

```bash
cargo build --workspace
cargo test --workspace     # all unit + contract tests; no live network access
cargo test -p rpi-ai --test model_catalog --test compat_matrix
scripts/verify-upstream.sh # confirm external/pi is at the pinned commit
```

## Status

v0.1 (T01–T16) complete: four-layer crates (`rpi-ai` / `rpi-agent` / `rpi-tui` / `rpi`) plus the extension host (L0 native + L1 Wasm) delivered. v0.11 in progress (upstream baseline raised to `4181f66` / v0.84.1+): JSON/RPC delta-only wire format, stream-termination semantics across all provider families, settings deep-merge, length-stop recovery chain, auth command family (`auth check` / `print-api-key`), extension API additions (model registry, markdown transformers), TUI trait refactor with dual renderers (regular + fullscreen alt-screen via `--tui-mode`), LaTeX/Mermaid rendering, and a viewport layout engine.

## License

[MIT](./LICENSE) — same as Pi.
