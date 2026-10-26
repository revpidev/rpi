# AGENTS.md — Working Conventions for the rpi Repository

Guidance for AI coding agents (and human contributors) making changes to this
repository. Read this before committing anything. It complements — and never
overrides — `RELEASING.md` (release mechanics), `UPSTREAM.md` (the behavioral
pin), and `README.md` (product overview).

## Language standard (mandatory)

All GitHub-facing content is written in **English by default**:

- **Commit messages** — subject and body, in every repository of the
  `revpidev` organization (rpi, rpi-docs, rpi-pages).
- **Pull requests** — titles and descriptions.
- **Issues and discussions** — reports, comments, labels.
- **Release notes** — the `changes/v<version>.md` files are the single source
  of truth for GitHub Release bodies and must be English.
- **Repository documentation** — README (except the Chinese edition),
  `CHANGELOG.md`, `RELEASING.md`, `docs/`, `changes/README.md`, fixture and
  harness READMEs under `fixtures/` and `scripts/`.

Chinese is allowed **only** in these deliberate exceptions:

- `README.zh-CN.md` — the Simplified-Chinese edition of the README.
- The bilingual user-facing copy of the revpi.dev website (rpi-pages
  `changelog.html` and friends), which carries parallel `l-zh`/`l-en` spans
  as a product surface for Chinese users.
- The internal version-planning documents of the docs repository — the
  `v0.1.x/` baselines, `plan/` task catalogs (including `plan/extensions/`),
  `extensions/` port specs, and `adr/` records in `rpi-docs`. These stay a
  standing Chinese surface (the planning back catalog, confirmed 2026-09-20);
  commit messages in `rpi-docs` follow the English rule regardless.

Legacy Chinese that still exists (comments inside source code, generated
harness reports, the internal planning back catalog in the docs repository)
migrates **incrementally**: any file you touch or newly create must end up
English; never expand a Chinese surface. Do not hand-translate generated
artifacts (e.g. `fixtures/generated/**`) — fix their generator instead when
it is in scope.

## Repository layout

| Path | What it is |
|---|---|
| `crates/rpi` | Main binary: CLI, interactive TUI, core services, packaging/updater |
| `crates/rpi-agent` | Agent loop, tools, session management |
| `crates/rpi-ai` | Provider adapters, model catalog (`providers/data`), streaming, auth/OAuth |
| `crates/rpi-tui` | Terminal UI library (rendering, widgets, mouse, theming, regular/fullscreen) |
| `crates/rpi-codemode` | QuickJS-via-wasmtime sandbox for the built-in `codemode` tool |
| `crates/rpi-mcp` | MCP protocol client used by the built-in `mcp` extension (stdio, streamable HTTP, OAuth) |
| `crates/rpi-durable` | Experimental durable storage/runtime stack; **not** on the default CLI path (no dependency from `crates/rpi`) |
| `crates/rpi-ext-*` | Seven first-party L0 extension crates shipped as `.rpix` — **lockstep** with the workspace version (`version.workspace = true`) |
| `crates/rpi-ext-host` / `rpi-ext-sdk` | Extension host ABI and SDK used by plugins (L1 Wasm + L0 native dynamic libraries) |
| `crates/rpi-test-*` | Faux provider, test-environment helpers, and the native-plugin fixture |
| `external/` | **Pinned, read-only upstream references** (git submodules): `pi` (gold standard), `pi-subagents`, `agent-smart-fetch`, `rpiv-mono`. Never commit changes inside `external/`. |
| `vendor/syntect` | Patched fork of syntect (visibility patches for the build-time grammar rewrite, issue #47). Any new patch must be documented in `vendor/syntect/PATCHES.md`. |
| `fixtures/` | Recorded golden data (sessions, themes, upstream snapshots). Re-recording follows the documented runbooks. |
| `scripts/` | Parity harnesses (`subagents-parity`, `mcp-parity`, `interactive-ui-parity`, …) and release tooling (`unwrap-release-notes.py`). |
| `changes/` | Per-version release notes — the changelog single source of truth. |
| `docs/` | Public protocol/reference contracts (`json-rpc.md`, `codemode.md`). |

## Non-negotiable rules

1. **Upstream parity pin.** The behavioral gold standard is pinned in
   `UPSTREAM.md` (currently `4a080252f`, Pi v1.4.0). Behavior changes that
   diverge from upstream require an ADR first (registered in the docs
   repository). The red-line pins (`4a080252f` / subagents `7d072b91d` /
   smart-fetch `b0111612` / rpiv-mono `afc21bc2`) move only via the documented
   rebase process, never opportunistically. (mcp-adapter was removed from the
   pin set with ADR-0037, TE49.)
2. **Quality gates are local and mandatory.** CI builds releases but is not
   the test gate (see the rc.12 note in `changes/v0.1.4.md`). Before every
   merge, run and pass:
   - `cargo fmt --check`
   - `cargo clippy --workspace --all-targets -- -D warnings`
   - `cargo test --workspace`
   - `sh scripts/test-install-sh.sh` (POSIX installer; also exercises the
     profile/pty paths)
   - `pwsh -NoProfile -File scripts/test-install-ps1.ps1` (PowerShell
     installer helpers; runs on Linux pwsh and Windows PowerShell 5.1)
   Before a release, double-run the full test suite. A handful of tests are
   environment-sensitive (fixed ports, timing under load); if one flakes,
   verify it in isolation before drawing conclusions.
3. **Regression tests or it didn't happen.** Bug fixes land with a test that
   fails when the fix is reverted ("only revert the source, keep the test"
   red/green evidence is the house style for review-driven fixes).
4. **Changelog discipline.** User-visible changes update
   `changes/v<version>.md` (single source of truth) **and** the two compiled
   surfaces in the same change set: `CHANGELOG.md` (embedded into the binary
   via `include_str!`, drives `/changelog`) and the rpi-pages
   `changelog.html` section. Keep paragraphs single-line; the release
   pipeline reflows legacy hard-wrapped text via
   `scripts/unwrap-release-notes.py`. RC entries fold into the stable file
   at stable-release time.
5. **Versioning.** The workspace version and the lockstep extension crates
   bump together; update `Cargo.lock` in the same commit (v0.1.2 lesson).
   RC versions follow `<stable>-rc.<N>` (lowercase, dotted, no leading
   zeros, baseline = the candidate stable).
6. **Docs travel with behavior.** User-visible surfaces update
   `README.md` / `README.zh-CN.md` in the same change set; wire-protocol
   changes update `docs/json-rpc.md`, and codemode surface changes update
   `docs/codemode.md`. Every relative link you add must resolve in the
   repository.

## Commit conventions

- Conventional Commits style: `type(scope): summary` — e.g.
  `fix(statusline): freeze decode_ms across message_end (#50)`,
  `docs(changes): fold rc.9 into v0.1.4`.
- **English only** (see the language standard). Keep the subject ≤ ~72
  characters where practical; the body carries detail.
- Reference issues and upstream anchors: `rpi#NN` for project issues,
  `#NN` for upstream Pi PRs/issues, commit hashes for upstream anchors.
- One logical change per commit; release bumps are their own commits
  (`chore(release): ...` / `release: ...`).

## Release essentials (full checklist in RELEASING.md)

- **Every distribution packaging and publishing action must be initiated by
  a human** (ADR-0042): `.rpix` dry-runs, release builds, tags, GitHub
  Releases, registry/site updates and issue closing. Agents may prepare files,
  commands and evidence but must not perform those actions autonomously.
  Ordinary development builds and tests are not distribution packaging.
- `.github/workflows/build.yml` now supports only `workflow_dispatch`
  distribution builds; tag pushes never start packaging automatically. A
  human must start each build through the GitHub UI or `gh workflow run`.
  Do not treat pushing a tag as authorization to package or publish.
- Order remains **assets before endpoints**: only after the 12 core files and
  the extension assets appear may a human refresh
  `revpi.dev/api/latest-version.json` via
  `rpi-pages/scripts/generate-site.py --version <version>`. Re-running builds
  or site generation after a tag re-push is also a human-initiated action.
- Stable releases never touch the RC endpoint; RC releases never touch the
  stable endpoint (the generator enforces both).

## Things that will get a change rejected

- Chinese in any GitHub-facing surface listed above.
- Commits that modify anything under `external/` or that hand-edit generated
  artifacts.
- A version bump without `Cargo.lock`, or a changelog update that doesn't
  sync all three surfaces.
- Divergence from upstream behavior without a registered ADR/deviation.
