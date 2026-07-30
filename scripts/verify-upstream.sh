#!/usr/bin/env bash
# Verifies that external/pi is exactly at the upstream pin recorded in
# UPSTREAM.md (coding-standards §15.2, ADR-0002 §1).
#
# Checks:
#   1. HEAD of external/pi equals the pinned commit from UPSTREAM.md.
#   2. external/pi has no local modifications (it is a read-only reference).
#
# Exit code 0 on success, 1 with a diagnostic on failure.
set -euo pipefail

cd "$(dirname "$0")/.."

EXPECTED=$(grep -E '^\| Git commit \|' UPSTREAM.md | grep -oE '[0-9a-f]{40}' | head -1)
if [[ -z "$EXPECTED" ]]; then
    echo "error: could not parse pinned commit from UPSTREAM.md" >&2
    exit 1
fi

if [[ ! -d external/pi/.git ]] && ! git -C external/pi rev-parse --git-dir >/dev/null 2>&1; then
    echo "error: external/pi is not a git checkout (submodule not initialized?)" >&2
    exit 1
fi

ACTUAL=$(git -C external/pi rev-parse HEAD)
echo "external/pi HEAD: $ACTUAL"
echo "UPSTREAM.md pin:  $EXPECTED"

if [[ "$ACTUAL" != "$EXPECTED" ]]; then
    echo "error: upstream pin mismatch — external/pi must stay at the pinned commit" >&2
    exit 1
fi

if [[ -n "$(git -C external/pi status --porcelain)" ]]; then
    echo "error: external/pi has local modifications (it is a read-only reference)" >&2
    exit 1
fi

echo "ok: upstream pin verified"
