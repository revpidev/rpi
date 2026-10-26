#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.dont_write_bytecode = True
SCRIPT = Path(__file__).resolve().with_name("sync-export-html-assets.py")
SPEC = importlib.util.spec_from_file_location("sync_export_html_assets", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
sync_assets = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(sync_assets)


class SyncExportHtmlAssetsTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.source = self.root / "upstream"
        self.output = self.root / "output"
        self.upstream = self.root / "repo"
        self.old_paths = (
            sync_assets.SOURCE_DIR,
            sync_assets.OUTPUT_DIR,
            sync_assets.UPSTREAM,
        )
        sync_assets.SOURCE_DIR = self.source
        sync_assets.OUTPUT_DIR = self.output
        sync_assets.UPSTREAM = self.upstream
        for relative, _ in sync_assets.ASSETS:
            path = self.source / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(f"asset {relative}\n", encoding="utf-8")
        (self.source / "template.js").write_text(
            "pi-url-params pi-share-base-url pi-share:v1: pi-untouched 中文\n",
            encoding="utf-8",
        )
        self.upstream.mkdir()

    def tearDown(self) -> None:
        (
            sync_assets.SOURCE_DIR,
            sync_assets.OUTPUT_DIR,
            sync_assets.UPSTREAM,
        ) = self.old_paths
        self.temp.cleanup()

    def test_generate_check_and_repeat_are_byte_stable(self) -> None:
        sync_assets.sync(False)
        first = {path.relative_to(self.output).as_posix(): path.read_bytes() for path in self.output.rglob("*") if path.is_file()}
        self.assertEqual(len(first), 6)
        self.assertEqual(
            first["template.js"],
            b"rpi-url-params rpi-share-base-url rpi-share:v1: pi-untouched \xe4\xb8\xad\xe6\x96\x87\n",
        )
        mtimes = {path: path.stat().st_mtime_ns for path in self.output.rglob("*") if path.is_file()}
        sync_assets.sync(False)
        sync_assets.sync(True)
        self.assertEqual(first, {path.relative_to(self.output).as_posix(): path.read_bytes() for path in self.output.rglob("*") if path.is_file()})
        self.assertEqual(mtimes, {path: path.stat().st_mtime_ns for path in self.output.rglob("*") if path.is_file()})
        self.assertEqual((self.source / "template.js").read_text(encoding="utf-8"), "pi-url-params pi-share-base-url pi-share:v1: pi-untouched 中文\n")

    def test_undeclared_output_fails_before_any_write(self) -> None:
        self.output.mkdir()
        (self.output / "unexpected.txt").write_text("do not keep", encoding="utf-8")
        with self.assertRaisesRegex(sync_assets.SyncError, "undeclared export asset output"):
            sync_assets.sync(False)
        self.assertFalse((self.output / "template.html").exists())

    def test_missing_brand_token_fails_before_any_write(self) -> None:
        (self.source / "template.js").write_text("unrelated\n", encoding="utf-8")
        with self.assertRaisesRegex(sync_assets.SyncError, "brand token missing"):
            sync_assets.sync(False)
        self.assertFalse((self.output / "template.html").exists())

    def test_pin_check_requires_the_exact_checkout(self) -> None:
        subprocess.run(["git", "init", "-q"], cwd=self.upstream, check=True)
        subprocess.run(["git", "config", "user.name", "Test"], cwd=self.upstream, check=True)
        subprocess.run(["git", "config", "user.email", "test@example.invalid"], cwd=self.upstream, check=True)
        (self.upstream / "tracked").write_text("content", encoding="utf-8")
        subprocess.run(["git", "add", "tracked"], cwd=self.upstream, check=True)
        subprocess.run(["git", "commit", "-qm", "initial"], cwd=self.upstream, check=True)
        actual = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=self.upstream, text=True).strip()
        sync_assets.assert_pin(actual)
        with self.assertRaisesRegex(sync_assets.SyncError, "external/pi HEAD is"):
            sync_assets.assert_pin("0" * 40)


if __name__ == "__main__":
    unittest.main()
