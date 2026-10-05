#!/usr/bin/env bash
# Extract the target-pin pi-mcp-adapter source into an out-of-repo snapshot
# and install its own lockfile closure (TE13 target-track skeleton,
# ADR-0025 §9; re-pinned to v2.34.0+9 by TE37 under ADR-0029 (v0.1.5) and to
# v4.0.0 by TE46 under ADR-0034 for the v0.1.6 rebase window). Read-only
# against external/: `git archive` reads the
# object database only (no checkout, no worktree), so
# `git -C external/pi-mcp-adapter status --porcelain` stays empty and the
# submodule HEAD is untouched. After the TE46 pin switch the submodule HEAD
# IS the target pin (v4.0.0), so the snapshot is a stable out-of-tree copy of
# the same target the default driver reads.
#
# The snapshot doubles as the out-of-tree dependency root: the parity hooks
# (parity-hooks.mjs) resolve bare specifiers against
# `$RPI_MCP_PARITY_DEPS/package.json`, so point both env vars at it:
#
#   RPI_MCP_PARITY_UPSTREAM=/tmp/rpi-mcp-parity-target-v400
#   RPI_MCP_PARITY_DEPS=/tmp/rpi-mcp-parity-target-v400
#
# Usage:
#   bash scripts/mcp-parity/setup-target-source.sh
#
# Environment:
#   RPI_MCP_TARGET_SRC  snapshot/deps dir (default /tmp/rpi-mcp-parity-target-v400)
#   RPI_MCP_TARGET_PIN  target commit (default 5884ac4e..., v4.0.0)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
PIN="${RPI_MCP_TARGET_PIN:-5884ac4e45f5834f51ab8914f61b6a36a6a0a51b}"
DEST="${RPI_MCP_TARGET_SRC:-/tmp/rpi-mcp-parity-target-v400}"
SUBMODULE="$REPO/external/pi-mcp-adapter"

if ! git -C "$SUBMODULE" cat-file -e "$PIN^{commit}" 2>/dev/null; then
  echo "target pin $PIN is not present in $SUBMODULE." >&2
  echo "Fetch the tag range read-only first (never a checkout):" >&2
  echo "  git -C external/pi-mcp-adapter fetch --deepen=250 origin" >&2
  exit 2
fi

rm -rf "$DEST"
mkdir -p "$DEST"
git -C "$SUBMODULE" archive --format=tar "$PIN" | tar -x -C "$DEST"

if [ ! -d "$DEST/node_modules" ]; then
  if [ -f "$DEST/package-lock.json" ]; then
    (cd "$DEST" && npm ci --no-audit --no-fund >/dev/null)
  else
    (cd "$DEST" && npm install --ignore-scripts --no-audit --no-fund >/dev/null)
  fi
fi

echo "target source ready: $DEST @ $(git -C "$SUBMODULE" rev-parse --short "$PIN")"
echo "export RPI_MCP_PARITY_UPSTREAM=$DEST"
echo "export RPI_MCP_PARITY_DEPS=$DEST"
