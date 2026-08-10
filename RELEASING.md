# Releasing rpi

Release checklist. The order is pinned by ADR-0011 ("端点与 Release 同步约束"):
the version endpoint (`https://revpi.dev/api/latest-version`) and the GitHub
Release have no automatic linkage, so they must be published in this exact
sequence.

## Checklist

1. **Bump the workspace version** — `version` under `[workspace.package]` in
   `Cargo.toml` — and merge to `main`.
2. **Tag the release**: `git tag v<version>` and push the tag. The tag
   triggers `.github/workflows/build.yml`, which builds all six targets and
   publishes the assets to the GitHub Release.
3. **Wait for all six target assets + their `.sha256` sidecars** (12 files) to
   appear on the Release page, and verify before proceeding:

   | Target | Asset |
   |---|---|
   | `x86_64-pc-windows-msvc` | `rpi-<version>-x86_64-pc-windows-msvc.zip` |
   | `aarch64-apple-darwin` | `rpi-<version>-aarch64-apple-darwin.tar.gz` |
   | `x86_64-unknown-linux-gnu` | `rpi-<version>-x86_64-unknown-linux-gnu.tar.gz` |
   | `x86_64-unknown-linux-musl` | `rpi-<version>-x86_64-unknown-linux-musl.tar.gz` |
   | `aarch64-unknown-linux-musl` | `rpi-<version>-aarch64-unknown-linux-musl.tar.gz` |
   | `aarch64-unknown-linux-gnu` | `rpi-<version>-aarch64-unknown-linux-gnu.tar.gz` |

4. **Update the version endpoint** — only after step 3 is complete. In the
   `rpi-pages` repository, then commit and push (Git integration deploys):

   ```bash
   python3 scripts/generate-site.py --version <version>
   git add api/ && git commit -m "chore(api): latest-version -> v<version>" && git push
   ```

   The official-site asset mirror (`https://revpi.dev/releases/download/...`)
   is a Pages Function that proxies GitHub — zero storage, nothing to upload;
   it works as soon as the Release assets exist. (`install.sh` / `install.ps1`
   themselves are synced to the site root by `generate-site.py`.)

## Why the order matters

- **Endpoint ahead of the Release** (step 4 before step 3 finishes): clients
  probe a version whose assets are not uploaded yet — `rpi update --self` and
  `install.sh` hit download 404s.
- **Endpoint behind the Release** (step 4 skipped or delayed): clients never
  see the new version — the update banner never appears and self-update
  reports "already up to date" forever.

## First release

Before the first GitHub Release exists,
`https://github.com/revpidev/rpi/releases/latest` is 404 and the install
scripts have nothing to download. The script-based install instructions in the
README go live together with the first Release.

## Security note

The `.sha256` sidecars are published next to the binaries and provide an
**integrity check only** (corrupted downloads, mirror mix-ups). They do **not**
protect against tampered release assets. Artifact signing (minisign / cosign)
is a planned follow-up and will get its own ADR (see ADR-0011, security
boundaries).
