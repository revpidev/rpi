#!/usr/bin/env python3
"""Synchronize the four configuration schemas from the pinned Pi checkout.

The schemas are generated upstream from the TypeScript configuration models
(settings/models/keybindings/theme). rpi keeps the same contracts, so this
script vendors the generated JSON, rewrites the public `$id` from pi.dev to
revpi.dev and writes both the repository copy (`schemas/`) and the site copy
(`../rpi-pages/schemas/`). No schema is hand-edited.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
UPSTREAM = ROOT / "external" / "pi"
SOURCE_DIR = UPSTREAM / "packages" / "coding-agent" / "schemas"
OUTPUT_DIR = ROOT / "schemas"
SITE_DIR = ROOT.parent / "rpi-pages" / "schemas"
SCHEMAS = (
    "settings.schema.json",
    "models.schema.json",
    "keybindings.schema.json",
    "theme.schema.json",
)
EXPECTED_PIN = "4a080252f06caf63a525d7873c5d94d3c23887e0"
GENERATED_COMMENT = (
    "This file is generated from the pinned Pi v1.4.0 TypeScript source "
    "(4a080252f) by scripts/sync-config-schemas.py in the rpi repository. "
    "Do not edit it manually; run the script to refresh it."
)
ID_RE = re.compile(r'"\$id"\s*:\s*"https://pi\.dev/schemas/([^"]+)"')
COMMENT_RE = re.compile(r'"\$comment"\s*:\s*"[^"]*"')


class SyncError(Exception):
    pass


def assert_pin() -> None:
    try:
        head = subprocess.check_output(
            ["git", "-C", str(UPSTREAM), "rev-parse", "HEAD"], text=True
        ).strip()
    except (OSError, subprocess.CalledProcessError) as error:
        raise SyncError(f"cannot read external/pi HEAD: {error}") from error
    if head != EXPECTED_PIN:
        raise SyncError(
            f"external/pi HEAD is {head}, expected {EXPECTED_PIN}; update the "
            "ADR/UPSTREAM pin before regenerating schemas"
        )


def render(source: Path) -> bytes:
    text = source.read_text(encoding="utf-8")
    text, ids = ID_RE.subn(
        lambda match: f'"$id": "https://revpi.dev/schemas/{match.group(1)}"', text
    )
    if ids != 1:
        raise SyncError(f"{source}: expected exactly one pi.dev $id, found {ids}")
    text, comments = COMMENT_RE.subn(f'"$comment": "{GENERATED_COMMENT}"', text)
    if comments != 1:
        raise SyncError(f"{source}: expected exactly one $comment, found {comments}")
    if "https://pi.dev" in text:
        raise SyncError(f"{source}: a pi.dev reference survived the rewrite")
    return text.encode("utf-8")


def expected_files() -> dict[str, bytes]:
    files: dict[str, bytes] = {}
    for name in SCHEMAS:
        source = SOURCE_DIR / name
        if not source.is_file():
            raise SyncError(f"missing upstream schema: {source}")
        files[name] = render(source)
    return files


def write_files(files: dict[str, bytes], output: Path) -> None:
    output.mkdir(parents=True, exist_ok=True)
    for name, data in files.items():
        (output / name).write_bytes(data)


def check_files(files: dict[str, bytes], output: Path) -> bool:
    ok = True
    for name, data in files.items():
        target = output / name
        if not target.is_file() or target.read_bytes() != data:
            print(f"out of date: {target}", file=sys.stderr)
            ok = False
    return ok


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--no-site", action="store_true")
    args = parser.parse_args()

    try:
        assert_pin()
        files = expected_files()
        outputs = [OUTPUT_DIR]
        if not args.no_site:
            outputs.append(SITE_DIR)
        if args.check:
            ok = all(check_files(files, output) for output in outputs)
            return 0 if ok else 1
        for output in outputs:
            write_files(files, output)
        print(
            "config schemas synchronized: "
            + ", ".join(str(output) for output in outputs)
        )
        return 0
    except SyncError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
