#!/usr/bin/env bash
# Refreshes or checks the vendored built-in model catalog. Generated data is
# read-only; all content changes come from the pinned upstream generator.
#
# Generate in a throwaway worktree at the pinned commit, never in external/pi:
#   git -C external/pi worktree add --detach /tmp/pi-catalog-gen <pin>
#   ln -s "$PWD/external/pi/node_modules" /tmp/pi-catalog-gen/node_modules
#   cd /tmp/pi-catalog-gen/packages/ai
#   node scripts/generate-models.ts --strict \
#     --json-output /tmp/rpi-site-catalog
#
# Vendor the same snapshot into rpi, or verify an existing vendoring without
# changing files:
#   scripts/refresh-model-catalog.sh /tmp/pi-catalog-gen/packages/ai/src/providers/data
#   scripts/refresh-model-catalog.sh --check /tmp/pi-catalog-gen/packages/ai/src/providers/data
#
# This script is offline: network access belongs to the explicit generation
# step above. The vendored set is an exact manifest-driven mirror, including
# removals, and --check is the CI/local regeneration-cleanliness gate.
set -euo pipefail

cd "$(dirname "$0")/.."

CHECK=0
if [[ "${1:-}" == "--check" ]]; then
    CHECK=1
    shift
fi
SRC="${1:-external/pi/packages/ai/src/providers/data}"
DST="crates/rpi-ai/src/providers/data"

if [[ ! -f "$SRC/.manifest.json" ]]; then
    echo "error: $SRC/.manifest.json not found (upstream data dir?)" >&2
    exit 1
fi

python3 - "$SRC" "$DST" "$CHECK" <<'EOF'
import hashlib
import json
from pathlib import Path
import shutil
import sys

src, dst, check = Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3] == "1"
manifest_path = src / ".manifest.json"
manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
files = manifest["files"]
expected = set(files) | {".manifest.json"}
actual = {path.name for path in src.glob("*.json")}
if actual != expected:
    sys.exit(f"error: source file set differs from manifest: {sorted(actual ^ expected)}")

def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()

bad = [name for name, want in files.items() if digest(src / name) != want]
if bad:
    sys.exit(f"error: source sha256 mismatch: {bad}")

if check:
    differences = []
    if not dst.is_dir():
        differences.append("destination directory is missing")
    else:
        destination_files = {path.name for path in dst.glob("*.json")}
        differences.extend(f"file set: {name}" for name in sorted(destination_files ^ expected))
        for name in sorted(expected & destination_files):
            if (src / name).read_bytes() != (dst / name).read_bytes():
                differences.append(f"content: {name}")
    if differences:
        print("error: vendored catalog differs from generated snapshot:", file=sys.stderr)
        for difference in differences:
            print(f"  {difference}", file=sys.stderr)
        sys.exit(1)
    print(f"ok: vendored catalog matches generated snapshot ({len(files)} providers)")
else:
    dst.mkdir(parents=True, exist_ok=True)
    for name in sorted(expected):
        shutil.copyfile(src / name, dst / name)
    for path in dst.glob("*.json"):
        if path.name not in expected:
            path.unlink()
    bad = [name for name, want in files.items() if digest(dst / name) != want]
    if bad:
        sys.exit(f"error: sha256 mismatch after vendor: {bad}")
    print(f"vendored {len(files)} provider catalogs + .manifest.json from {src}")
    print(f"ok: manifest sha256 verified (generatedAt {manifest['generatedAt']})")
EOF

echo "next: cargo test -p rpi-ai --test model_catalog --test compat_matrix"
