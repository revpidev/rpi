#!/usr/bin/env bash
# Extract the target-pin pi-subagents source into an out-of-repo snapshot for
# the target-track upstream leg (TE13, design §3.6 / ADR-0025 §9).
#
# Read-only against external/: `git archive` reads the object database only
# (no checkout, no `git worktree add`), so
# `git -C external/pi-subagents status --porcelain` stays empty and the
# submodule HEAD keeps the old pin until TE27. Nothing is written under the
# repository; the snapshot and its npm deps live under /tmp.
#
# The snapshot's own production deps are installed out-of-repo because
# v0.66 `src/shared/utils.ts` -> `formatters.ts` -> `settings.ts` ->
# `agents/agents.ts` imports the `yaml` package at runtime (v0.48 stopped
# before agents.ts, which is why the regression track needs no deps).
#
# Usage:
#   bash scripts/subagents-parity/setup-target-source.sh
#
# Environment:
#   RPI_SUBAGENTS_TARGET_SRC  snapshot dir (default /tmp/rpi-subagents-parity-target-v066)
#   RPI_SUBAGENTS_TARGET_PIN  target commit (default 0fc0eebb..., v0.66.0)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
PIN="${RPI_SUBAGENTS_TARGET_PIN:-0fc0eebb9604970c506708b7508d6aa38921fde2}"
DEST="${RPI_SUBAGENTS_TARGET_SRC:-/tmp/rpi-subagents-parity-target-v066}"
SUBMODULE="$REPO/external/pi-subagents"

if ! git -C "$SUBMODULE" cat-file -e "$PIN^{commit}" 2>/dev/null; then
  echo "target pin $PIN is not present in $SUBMODULE." >&2
  echo "Fetch the tag range read-only first (never a checkout):" >&2
  echo "  git -C external/pi-subagents fetch --deepen=700 origin" >&2
  exit 2
fi

rm -rf "$DEST"
mkdir -p "$DEST"
git -C "$SUBMODULE" archive --format=tar "$PIN" | tar -x -C "$DEST"

if [ ! -d "$DEST/node_modules" ]; then
  (cd "$DEST" && npm install --omit=dev --ignore-scripts --no-audit --no-fund >/dev/null)
fi

echo "target source ready: $DEST @ $(git -C "$SUBMODULE" rev-parse --short "$PIN")"
