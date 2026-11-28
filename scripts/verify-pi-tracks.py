#!/usr/bin/env python3
"""Verify the pinned Pi regression/target tracks without modifying external/."""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
CONFIG_PATH = ROOT / "scripts" / "upstream-tracks" / "pi.json"


def git(repository: Path, *args: str, check: bool = True) -> str:
    result = subprocess.run(
        ["git", "-C", str(repository), *args],
        check=False,
        capture_output=True,
        text=True,
    )
    if check and result.returncode != 0:
        raise ValueError(result.stderr.strip() or f"git {' '.join(args)} failed")
    if not check:
        return str(result.returncode)
    return result.stdout.strip()


def require_equal(label: str, actual: Any, expected: Any) -> None:
    if actual != expected:
        raise ValueError(f"{label}: expected {expected!r}, got {actual!r}")


def verify_track(repository: Path, label: str, track: dict[str, str]) -> None:
    commit = git(repository, "rev-parse", f"{track['tag']}^{{}}")
    tree = git(repository, "rev-parse", f"{track['tag']}^{{tree}}")
    require_equal(f"{label} tag commit", commit, track["commit"])
    require_equal(f"{label} tree", tree, track["tree"])
    require_equal(f"{label} commit object", git(repository, "cat-file", "-t", commit), "commit")
    print(f"{label}: {track['tag']} commit={commit} tree={tree}")


def verify_range(repository: Path, regression: str, target: str, expected: dict[str, int]) -> None:
    require_equal(
        "regression is an ancestor of target",
        git(repository, "merge-base", "--is-ancestor", regression, target, check=False),
        "0",
    )
    commits = int(git(repository, "rev-list", "--count", f"{regression}..{target}"))
    require_equal("range commits", commits, expected["commits"])

    rows = git(repository, "diff", "--numstat", regression, target, "--").splitlines()
    insertions = 0
    deletions = 0
    for row in rows:
        added, removed, _path = row.split("\t", 2)
        if added != "-":
            insertions += int(added)
        if removed != "-":
            deletions += int(removed)
    require_equal("range files", len(rows), expected["files"])
    require_equal("range insertions", insertions, expected["insertions"])
    require_equal("range deletions", deletions, expected["deletions"])
    print(
        f"range: commits={commits} files={len(rows)} "
        f"insertions={insertions} deletions={deletions}"
    )


def main() -> int:
    try:
        config = json.loads(CONFIG_PATH.read_text(encoding="utf-8"))
        require_equal("config schema", config["schema"], 1)
        repository = ROOT / config["repository"]
        require_equal("repository path", config["repository"], "external/pi")
        regression = config["regression"]
        target = config["target"]
        verify_track(repository, "regression", regression)
        verify_track(repository, "target", target)
        verify_range(repository, regression["commit"], target["commit"], config["range"])
    except (KeyError, TypeError, ValueError, OSError, json.JSONDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print("ok: Pi target/regression tracks verified")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
