#!/usr/bin/env bash
# One-command rerun of the built-in MCP parity evidence chain (issue #11):
# leg run → archival under rpi/fixtures/generated/mcp-parity/ (checked into
# git as the evidence chain).
#
# Leg (see scripts/mcp-parity/README.md):
#   builtin-mcp-parity  built-in rpi-mcp vs upstream packages/mcp (V16-08 G3)
#
# The five adapter legs (mcp-parity / render-call-parity / oauth-parity /
# conformance / e2e-parity) were removed together with rpi-ext-mcp-adapter
# (ADR-0037, TE49) — the adapter was their upstream/rpi driver source.
# `builtin-mcp-parity` (and the shared fixture-server.mjs) is the only
# remaining leg.
#
# Usage:
#   bash scripts/mcp-parity/run-parity-suite.sh [leg ...]     # default: all
#
# Exit code: 0 iff every selected leg passed.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

ALL_LEGS=(builtin-mcp-parity)
if [[ $# -gt 0 ]]; then
  LEGS=("$@")
else
  LEGS=("${ALL_LEGS[@]}")
fi
declare -A LEG_STATUS

log() { printf '\n==> %s\n' "$*"; }

# --- builtin-mcp-parity (V16-08 G3: built-in rpi-mcp vs upstream packages/mcp) ---
if [[ " ${LEGS[*]} " == *" builtin-mcp-parity "* ]]; then
  log "leg builtin-mcp-parity"
  if node "$SCRIPT_DIR/run-builtin-mcp-parity.mjs"; then
    LEG_STATUS[builtin-mcp-parity]=PASS
  else
    LEG_STATUS[builtin-mcp-parity]=FAIL
  fi
fi

# --- verdict -----------------------------------------------------------------
log "summary"
overall=0
for leg in "${ALL_LEGS[@]}"; do
  [[ " ${LEGS[*]} " != *" $leg "* ]] && continue
  status="${LEG_STATUS[$leg]:-SKIPPED}"
  printf '%-14s %s\n' "$leg" "$status"
  [[ $status != PASS ]] && overall=1
done
exit "$overall"