#!/usr/bin/env bash
# Extract the parity-harness source snapshots out-of-repo (TE13, design
# §3.6 / ADR-0025 §9; re-rotated by TE37 under ADR-0029 for v0.1.5, by
# TE45 for the v0.1.6 window under ADR-0034, by TE48 for the v0.1.7 window
# under ADR-0036, and by TE51 for the v0.1.8 window under ADR-0038).
#
# Two snapshots (both read-only against external/: `git archive` reads the
# object database only — no checkout, no `git worktree add` — so
# `git -C external/pi-subagents status --porcelain` stays empty and the
# submodule HEAD is untouched):
#
#   1. TARGET snapshot — the v0.1.8 target pin v0.83.0 @ 3a63e0ad (ADR-0038;
#      the gitlink only switches once TE51 lands, which rotates the harness).
#   2. REGRESSION snapshot — the retired v0.76.1 pin snapshot (7d072b91d;
#      the v0.1.7 zero-regression baseline). The live worktree cannot serve
#      as the upstream leg directly because the discovery chain (agents.ts)
#      imports the `yaml` package, which is unresolvable from a pristine
#      external/ (no node_modules may be written there) — hence the snapshot
#      with its own prod deps.
#      It also pins the model-resolution freeze face (v0.74 added the
#      `scoped` token and provider-prefixed id resolution; see
#      upstream-runner.mjs).
#
# The snapshots' own production deps are installed out-of-repo (npm install
# --omit=dev inside each snapshot dir); nothing is written under the
# repository.
#
# Usage:
#   bash scripts/subagents-parity/setup-target-source.sh [--skip-regression]
#
# Environment:
#   RPI_SUBAGENTS_TARGET_SRC     target snapshot dir (default /tmp/rpi-subagents-parity-target-v0830)
#   RPI_SUBAGENTS_TARGET_PIN     target commit (default 3a63e0ad..., v0.83.0)
#   RPI_SUBAGENTS_REGRESSION_SRC  regression snapshot dir (default /tmp/rpi-subagents-parity-regression-v0761)
#   RPI_SUBAGENTS_REGRESSION_PIN  regression commit (default 7d072b91d..., v0.76.1)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
SUBMODULE="$REPO/external/pi-subagents"
TARGET_PIN="${RPI_SUBAGENTS_TARGET_PIN:-3a63e0ad8898cf116ce671feabb0f8a7b31731a4}"
TARGET_DEST="${RPI_SUBAGENTS_TARGET_SRC:-/tmp/rpi-subagents-parity-target-v0830}"
REGRESSION_PIN_DEFAULT="7d072b91dca7e1aa915282f0159cec0175d65efa"
REGRESSION_DEST="${RPI_SUBAGENTS_REGRESSION_SRC:-/tmp/rpi-subagents-parity-regression-v0761}"

extract_snapshot() {
  local pin="$1" dest="$2" label="$3"
  if ! git -C "$SUBMODULE" cat-file -e "${pin}^{commit}" 2>/dev/null; then
    echo "$label pin $pin is not present in $SUBMODULE." >&2
    echo "Fetch the tag range read-only first (never a checkout):" >&2
    echo "  git -C external/pi-subagents fetch --deepen=350 origin" >&2
    exit 2
  fi
  rm -rf "$dest"
  mkdir -p "$dest"
  git -C "$SUBMODULE" archive --format=tar "$pin" | tar -x -C "$dest"
  if [ ! -d "$dest/node_modules" ]; then
    (cd "$dest" && npm install --omit=dev --ignore-scripts --no-audit --no-fund >/dev/null)
  fi
  echo "$label source ready: $dest @ $(git -C "$SUBMODULE" rev-parse --short "$pin")"
}

extract_snapshot "$TARGET_PIN" "$TARGET_DEST" "target"

if [ "${1:-}" != "--skip-regression" ]; then
  REGRESSION_PIN="${RPI_SUBAGENTS_REGRESSION_PIN:-$REGRESSION_PIN_DEFAULT}"
  extract_snapshot "$REGRESSION_PIN" "$REGRESSION_DEST" "regression"
fi
