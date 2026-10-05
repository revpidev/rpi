#!/usr/bin/env bash
# Extract the parity-harness source snapshots out-of-repo (TE13, design
# §3.6 / ADR-0025 §9; re-rotated by TE37 under ADR-0029 for v0.1.5 and by
# TE45 for the v0.1.6 window under ADR-0034).
#
# Two snapshots (both read-only against external/: `git archive` reads the
# object database only — no checkout, no `git worktree add` — so
# `git -C external/pi-subagents status --porcelain` stays empty and the
# submodule HEAD is untouched):
#
#   1. TARGET snapshot — the current pin v0.74.0 @ b6bda32f (ADR-0034;
#      switched with M0.75 on 2026-10-01, TE45 rotates the harness).
#   2. REGRESSION snapshot — the retired v0.70.0 pin snapshot (b72714de;
#      the v0.1.5 zero-regression baseline). The live worktree cannot serve
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
#   RPI_SUBAGENTS_TARGET_SRC     target snapshot dir (default /tmp/rpi-subagents-parity-target-v074)
#   RPI_SUBAGENTS_TARGET_PIN     target commit (default b6bda32f..., v0.74.0)
#   RPI_SUBAGENTS_REGRESSION_SRC  regression snapshot dir (default /tmp/rpi-subagents-parity-regression-v070)
#   RPI_SUBAGENTS_REGRESSION_PIN  regression commit (default b72714de..., v0.70.0)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
SUBMODULE="$REPO/external/pi-subagents"
TARGET_PIN="${RPI_SUBAGENTS_TARGET_PIN:-b6bda32f03b7f549623bc404c9be14dca298ddc4}"
TARGET_DEST="${RPI_SUBAGENTS_TARGET_SRC:-/tmp/rpi-subagents-parity-target-v074}"
REGRESSION_PIN_DEFAULT="b72714de95e612406b3461e63dfc182856333a7e"
REGRESSION_DEST="${RPI_SUBAGENTS_REGRESSION_SRC:-/tmp/rpi-subagents-parity-regression-v070}"

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
