#!/usr/bin/env python3
"""Generate the four configuration schemas from rpi's typed configuration models.

The Rust generator and the runtime loaders share the same document definitions.
This wrapper preserves the existing command-line interface and forwards to the
generator, which writes both `schemas/` and `../rpi-pages/schemas/`.
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def main() -> int:
    return subprocess.run(
        ["cargo", "run", "-q", "-p", "rpi", "--bin", "generate_config_schemas", "--", *sys.argv[1:]],
        cwd=ROOT,
        check=False,
    ).returncode


if __name__ == "__main__":
    raise SystemExit(main())
