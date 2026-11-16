#!/usr/bin/env bash
# V19-00 replay/proc harness deterministic self-test. Requires release binaries.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
BINARY="${1:-$ROOT/target/release/rpi-v19-replay}"
OUT="${TMPDIR:-/tmp}/rpi-v19-harness-self-test"
rm -rf "$OUT"
mkdir -p "$OUT"

python3 scripts/v019-benchmark/generate_inputs.py --check
python3 -m unittest discover -s scripts/v019-benchmark -p 'test_*.py' -v

python3 scripts/v019-benchmark/run_replay.py \
  --binary "$BINARY" \
  --bundle fixtures/v019-benchmark/bundles/smoke-text-regular.json \
  --output-dir "$OUT/smoke" \
  --repeat 2 >"$OUT/smoke.log"

if python3 scripts/v019-benchmark/run_replay.py \
  --binary "$BINARY" \
  --bundle fixtures/v019-benchmark/bundles/smoke-text-regular.json \
  --output-dir "$OUT/bad-hash" \
  --repeat 1 \
  --input-sha256 0000000000000000000000000000000000000000000000000000000000000000 \
  >"$OUT/bad-hash.log" 2>&1; then
  echo "error: an incorrect input hash was accepted" >&2
  exit 1
fi

INPUT_HASH=$(python3 - <<'PY'
import json
m = json.load(open('fixtures/v019-benchmark/bundles/manifest.json'))
print(m['bundles']['smoke-text-regular.json']['sha256'])
PY
)
if env -u RPI_V19_BENCHMARK_LOG RPI_OFFLINE=1 "$BINARY" \
  --bundle fixtures/v019-benchmark/bundles/smoke-text-regular.json \
  --agent-dir "$OUT/smoke/sandbox/agent" \
  --cwd "$OUT/smoke/sandbox/cwd" \
  --summary "$OUT/missing-log-summary.json" \
  --terminal-output "$OUT/missing-log-terminal.bytes" \
  --input-sha256 "$INPUT_HASH" >"$OUT/missing-log.log" 2>&1; then
  echo "error: replay passed without a frame log" >&2
  exit 1
fi

cargo build -q -p rpi --bin rpi-v19-replay --features v19-benchmark
if target/debug/rpi-v19-replay >"$OUT/debug-profile.log" 2>&1; then
  echo "error: replay passed in a debug build" >&2
  exit 1
fi
grep -q 'requires a release-profile build' "$OUT/debug-profile.log"

python3 scripts/v019-benchmark/proc_sample.py capture \
  --pid $$ --label harness-self-test --output "$OUT/proc"
PID=$(python3 - "$OUT/proc/capture.json" <<'PY'
import json, sys
print(json.load(open(sys.argv[1]))['processes'][0]['pid'])
PY
)
python3 scripts/v019-benchmark/proc_sample.py compare \
  --baseline "$OUT/proc/$PID/summary.json" \
  --candidate "$OUT/proc/$PID/summary.json" \
  --output "$OUT/proc/delta.json"
python3 scripts/v019-benchmark/proc_sample.py check-budget --report "$OUT/proc/delta.json"

printf 'ok: V19-00 replay and /proc harness self-test passed; logs: %s\n' "$OUT"
