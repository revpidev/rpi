#!/usr/bin/env bash
# TE06 smart-fetch parity harness: (re)generate golden fixtures from the
# pinned upstream and replay them against the Rust port.
#
# V19-03 dual track (ADR-0048):
#   regression (default) — v0.3.17 b0111612, fixtures/smart-fetch-parity
#   target               — v0.3.23 feb6da00, fixtures/smart-fetch-parity-v0323
#
# Usage: scripts/smart-fetch-parity/run.sh [gen|test|all] [--track=target|regression]
#   gen  — extract the track snapshot, stage sources, regenerate fixtures
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
TRACK="${RPI_SF_TRACK:-regression}"
case "$TRACK" in
  target) PIN="feb6da001161021df6c2de550f225ed5306a671f"; OUT_NAME="smart-fetch-parity-v0323" ;;
  regression) PIN="b01116124971de44f16a4477e34c06ba2ab1d0bf"; OUT_NAME="smart-fetch-parity" ;;
  *) echo "unknown track: $TRACK" >&2; exit 2 ;;
esac
SNAPSHOT="${RPI_SF_SNAPSHOT:-/tmp/rpi-smart-fetch-parity-$TRACK}"

ensure_deps() {
  if [ ! -x "$DEPS/node_modules/.bin/tsx" ]; then
    echo "parity deps missing under $DEPS — see scripts/smart-fetch-parity/README.md" >&2
    exit 1
  fi
}

stage_sources() {
  # Copies (NOT symlinks): node realpaths symlinks and would resolve bare
  # imports back inside external/, where no node_modules exists. The snapshot
  # is extracted with git archive (read-only against external/).
  rm -rf "$SNAPSHOT"
  mkdir -p "$SNAPSHOT"
  git -C "$UPSTREAM" archive --format=tar "$PIN" | tar -x -C "$SNAPSHOT"
  rm -rf "$STAGING/core-src" "$STAGING/upstream-src"
  cp -r "$SNAPSHOT/packages/core/src" "$STAGING/core-src"
  cp -r "$SNAPSHOT/packages/pi-smart-fetch/src" "$STAGING/upstream-src"
}

generate() {
  ensure_deps
  stage_sources
  RPI_SF_OUT_DIR="$ROOT/fixtures/generated/$OUT_NAME" \
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
