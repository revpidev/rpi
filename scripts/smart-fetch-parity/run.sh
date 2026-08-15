#!/usr/bin/env bash
# TE06 smart-fetch parity harness: (re)generate golden fixtures from the
# PINNED upstream and replay them against the Rust port.
#
# Usage: scripts/smart-fetch-parity/run.sh [gen|test|all]
#   gen  — copy pinned sources to the staging tree, regenerate fixtures
#   test — cargo test -p rpi-ext-smart-fetch --test parity_fixtures
#   all  — both (default)
#
# External deps live in /tmp/rpi-smart-fetch-parity-deps (never in
# external/ — the submodule must stay pristine, gates.md G4). See README.md
# for the one-time setup.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
DEPS="${RPI_SF_DEPS:-/tmp/rpi-smart-fetch-parity-deps}"
STAGING="$DEPS"
UPSTREAM="$ROOT/external/agent-smart-fetch"
MODE="${1:-all}"

ensure_deps() {
  if [ ! -x "$DEPS/node_modules/.bin/tsx" ]; then
    echo "parity deps missing under $DEPS — see scripts/smart-fetch-parity/README.md" >&2
    exit 1
  fi
}

stage_sources() {
  # Copies (NOT symlinks): node realpaths symlinks and would resolve bare
  # imports back inside external/, where no node_modules exists.
  rm -rf "$STAGING/core-src" "$STAGING/pi-src"
  cp -r "$UPSTREAM/packages/core/src" "$STAGING/core-src"
  cp -r "$UPSTREAM/packages/pi-smart-fetch/src" "$STAGING/pi-src"
}

generate() {
  ensure_deps
  stage_sources
  "$DEPS/node_modules/.bin/tsx" "$ROOT/scripts/smart-fetch-parity/gen-fixtures.mjs"
}

replay() {
  (cd "$ROOT" && cargo test -p rpi-ext-smart-fetch --test parity_fixtures)
}

case "$MODE" in
  gen) generate ;;
  test) replay ;;
  all | *) generate && replay ;;
esac
