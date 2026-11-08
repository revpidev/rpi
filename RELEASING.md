# Releasing rpi

> **Mandatory manual gate (ADR-0042, 2026-10-26):** every distribution packaging and publishing action must be initiated by a human, including `.rpix` dry-runs, release builds, tags, GitHub Releases, registry/site updates and issue closing. Agents may prepare files, commands and evidence but must not perform these actions autonomously. Ordinary development builds and tests are not distribution packaging. `.github/workflows/build.yml` now supports only `workflow_dispatch` distribution builds, so tag pushes never start packaging automatically; a human must start each build through the GitHub UI or `gh workflow run`. The remaining checklist describes the ordering constraints that stay in force; every trigger step is executed only by a human.

Release checklist. The order is pinned by ADR-0011 ("endpoint and Release synchronization constraint") and ADR-0042 (all actions manually initiated): the version endpoint (`https://revpi.dev/api/latest-version`) and the GitHub Release have no automatic linkage, so they must be published in this exact sequence.

**Manifest applicability:** V18-13 is still a pending implementation plan. Releases before the first Release that actually carries V18-13 use the existing 68-file manifest (12 core archive/sidecar files + 28 non-musl `.rpix` + 28 `.rpix.sha256`). The dynamic-musl/static-fallback/profile rules and 124-file manifest below are the **target state only for that V18-13-bearing Release** (planned for v0.1.8, or v0.1.9 if deferred); they are not a retroactive requirement for v0.1.8-rc.1 or other already published Releases.

## Checklist

1. **Bump the workspace version** — `version` under `[workspace.package]` in `Cargo.toml` — and merge to `main`.
2. **Sync the changelog surfaces (stable releases)**: `changes/v<version>.md` is the single source of truth, but two downstream surfaces are compiled/published from it and must be synced in the same commit — (a) `CHANGELOG.md` at the repo root (embedded into the binary via `include_str!` in `core/changelog.rs`, driving `/changelog` and the new-version notice), and (b) the `revpi.dev` changelog page (`rpi-pages/changelog.html`, whose section for the version must match `changes/`). RC entries fold into `changes/v<stable>.md` at stable time — fold them into both downstream surfaces then.
   Installer changes additionally require `sh scripts/test-install-sh.sh` and `pwsh -NoProfile -File scripts/test-install-ps1.ps1` to pass, and the site copies in `rpi-pages/install.sh` / `install.ps1` to be refreshed with `generate-site.py` in the same release commit.
3. **Tag and manually start the release build**: a human creates `git tag v<version>` and pushes it, then starts `.github/workflows/build.yml` through the GitHub UI or `gh workflow run build.yml --ref v<version>`; tag pushes never start packaging automatically. The manually started workflow builds all six targets and publishes the assets to the GitHub Release. Before V18-13, musl main assets remain static and the seven lockstep first-party extensions are published only for the four non-musl targets. **In the first V18-13-bearing Release**, musl main assets become dynamically linked (same file names; `-C target-feature=-crt-static`), each musl target gains a `rpi-<version>-<target>-static.tar.gz` fallback (binary only, no plugins), extensions cover all six targets, and standard/full profile archives are added for all six targets (see step 4). Dynamic and static musl builds must use separate staging/target directories and linkage assertions so the static rebuild cannot overwrite the dynamic input to the base/profile archives. The release notes are attached automatically from `changes/v<version>.md` (the changelog single source of truth), normalized by `scripts/unwrap-release-notes.py` — since the 2026-09-05 formatting convention the source files use single-line paragraphs, and the script stays as an idempotent reflow safeguard for any legacy or pasted hard-wrapped text (GitHub renders release bodies with hard line breaks) — make sure that file is final before tagging; a human-initiated tag re-push followed by another manual build refreshes both the assets and the notes. **After any tag re-push, a human must also re-run `generate-site.py` in rpi-pages**: the rebuild replaces asset bytes under the same URLs, and the extension index (`api/extensions/*.json`) pins per-asset sha256 anchors — stale anchors make every `rpi update --extensions` fail the integrity check (hit during the v0.1.3 re-tag and the v0.1.4 re-push; the 1h mirror cache TTL added in rpi-pages only bounds the proxy staleness, not the index anchors). For a V18-13-bearing Release this also regenerates `api/install-profiles/<version>.json`, which pins every standard/full bundle sha256; a stale bundle index deliberately makes profile installation fail closed; only an explicit mini installation remains available, not an integrity-failure fallback.
4. **Wait for all assets applicable to this release** to appear on the Release page, and verify before proceeding. Use the 68-file pre-V18-13 manifest described above until V18-13 has actually landed. The **V18-13 target manifest** is **20 archives + 20 `.sha256` + 42 `.rpix` + 42 `.rpix.sha256` = 124 files**:

   | Kind | Assets |
   |---|---|
   | Base (6) | `rpi-<version>-<target>.tar.gz` / `.zip` for all six targets; **musl entries are dynamic only in the V18-13 target manifest** (names unchanged) |
   | Profile (12) | `rpi-<version>-<target>-standard.tar.gz` / `.zip` and `rpi-<version>-<target>-full.tar.gz` / `.zip` for all six targets (Windows uses `.zip`; musl profiles are dynamic) |
   | Static fallback (2, musl only) | `rpi-<version>-<target>-static.tar.gz` — binary only, no plugins |
   | Plugins (42) | seven first-party `.rpix` × six targets (two musl dynamic targets included) |

   Every archive above has a matching `.sha256`; every `.rpix` has a matching `.rpix.sha256`. The `-static` fallback is intentionally not produced for non-musl targets and its registry label (`<target>-static`) carries no plugin artifacts.

   | Target | Base asset |
   |---|---|
   | `x86_64-pc-windows-msvc` | `rpi-<version>-x86_64-pc-windows-msvc.zip` |
   | `aarch64-apple-darwin` | `rpi-<version>-aarch64-apple-darwin.tar.gz` |
   | `x86_64-unknown-linux-gnu` | `rpi-<version>-x86_64-unknown-linux-gnu.tar.gz` |
   | `x86_64-unknown-linux-musl` | `rpi-<version>-x86_64-unknown-linux-musl.tar.gz` (dynamic in the V18-13 target manifest) |
   | `aarch64-unknown-linux-musl` | `rpi-<version>-aarch64-unknown-linux-musl.tar.gz` (dynamic in the V18-13 target manifest) |
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
- the workspace version and the lockstep extensions bump together (`version.workspace = true` propagates naturally).

### Release sequence (isomorphic to stable; the ordering constraint applies equally to the RC endpoint)

1. Bump the workspace version to `<stable>-rc.N` + write `changes/v<stable>-rc.N.md` (the release notes; at stable-release time its entries fold into `changes/v<stable>.md`), merge to `main`.
2. A human tags `v<stable>-rc.N` and pushes it, then manually starts build.yml through the GitHub UI or `gh workflow run build.yml --ref v<stable>-rc.N`; tag pushes never start packaging automatically. The workflow produces the manifest applicable to that tag: the existing 68-file manifest before V18-13, or the 124-file target manifest only after V18-13 has landed (six base targets including dynamic musl, two musl `-static` fallbacks, standard/full profile archives, seven `.rpix` per target for all six targets, plus `.sha256` sidecars); **the Release is automatically marked prerelease** (tag contains `-rc`), and `releases/latest` plus the install.sh fallback path stay stable.
3. Wait for the assets using the same applicability rule as stable: 68 files before V18-13, or 20 archives + 20 sidecars + 7 × 6 extension `.rpix` assets and their sidecars = 124 files for a V18-13-bearing Release.
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

The `.sha256` sidecars are published next to the binaries and provide an **integrity check only** (corrupted downloads, mirror mix-ups). They do **not** protect against tampered release assets. Standard/full bundles additionally require the independent rpi-pages profile-index hash; their Release sidecar alone must never authorize plugin installation. Artifact signing (minisign / cosign) is a planned follow-up and will get its own ADR (see ADR-0011, security boundaries).
