#!/usr/bin/env python3
"""Synchronize the six embedded export-html assets from the pinned Pi checkout."""

from __future__ import annotations

import argparse
import hashlib
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
UPSTREAM = ROOT / "external" / "pi"
SOURCE_DIR = UPSTREAM / "packages" / "coding-agent" / "src" / "core" / "export-html"
OUTPUT_DIR = ROOT / "crates" / "rpi" / "src" / "core" / "export_html"

ASSETS = (
    ("template.html", "template.html"),
    ("template.css", "template.css"),
    ("template.js", "template.js"),
    ("vendor/marked.min.js", "vendor/marked.min.js"),
    ("vendor/marked-cjk-friendly.min.js", "vendor/marked-cjk-friendly.min.js"),
    ("vendor/highlight.min.js", "vendor/highlight.min.js"),
)
BRAND_RENAMES = (
    ("pi-url-params", "rpi-url-params"),
    ("pi-share-base-url", "rpi-share-base-url"),
    ("pi-share:v1:", "rpi-share:v1:"),
)
PIN_PATTERN = re.compile(r"^[0-9a-f]{40}$")


class SyncError(Exception):
    pass


def expected_assets() -> dict[str, bytes]:
    expected: dict[str, bytes] = {}
    for relative, source_relative in ASSETS:
        source = SOURCE_DIR / source_relative
        if not source.is_file():
            raise SyncError(f"missing upstream asset: {source}")
        data = source.read_bytes()
        if relative == "template.js":
            text = data.decode("utf-8")
            for upstream_name, rpi_name in BRAND_RENAMES:
                if upstream_name not in text:
                    raise SyncError(f"brand token missing from template.js: {upstream_name}")
                text = text.replace(upstream_name, rpi_name)
            data = text.encode("utf-8")
        expected[relative] = data
    return expected


def validate_output_directory() -> None:
    if not OUTPUT_DIR.exists():
        return
    if OUTPUT_DIR.is_symlink() or not OUTPUT_DIR.is_dir():
        raise SyncError(f"invalid export asset output directory: {OUTPUT_DIR}")
    declared = {relative for relative, _ in ASSETS}
    for path in OUTPUT_DIR.rglob("*"):
        relative = path.relative_to(OUTPUT_DIR).as_posix()
        if path.is_dir() and not path.is_symlink():
            continue
        if path.is_symlink() or not path.is_file() or relative not in declared:
            raise SyncError(f"undeclared export asset output: {path}")


def write_asset(relative: str, data: bytes) -> None:
    destination = OUTPUT_DIR / relative
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(
        dir=destination.parent, prefix=f".{destination.name}.", delete=False
    ) as temporary:
        temporary.write(data)
        temporary.flush()
        os.fsync(temporary.fileno())
        temporary_path = Path(temporary.name)
    os.replace(temporary_path, destination)


def sync(check: bool) -> list[tuple[str, str]]:
    validate_output_directory()
    expected = expected_assets()
    digests: list[tuple[str, str]] = []
    mismatches: list[str] = []
    for relative, data in expected.items():
        digest = hashlib.sha256(data).hexdigest()
        digests.append((relative, digest))
        destination = OUTPUT_DIR / relative
        if check:
            if not destination.is_file() or destination.read_bytes() != data:
                mismatches.append(relative)
        else:
            write_asset(relative, data)
    if mismatches:
        raise SyncError(f"export assets differ from pinned upstream: {', '.join(mismatches)}")
    return digests


def assert_pin(pin: str) -> None:
    if not PIN_PATTERN.fullmatch(pin):
        raise SyncError("--pin must be a full 40-character lowercase commit hash")
    result = subprocess.run(
        ["git", "-C", str(UPSTREAM), "rev-parse", "HEAD"],
        check=False,
        capture_output=True,
        text=True,
    )
    actual = result.stdout.strip()
    if result.returncode != 0:
        raise SyncError(f"cannot read external/pi HEAD: {result.stderr.strip()}")
    if actual != pin:
        raise SyncError(f"external/pi HEAD is {actual}, expected {pin}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pin", required=True, help="required full external/pi commit hash")
    parser.add_argument("--check", action="store_true", help="compare without writing files")
    args = parser.parse_args()
    try:
        assert_pin(args.pin)
        digests = sync(args.check)
    except SyncError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    for relative, digest in digests:
        print(f"{relative}: {digest}")
    print(f"ok: 6 export-html assets synchronized at {args.pin}" + (" (check only)" if args.check else ""))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
