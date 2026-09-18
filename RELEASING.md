# Releasing rpi

Release checklist. The order is pinned by ADR-0011 ("endpoint and Release synchronization constraint"): the version endpoint (`https://revpi.dev/api/latest-version`) and the GitHub Release have no automatic linkage, so they must be published in this exact sequence.

## Checklist

1. **Bump the workspace version** — `version` under `[workspace.package]` in `Cargo.toml` — and merge to `main`.
2. **Sync the changelog surfaces (stable releases)**: `changes/v<version>.md` is the single source of truth, but two downstream surfaces are compiled/published from it and must be synced in the same commit — (a) `CHANGELOG.md` at the repo root (embedded into the binary via `include_str!` in `core/changelog.rs`, driving `/changelog` and the new-version notice), and (b) the `revpi.dev` changelog page (`rpi-pages/changelog.html`, whose section for the version must match `changes/`). RC entries fold into `changes/v<stable>.md` at stable time — fold them into both downstream surfaces then.
3. **Tag the release**: `git tag v<version>` and push the tag. The tag triggers `.github/workflows/build.yml`, which builds all six targets and publishes the assets to the GitHub Release. The release notes are attached automatically from `changes/v<version>.md` (the changelog single source of truth), normalized by `scripts/unwrap-release-notes.py` — since the 2026-09-05 formatting convention the source files use single-line paragraphs, and the script stays as an idempotent reflow safeguard for any legacy or pasted hard-wrapped text (GitHub renders release bodies with hard line breaks) — make sure that file is final before tagging; a tag re-push refreshes both the assets and the notes.
4. **Wait for all six target assets + their `.sha256` sidecars** (12 files) to appear on the Release page, and verify before proceeding:

   | Target | Asset |
   |---|---|
   | `x86_64-pc-windows-msvc` | `rpi-<version>-x86_64-pc-windows-msvc.zip` |
   | `aarch64-apple-darwin` | `rpi-<version>-aarch64-apple-darwin.tar.gz` |
   | `x86_64-unknown-linux-gnu` | `rpi-<version>-x86_64-unknown-linux-gnu.tar.gz` |
   | `x86_64-unknown-linux-musl` | `rpi-<version>-x86_64-unknown-linux-musl.tar.gz` |
   | `aarch64-unknown-linux-musl` | `rpi-<version>-aarch64-unknown-linux-musl.tar.gz` |
   | `aarch64-unknown-linux-gnu` | `rpi-<version>-aarch64-unknown-linux-gnu.tar.gz` |

5. **Update the version endpoint** — only after step 4 is complete. In the `rpi-pages` repository, then commit and push (Git integration deploys):

   ```bash
   python3 scripts/generate-site.py --version <version>
   git add api/ && git commit -m "chore(api): latest-version -> v<version>" && git push
   ```

The official-site asset mirror (`https://revpi.dev/releases/download/...`) is a Pages Function that proxies GitHub — zero storage, nothing to upload; it works as soon as the Release assets exist. (`install.sh` / `install.ps1` themselves are synced to the site root by `generate-site.py`.)

## RC pre-releases (pre-release channel)

V14-19 (an rpi-native requirement with no upstream counterpart; design in the rpi-docs v0.1.4 requirements baseline §6). RC is a **release preview channel**: `rpi update --rc` / `rpi update --extensions --rc`; without `--rc` the channel is always stable.

### RC version-number rules (hard constraints)

- `<stable>-rc.<N>` (e.g. `0.1.5-rc.1`), tag `v0.1.5-rc.1`; the baseline must be the **candidate target version** (`0.1.5-rc.1` is a candidate for 0.1.5, not a suffix of 0.1.4);
- lowercase `rc`, dotted numeric fields (`rc.10 > rc.9`; without the dot, `rc10` sorts wrong under ASCII ordering); N starts at 1, increases monotonically, and is never reused;
- SemVer mandates apply: numeric identifiers must not have leading zeros (`rc.01` is invalid);
- the workspace version and the four lockstep extensions bump together (`version.workspace = true` propagates naturally).

### Release sequence (isomorphic to stable; the ordering constraint applies equally to the RC endpoint)

1. Bump the workspace version to `<stable>-rc.N` + write `changes/v<stable>-rc.N.md` (the release notes; at stable-release time its entries fold into `changes/v<stable>.md`), merge to `main`.
2. Tag `v<stable>-rc.N` and push — build.yml fires (the `v*` wildcard already covers it), producing the six-target assets + `.rpix` extension assets + `.sha256` sidecars; **the Release is automatically marked prerelease** (tag contains `-rc`), and `releases/latest` plus the install.sh fallback path stay stable.
3. Wait for the assets (same manifest as stable: 12 core files + extension assets).
4. **Only then** refresh the RC endpoint: in rpi-pages, `python3 scripts/generate-site.py --rc-version <stable>-rc.N` → commit + push. Clients probe `https://revpi.dev/api/latest-rc-version.json` (derived from the stable endpoint's directory).

### Interaction with stable releases

- **A stable release never touches the RC endpoint** (old RCs are judged not-newer by client semver, so there is no side effect);
- after a stable release ships, no further rc is appended to that baseline; rc users switch to stable by running `rpi update` (no flag) — the semver total order `0.1.5 > 0.1.5-rc.N` makes the same-baseline switch fall out naturally;
- pre-release builds probe the RC endpoint for the startup version check (the banner is marked pre-release and points to `rpi update --rc`); stable users are never bothered by RCs.

## Why the order matters

- **Endpoint ahead of the Release** (step 4 before step 3 finishes): clients probe a version whose assets are not uploaded yet — `rpi update --self` and `install.sh` hit download 404s.
- **Endpoint behind the Release** (step 4 skipped or delayed): clients never see the new version — the update banner never appears and self-update reports "already up to date" forever.

## First release

Before the first GitHub Release exists, `https://github.com/revpidev/rpi/releases/latest` is 404 and the install scripts have nothing to download. The script-based install instructions in the README go live together with the first Release.

## Security note

The `.sha256` sidecars are published next to the binaries and provide an **integrity check only** (corrupted downloads, mirror mix-ups). They do **not** protect against tampered release assets. Artifact signing (minisign / cosign) is a planned follow-up and will get its own ADR (see ADR-0011, security boundaries).
