#!/usr/bin/env bash
# One-command rerun of the MCP parity evidence chain (issue #11): dependency
# install → four legs → archival under rpi/fixtures/generated/mcp-parity/
# (checked into git as the evidence chain).
#
# Legs (see scripts/mcp-parity/README.md):
#   mcp-parity     frame/result parity, upstream Node vs rpi Rust client
#   oauth-parity   OAuth authorization-code + PKCE flow parity (stub AS)
#   conformance    official @modelcontextprotocol/conformance referee
#                  driving the crate example conformance_driver
#   e2e-parity     pi -p vs rpi -p five-mode tool-result parity
#                  (needs a working model provider login for BOTH CLIs)
#   render-call-parity  renderCall pure-function byte parity (TE09 FR-E)
#
# Usage:
#   bash scripts/mcp-parity/run-parity-suite.sh [leg ...]     # default: all
#
# Artifacts are written to STABLE per-leg names (conformance/<scenario>/
# instead of the referee's <scenario>-<timestamp>/), with run-volatile
# values normalized (see normalize-conformance.mjs), so a clean rerun is a
# no-op diff in git.
#
# Exit code: 0 iff every selected leg passed.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/../.." && pwd)"
OUT_DIR="$REPO/fixtures/generated/mcp-parity"
DEPS_DIR="${RPI_MCP_PARITY_DEPS:-/tmp/rpi-mcp-parity-deps}"
CARGO="${RPI_MCP_PARITY_CARGO:-cargo}"
REFREE_BIN="$DEPS_DIR/node_modules/@modelcontextprotocol/conformance/dist/index.js"
CONFORMANCE_SCENARIOS=(
  initialize
  tools_call
  sse-retry
  elicitation-sep1034-client-defaults
)

ALL_LEGS=(mcp-parity render-call-parity oauth-parity conformance e2e-parity)
if [[ $# -gt 0 ]]; then
  LEGS=("$@")
else
  LEGS=("${ALL_LEGS[@]}")
fi
declare -A LEG_STATUS

log() { printf '\n==> %s\n' "$*"; }

is_baselined() {
  grep -Fqx "  - $1" "$SCRIPT_DIR/conformance-baseline.yml"
}

# --- dependency install (idempotent; skips when the closure is in place) ---
need_setup=0
if [[ ! -d "$DEPS_DIR/node_modules" ]] \
  || [[ ! -f "$DEPS_DIR/node_modules/tsx/dist/loader.mjs" ]] \
  || [[ ! -f "$REFREE_BIN" ]]; then
  need_setup=1
fi
if [[ ${RPI_PARITY_SUITE_SKIP_SETUP:-0} != 1 && $need_setup -eq 1 ]]; then
  log "installing out-of-tree deps (npm ci from upstream lockfile) into $DEPS_DIR"
  if ! bash "$SCRIPT_DIR/setup-deps.sh" "$DEPS_DIR"; then
    echo "run-parity-suite.sh: setup-deps.sh failed" >&2
    exit 1
  fi
elif [[ $need_setup -eq 1 ]]; then
  echo "run-parity-suite.sh: deps missing under $DEPS_DIR and RPI_PARITY_SUITE_SKIP_SETUP=1" >&2
  exit 1
fi
export RPI_MCP_PARITY_DEPS="$DEPS_DIR"

# --- mcp-parity --------------------------------------------------------------
if [[ " ${LEGS[*]} " == *" mcp-parity "* ]]; then
  log "leg mcp-parity"
  if node "$SCRIPT_DIR/run-mcp-parity.mjs"; then
    LEG_STATUS[mcp-parity]=PASS
  else
    LEG_STATUS[mcp-parity]=FAIL
  fi
fi

# --- render-call-parity (TE09 FR-E) ------------------------------------------
if [[ " ${LEGS[*]} " == *" render-call-parity "* ]]; then
  log "leg render-call-parity"
  if node "$SCRIPT_DIR/run-render-call-parity.mjs"; then
    LEG_STATUS[render-call-parity]=PASS
  else
    LEG_STATUS[render-call-parity]=FAIL
  fi
fi

# --- oauth-parity ------------------------------------------------------------
if [[ " ${LEGS[*]} " == *" oauth-parity "* ]]; then
  log "leg oauth-parity"
  if node "$SCRIPT_DIR/run-oauth-parity.mjs"; then
    LEG_STATUS[oauth-parity]=PASS
  else
    LEG_STATUS[oauth-parity]=FAIL
  fi
fi

# --- conformance (official referee → conformance_driver) --------------------
if [[ " ${LEGS[*]} " == *" conformance "* ]]; then
  log "leg conformance (build driver example)"
  if ! "$CARGO" build -p rpi-ext-mcp-adapter --example conformance_driver; then
    echo "run-parity-suite.sh: cargo build of conformance_driver failed" >&2
    LEG_STATUS[conformance]=FAIL
  else
    DRIVER="$REPO/target/debug/examples/conformance_driver"
    STAGE="$(mktemp -d "${TMPDIR:-/tmp}/rpi-conformance-stage.XXXXXX")"
    SUMMARY="$OUT_DIR/conformance/driver-summary.txt"
    mkdir -p "$OUT_DIR/conformance"
    : >"$SUMMARY"
    failed=0
    for scenario in "${CONFORMANCE_SCENARIOS[@]}"; do
      log "conformance scenario: $scenario"
      scenario_log="$STAGE/referee.log"
      if node "$REFREE_BIN" client \
        --command "$DRIVER" \
        --scenario "$scenario" \
        --expected-failures "$SCRIPT_DIR/conformance-baseline.yml" \
        --timeout 90000 \
        --output-dir "$STAGE" >"$scenario_log" 2>&1; then
        referee_rc=0
      else
        referee_rc=1
      fi
      verdict=PASS
      wire_failed=0
      result_dir="$(find "$STAGE" -maxdepth 1 -type d -name "$scenario-*" | head -n1)"
      if [[ -n "$result_dir" && -f "$result_dir/checks.json" ]] \
        && grep -q '"status": "FAILURE"' "$result_dir/checks.json"; then
        wire_failed=1
      fi
      if [[ -z "$result_dir" ]]; then
        echo "run-parity-suite.sh: no referee output dir for $scenario" >&2
        verdict=FAIL
        failed=1
      elif grep -q "Client timed out after" "$scenario_log" \
        || { grep -q "Client exited with code" "$scenario_log" && ! is_baselined "$scenario"; } \
        || { [[ $referee_rc -ne 0 ]] && ! is_baselined "$scenario"; } \
        || { [[ $wire_failed -eq 1 ]] && ! is_baselined "$scenario"; }; then
        verdict=FAIL
        failed=1
        tail -40 "$scenario_log"
      elif [[ $referee_rc -ne 0 ]] || grep -q "Client exited with code" "$scenario_log" || [[ $wire_failed -eq 1 ]]; then
        verdict="FAIL (expected: server->client elicitation is P2 scope; P0 answers -32601)"
      fi
      echo "$scenario $verdict" >>"$SUMMARY"

      # Archive under the stable per-scenario directory, normalizing
      # run-volatile values (timestamps/ephemeral ports/session ids/retry
      # jitter) so reruns do not churn the archived evidence.
      if [[ -n "$result_dir" ]]; then
        target="$OUT_DIR/conformance/$scenario"
        rm -rf "$target"
        mkdir -p "$target"
        for f in checks.json stdout.txt stderr.txt; do
          [[ -f "$result_dir/$f" ]] || continue
          node "$SCRIPT_DIR/normalize-conformance.mjs" "$result_dir/$f" "$target/$f"
        done
      fi
    done
    rm -rf "$STAGE"
    LEG_STATUS[conformance]=$( [[ $failed -eq 0 ]] && echo PASS || echo FAIL )
  fi
fi

# --- e2e-parity --------------------------------------------------------------
if [[ " ${LEGS[*]} " == *" e2e-parity "* ]]; then
  log "leg e2e-parity (needs model provider auth for both CLIs)"
  if node "$SCRIPT_DIR/run-e2e-parity.mjs"; then
    LEG_STATUS[e2e-parity]=PASS
  else
    LEG_STATUS[e2e-parity]=FAIL
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
