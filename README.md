# Rpi

**The AI coding partner in your terminal — a Rust reimplementation of [Pi](https://github.com/earendil-works/pi).**

[English](./README.md) · [中文](./README.zh-CN.md)

Rpi is a 1:1 behavioral reimplementation of the [Pi coding agent](https://github.com/earendil-works/pi) in Rust: same behavior, same session formats, same extension API shape — with an API shape isomorphic to upstream (Rust/Wasm reimplementation) and naming/package conventions fixed as intentional deviations by ADR. It compiles to a single static binary with no Node, Python, or other runtime dependency.

## Highlights

- ⚡ **Single Rust binary** — statically linked, no runtime required; copy it to any machine and it starts in milliseconds.
- 🖥️ **A complete terminal experience** — multi-turn conversation, streaming output, syntax highlighting, slash commands, Unicode-aware rendering.
- 🛠️ **Gets real work done** — built-in tools (read files, edit code, run commands), long-conversation context compaction that never interrupts the flow, HTML session export, and one-click share links.
- ☁️ **38 model providers built in** — OpenAI, Anthropic, Claude Code, Google, Mistral, DeepSeek, Groq, OpenRouter, Bedrock, Vertex, Codex, Qwen, Kimi, and more, with a remote model catalog served from `revpi.dev`.
- 🔌 **Extensible** — plugins as Wasm (L1) or native dynamic libraries (L0); skills, prompt templates, and themes.
- 🔒 **Privacy under your control** — everything runs locally; every network endpoint can be disabled (see [Configuration](#configuration)).

## Quick start

> **Note**: no prebuilt binaries or published crates yet — build from source (a stable Rust toolchain is the only requirement).

```bash
git clone --recurse-submodules https://github.com/revpidev/rpi.git
cd rpi
cargo build --release
./target/release/rpi --provider anthropic --model claude-sonnet-4-20250514
```

The `external/pi` submodule is the pinned upstream reference used for behavioral parity checks and development scripts — it is **not** required to build or run rpi.

Set your API key via the standard environment variable for your provider (e.g. `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`), or in `~/.rpi/settings.json`. Anthropic, OpenAI Codex, and radius also support interactive OAuth login.

## Usage

Run `rpi` with no arguments to start the interactive mode; type `/help` for the built-in slash commands.

Common options:

| Option | Description |
|---|---|
| `--provider <id>` / `--model <id>` | Choose provider and model (run `rpi --list-models` to browse the catalog) |
| `--continue` / `--resume` | Continue the last session or resume a named one |
| `--session <id>` / `--fork <id>` | Open or fork a specific session |
| `--print` | Non-interactive single turn, prints the reply and exits |
| `--export <format>` | Export a session (e.g. HTML) |
| `--offline` | Disable all network endpoints for this run |
| `rpi <message>` | One-shot prompt in non-interactive mode |

Package management commands:

```bash
rpi update --self          # self-update (binary installs print a download instruction)
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
| `external/pi` | Upstream Pi checkout (git submodule, pinned — see `UPSTREAM.md`) |

## Upstream parity

Rpi is a 1:1 reimplementation of Pi: behavior is pinned against a specific upstream commit, verified by a parity checklist (session wire format, extension API, TUI frames). The pinned version and intentional deviations are recorded in [`UPSTREAM.md`](./UPSTREAM.md) — do not update the baseline without an ADR. The project is MIT-licensed, matching Pi.

## Development

```bash
cargo build --workspace
cargo test --workspace     # all unit + contract tests; no live network access
cargo test -p rpi-ai --test model_catalog --test compat_matrix
scripts/verify-upstream.sh # confirm external/pi is at the pinned commit
```

## Status

v0.1 (T01–T16) complete: four-layer crates (`rpi-ai` / `rpi-agent` / `rpi-tui` / `rpi`) plus the extension host (L0 native + L1 Wasm) delivered. v0.11 in progress (upstream baseline raised to `4181f66` / v0.84.1+).

## License

[MIT](./LICENSE) — same as Pi.
