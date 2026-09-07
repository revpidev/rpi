# Releasing rpi

Release checklist. The order is pinned by ADR-0011 ("端点与 Release 同步约束"): the version endpoint (`https://revpi.dev/api/latest-version`) and the GitHub Release have no automatic linkage, so they must be published in this exact sequence.

## Checklist

1. **Bump the workspace version** — `version` under `[workspace.package]` in `Cargo.toml` — and merge to `main`.
2. **Tag the release**: `git tag v<version>` and push the tag. The tag triggers `.github/workflows/build.yml`, which builds all six targets and publishes the assets to the GitHub Release. The release notes are attached automatically from `changes/v<version>.md` (the changelog single source of truth), normalized by `scripts/unwrap-release-notes.py` — since the 2026-09-05 formatting convention the source files use single-line paragraphs, and the script stays as an idempotent reflow safeguard for any legacy or pasted hard-wrapped text (GitHub renders release bodies with hard line breaks) — make sure that file is final before tagging; a tag re-push refreshes both the assets and the notes.
3. **Wait for all six target assets + their `.sha256` sidecars** (12 files) to appear on the Release page, and verify before proceeding:

   | Target | Asset |
   |---|---|
   | `x86_64-pc-windows-msvc` | `rpi-<version>-x86_64-pc-windows-msvc.zip` |
   | `aarch64-apple-darwin` | `rpi-<version>-aarch64-apple-darwin.tar.gz` |
   | `x86_64-unknown-linux-gnu` | `rpi-<version>-x86_64-unknown-linux-gnu.tar.gz` |
   | `x86_64-unknown-linux-musl` | `rpi-<version>-x86_64-unknown-linux-musl.tar.gz` |
   | `aarch64-unknown-linux-musl` | `rpi-<version>-aarch64-unknown-linux-musl.tar.gz` |
   | `aarch64-unknown-linux-gnu` | `rpi-<version>-aarch64-unknown-linux-gnu.tar.gz` |

4. **Update the version endpoint** — only after step 3 is complete. In the `rpi-pages` repository, then commit and push (Git integration deploys):

   ```bash
   python3 scripts/generate-site.py --version <version>
   git add api/ && git commit -m "chore(api): latest-version -> v<version>" && git push
   ```

The official-site asset mirror (`https://revpi.dev/releases/download/...`) is a Pages Function that proxies GitHub — zero storage, nothing to upload; it works as soon as the Release assets exist. (`install.sh` / `install.ps1` themselves are synced to the site root by `generate-site.py`.)

## RC 预发布（pre-release channel）

V14-19（rpi 自有需求，无上游对照；设计见 rpi-docs v0.1.4 需求基线 §6）。RC 是**发布预览通道**：`rpi update --rc` / `rpi update --extensions --rc`，不带 `--rc` 恒为 stable。

### RC 版本号规范（硬约束）

- `<stable>-rc.<N>`（如 `0.1.5-rc.1`），tag `v0.1.5-rc.1`；基线必须是**候选目标版本**（`0.1.5-rc.1` 是 0.1.5 的候选，不是 0.1.4 的后缀）；
- 小写 `rc`、点分数字段（`rc.10 > rc.9`；无点 `rc10` 会按 ASCII 序排错）；N 从 1 起单调递增不复用；
- SemVer 强制项适用：数字标识符禁前导零（`rc.01` 非法）；
- workspace 版本与四款 lockstep 扩展同 bump（`version.workspace = true` 自然传播）。

### 发版顺序（与 stable 同构；顺序约束对 RC 端点同样成立）

1. workspace 版本 bump `<stable>-rc.N` + `changes/v<stable>-rc.N.md`（发布说明；正式版发布时条目并入 `changes/v<stable>.md`），合入 `main`。
2. 打 tag `v<stable>-rc.N` 并推送——build.yml 触发（`v*` 通配已覆盖），六目标资产 + `.rpix` 扩展资产 + `.sha256` sidecar 自动产出；**Release 自动标记 prerelease**（tag 含 `-rc`），`releases/latest` 与 install.sh 回退路径保持 stable。
3. 等资产齐（同 stable 清单：12 个本体文件 + 扩展资产）。
4. **才**刷新 RC 端点：rpi-pages `python3 scripts/generate-site.py --rc-version <stable>-rc.N` → commit + push。客户端探测 `https://revpi.dev/api/latest-rc-version.json`（由 stable 端点同目录推导）。

### 与 stable 发布的交互

- **stable 发布不触碰 RC 端点**（旧 RC 会被客户端 semver 判为不新，无副作用）；
- 正式版发布后该基线不再追加 rc；rc 用户切 stable：直接 `rpi update`（无旗标）——semver 全序 `0.1.5 > 0.1.5-rc.N` 使同基线切换自然成立；
- 预发布构建的启动版本检查自动改探 RC 端点（横幅标注 pre-release，指引 `rpi update --rc`）；stable 用户不会被 RC 打扰。

## Why the order matters

- **Endpoint ahead of the Release** (step 4 before step 3 finishes): clients probe a version whose assets are not uploaded yet — `rpi update --self` and `install.sh` hit download 404s.
- **Endpoint behind the Release** (step 4 skipped or delayed): clients never see the new version — the update banner never appears and self-update reports "already up to date" forever.

## First release

Before the first GitHub Release exists, `https://github.com/revpidev/rpi/releases/latest` is 404 and the install scripts have nothing to download. The script-based install instructions in the README go live together with the first Release.

## Security note

The `.sha256` sidecars are published next to the binaries and provide an **integrity check only** (corrupted downloads, mirror mix-ups). They do **not** protect against tampered release assets. Artifact signing (minisign / cosign) is a planned follow-up and will get its own ADR (see ADR-0011, security boundaries).
