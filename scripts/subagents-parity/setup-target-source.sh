#!/usr/bin/env bash
# Extract the parity-harness source snapshots out-of-repo (TE13, design
# §3.6 / ADR-0025 §9; re-rotated by TE37 under ADR-0029 for the v0.1.5
# window).
#
# Two snapshots (both read-only against external/: `git archive` reads the
# object database only — no checkout, no `git worktree add` — so
# `git -C external/pi-subagents status --porcelain` stays empty and the
# submodule HEAD is untouched):
#
#   1. TARGET snapshot — the current pin v0.70.0 @ b72714de (ADR-0029;
#      switched with TE39 on 2026-09-27).
#   2. REGRESSION snapshot — the retired v0.66.0 pin snapshot (0fc0eebb;
#      archaeology only — its zero-regression mission ended with TE39). The live
#      worktree cannot serve as the upstream leg directly because the
#      v0.66 discovery chain (agents.ts) imports the `yaml` package, which
#      is unresolvable from a pristine external/ (no node_modules may be
#      written there) — hence the snapshot with its own prod deps.
#      It also pins the fallback/model freeze face (v0.70 deleted
#      model-fallback.ts, #2270; see upstream-runner.mjs).
#
# The snapshots' own production deps are installed out-of-repo (npm install
# --omit=dev inside each snapshot dir); nothing is written under the
# repository.
#
# Usage:
#   bash scripts/subagents-parity/setup-target-source.sh [--skip-regression]
#
# Environment:
#   RPI_SUBAGENTS_TARGET_SRC     target snapshot dir (default /tmp/rpi-subagents-parity-target-v070)
#   RPI_SUBAGENTS_TARGET_PIN     target commit (default b72714de..., v0.70.0)
#   RPI_SUBAGENTS_REGRESSION_SRC  regression snapshot dir (default /tmp/rpi-subagents-parity-regression-v066)
#   RPI_SUBAGENTS_REGRESSION_PIN  regression commit (default = the submodule HEAD, i.e. the current pin)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
SUBMODULE="$REPO/external/pi-subagents"
TARGET_PIN="${RPI_SUBAGENTS_TARGET_PIN:-b72714de95e612406b3461e63dfc182856333a7e}"
TARGET_DEST="${RPI_SUBAGENTS_TARGET_SRC:-/tmp/rpi-subagents-parity-target-v070}"
REGRESSION_DEST="${RPI_SUBAGENTS_REGRESSION_SRC:-/tmp/rpi-subagents-parity-regression-v066}"

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
  REGRESSION_PIN="${RPI_SUBAGENTS_REGRESSION_PIN:-$(git -C "$SUBMODULE" rev-parse HEAD)}"
  extract_snapshot "$REGRESSION_PIN" "$REGRESSION_DEST" "regression"
fi
