#!/usr/bin/env bash
# Verifies that external/ submodules are exactly at the upstream pins recorded
# in UPSTREAM.md (coding-standards §15.2, ADR-0002 §1).
#
# Checks, for external/pi and each plugin reference submodule listed in
# UPSTREAM.md's plugin pin table:
#   1. HEAD equals the pinned commit from UPSTREAM.md.
#   2. The submodule has no local modifications (read-only references).
#
# Exit code 0 on success, 1 with a diagnostic on failure.
set -euo pipefail

cd "$(dirname "$0")/.."

check_pin() {
    local dir="$1" expected="$2"
    if ! git -C "$dir" rev-parse --git-dir >/dev/null 2>&1; then
        echo "error: $dir is not a git checkout (submodule not initialized?)" >&2
        exit 1
    fi
    local actual
    actual=$(git -C "$dir" rev-parse HEAD)
    echo "$dir HEAD: $actual"
    echo "$dir pin:  $expected"
    if [[ "$actual" != "$expected" ]]; then
        echo "error: upstream pin mismatch — $dir must stay at the pinned commit" >&2
        exit 1
    fi
    if [[ -n "$(git -C "$dir" status --porcelain)" ]]; then
        echo "error: $dir has local modifications (it is a read-only reference)" >&2
        exit 1
    fi
}

EXPECTED=$(grep -E '^\| Git commit \|' UPSTREAM.md | grep -oE '[0-9a-f]{40}' | head -1)
if [[ -z "$EXPECTED" ]]; then
    echo "error: could not parse pinned commit from UPSTREAM.md" >&2
    exit 1
fi
check_pin external/pi "$EXPECTED"

# Plugin reference pins: one `| \`external/<name>\` | \`<sha>\` |` row per
# submodule in UPSTREAM.md's plugin pin table.
while IFS='|' read -r _ dir sha _; do
    dir=$(echo "$dir" | tr -d ' `')
    sha=$(echo "$sha" | grep -oE '[0-9a-f]{40}' | head -1)
    check_pin "$dir" "$sha"
done < <(grep -E '^\| `external/' UPSTREAM.md)

# Codemode/tool_search source comments cite upstream line ranges; the pin
# check above only compares commits, so verify the anchors still point at the
# pinned lines (V16-07 review O-E).
bash scripts/verify-upstream-anchors.sh

echo "ok: upstream pins verified"
