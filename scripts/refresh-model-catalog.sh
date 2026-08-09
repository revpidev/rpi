#!/usr/bin/env bash
# Refreshes the vendored built-in model catalog (T13 W4; coding-standards §3.2:
# generated data is read-only, changes go through the generator).
#
# Source of truth: external/pi/packages/ai/src/providers/data/*.json — the
# output of upstream `packages/ai/scripts/generate-models.ts`, which pulls
# models.dev + the OpenRouter/NVIDIA/Vercel APIs and bakes in all correction
# rules (compat deltas, thinkingLevelMap, pricing tiers, implied Kimi costs).
#
# This script is intentionally OFFLINE: it never fetches models.dev itself.
# Refresh flow:
#   1. Bump the upstream pin (requires an ADR, coding-standards §15.2) and, in
#      the upstream checkout, run its generator:
#        cd external/pi && npm run generate-models   # upstream repo, network
#      (Do this on a throwaway upstream worktree — external/pi in THIS repo is
#      a read-only reference and must stay unmodified.)
#   2. Run this script to re-vendor the resulting JSONs + .manifest.json.
#   3. Re-run the catalog tests; update the pinned `generatedAt` expectation in
#      crates/rpi-ai/tests/model_catalog.rs.
#
# Usage: scripts/refresh-model-catalog.sh [upstream-data-dir]
set -euo pipefail

cd "$(dirname "$0")/.."

SRC="${1:-external/pi/packages/ai/src/providers/data}"
DST="crates/rpi-ai/src/providers/data"

if [[ ! -f "$SRC/.manifest.json" ]]; then
    echo "error: $SRC/.manifest.json not found (upstream data dir?)" >&2
    exit 1
fi

count=$(find "$SRC" -maxdepth 1 -name '*.json' ! -name '.*' | wc -l)
if [[ "$count" -eq 0 ]]; then
    echo "error: no provider JSONs in $SRC" >&2
    exit 1
fi

cp "$SRC"/*.json "$SRC"/.manifest.json "$DST/"
echo "vendored $count provider catalogs + .manifest.json from $SRC"

# Post-vendor integrity check: each file must match its manifest sha256.
python3 - "$DST" <<'EOF'
import hashlib, json, sys
d = sys.argv[1]
manifest = json.load(open(f"{d}/.manifest.json"))
bad = [
    name for name, want in manifest["files"].items()
    if hashlib.sha256(open(f"{d}/{name}", "rb").read()).hexdigest() != want
]
if bad:
    sys.exit(f"error: sha256 mismatch after vendor: {bad}")
print(f"ok: {len(manifest['files'])} files match manifest sha256 (generatedAt {manifest['generatedAt']})")
EOF

echo "next: cargo test -p rpi-ai --test model_catalog --test compat_matrix"
