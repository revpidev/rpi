#!/usr/bin/env bash
# Out-of-tree dependency install for the MCP cross-implementation parity
# harness (scripts/mcp-parity). Nothing is written into rpi/external/ — the
# pinned submodules stay pristine (G4 red line).
#
# The upstream package.json + package-lock.json (external/pi-mcp-adapter @
# 3d953f90) are copied verbatim into the deps dir and installed with
# `npm ci`, so the FULL transitive closure is exactly the one upstream
# pinned and tested against — including tsx (upstream modules use
# non-erasable TypeScript syntax: constructor parameter properties) and the
# official conformance referee (@modelcontextprotocol/conformance, used by
# run-parity-suite.sh's conformance leg).
#
# Usage: bash scripts/mcp-parity/setup-deps.sh [deps-dir]
# Default deps-dir: /tmp/rpi-mcp-parity-deps
# Idempotent: re-running reinstalls from the same lockfile (npm ci wipes
# node_modules first).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/../.." && pwd)"
UPSTREAM="$REPO/external/pi-mcp-adapter"

DEPS_DIR="${1:-/tmp/rpi-mcp-parity-deps}"

for f in package.json package-lock.json; do
  if [[ ! -f "$UPSTREAM/$f" ]]; then
    echo "setup-deps.sh: missing $UPSTREAM/$f — the pi-mcp-adapter submodule must be checked out (git submodule update --init)" >&2
    exit 1
  fi
done

mkdir -p "$DEPS_DIR"
cp "$UPSTREAM/package.json" "$UPSTREAM/package-lock.json" "$DEPS_DIR/"
cd "$DEPS_DIR"

# npm ci fails loudly when the lockfile is out of sync with package.json —
# no silent fallback to floating resolution.
npm ci --no-audit --no-fund

echo "deps ready: $DEPS_DIR (upstream lockfile closure, npm ci)"
