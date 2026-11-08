#!/usr/bin/env python3
"""Tests for scripts/package-profile.py (no network, no real release assets)."""

from __future__ import annotations

import hashlib
import io
import json
import os
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
SCRIPT = REPO_ROOT / "scripts" / "package-profile.py"
PROFILES_PATH = REPO_ROOT / "scripts" / "profiles.json"
INSTALL_SH = REPO_ROOT / "install.sh"

VERSION = "0.1.8"
TARGET = "x86_64-unknown-linux-gnu"


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def make_rpix(
    dist: Path,
    name: str,
    version: str = VERSION,
    target: str = TARGET,
    default_settings: dict | None = None,
    manifest_name: str | None = None,
    sidecar_value: str | None = None,
    manifest_version: str | None = None,
    file_name: str | None = None,
) -> Path:
    manifest = {
        "name": manifest_name if manifest_name is not None else name,
        "version": manifest_version if manifest_version is not None else version,
        "native": "libtest.so",
    }
    if default_settings is not None:
        manifest["defaultSettings"] = default_settings
    payload = json.dumps(manifest).encode()
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w") as archive:
        info = tarfile.TarInfo("rpi-extension.json")
        info.size = len(payload)
        archive.addfile(info, io.BytesIO(payload))
        lib = b"\x7fELF fake cdylib"
        info = tarfile.TarInfo("libtest.so")
        info.size = len(lib)
        archive.addfile(info, io.BytesIO(lib))
    import gzip

    data = gzip.compress(raw.getvalue(), mtime=0)
    path = dist / (file_name or f"{name}-{version}-{target}.rpix")
    path.write_bytes(data)
    digest = sidecar_value if sidecar_value is not None else sha256(data)
    (dist / f"{path.name}.sha256").write_text(f"{digest}  {path.name}\n")
    return path


def load_profiles() -> dict[str, list[str]]:
    return json.loads(PROFILES_PATH.read_text())["profiles"]


def run(
    dist: Path, binary: Path, *extra: str, target: str = TARGET
) -> subprocess.CompletedProcess:
    return subprocess.run(
        [
            sys.executable,
            str(SCRIPT),
            "--dist",
            str(dist),
            "--version",
            VERSION,
            "--target",
            target,
            "--binary",
            str(binary),
            *extra,
        ],
        capture_output=True,
        text=True,
    )


def tar_members(path: Path) -> dict[str, bytes]:
    with tarfile.open(path, "r:gz") as archive:
        return {
            member.name: archive.extractfile(member).read()
            for member in archive.getmembers()
            if member.isfile()
        }


def zip_members(path: Path) -> dict[str, bytes]:
    with zipfile.ZipFile(path) as archive:
        return {
            name: archive.read(name)
            for name in archive.namelist()
            if not name.endswith("/")
        }


class PackageProfileTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.dist = self.root / "dist"
        self.dist.mkdir()
        self.binary = self.root / "rpi"
        self.binary.write_bytes(b"#!/bin/sh\necho rpi\n")
        self.binary.chmod(0o755)
        self.profiles = load_profiles()

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def seed_plugins(self, names: list[str], target: str = TARGET) -> None:
        defaults = {
            "rpi-statusline": {
                "statusLine": {"type": "items", "items": ["live-total-tokens"]}
            },
            "rpi-subagents": {"subagents": {"defaultAgent": "worker"}},
        }
        for name in names:
            make_rpix(
                self.dist, name, target=target, default_settings=defaults.get(name)
            )

    def test_standard_members_and_settings(self) -> None:
        self.seed_plugins(self.profiles["standard"])
        result = run(self.dist, self.binary, "--profiles", "standard")
        self.assertEqual(result.returncode, 0, result.stderr)
        archive = self.dist / f"rpi-{VERSION}-{TARGET}-standard.tar.gz"
        self.assertTrue(archive.is_file())
        self.assertTrue((self.dist / f"{archive.name}.sha256").is_file())
        sidecar = (self.dist / f"{archive.name}.sha256").read_text().split()[0]
        self.assertEqual(sidecar, sha256(archive.read_bytes()))
        members = tar_members(archive)
        expected = {
            "rpi",
            "settings.defaults.json",
            *(
                f"extensions/{name}-{VERSION}-{TARGET}.rpix"
                for name in self.profiles["standard"]
            ),
            *(
                f"extensions/{name}-{VERSION}-{TARGET}.rpix.sha256"
                for name in self.profiles["standard"]
            ),
        }
        self.assertEqual(set(members), expected)
        settings = json.loads(members["settings.defaults.json"])
        self.assertEqual(settings["packages"], self.profiles["standard"])
        self.assertEqual(
            settings["statusLine"], {"type": "items", "items": ["live-total-tokens"]}
        )
        self.assertEqual(settings["subagents"], {"defaultAgent": "worker"})
        # The embedded plugin bytes are the dist bytes, untouched.
        for name in self.profiles["standard"]:
            rpix = self.dist / f"{name}-{VERSION}-{TARGET}.rpix"
            self.assertEqual(
                members[f"extensions/{rpix.name}"], rpix.read_bytes()
            )

    def test_full_members(self) -> None:
        self.seed_plugins(self.profiles["full"])
        result = run(self.dist, self.binary, "--profiles", "full")
        self.assertEqual(result.returncode, 0, result.stderr)
        archive = self.dist / f"rpi-{VERSION}-{TARGET}-full.tar.gz"
        members = tar_members(archive)
        self.assertEqual(
            json.loads(members["settings.defaults.json"])["packages"],
            self.profiles["full"],
        )
        for name in self.profiles["full"]:
            self.assertIn(f"extensions/{name}-{VERSION}-{TARGET}.rpix", members)

    def test_default_settings_first_wins(self) -> None:
        self.seed_plugins(self.profiles["standard"])
        make_rpix(self.dist, "rpi-subagents", default_settings={"theme": "first"})
        make_rpix(self.dist, "rpi-todo", default_settings={"theme": "second"})
        result = run(self.dist, self.binary, "--profiles", "standard")
        self.assertEqual(result.returncode, 0, result.stderr)
        archive = self.dist / f"rpi-{VERSION}-{TARGET}-standard.tar.gz"
        settings = json.loads(tar_members(archive)["settings.defaults.json"])
        self.assertEqual(settings["theme"], "first")
        self.assertEqual(settings["packages"], self.profiles["standard"])

    def test_missing_plugin_fails(self) -> None:
        self.seed_plugins(self.profiles["standard"][:-1])
        result = run(self.dist, self.binary, "--profiles", "standard")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("missing plugin archive", result.stderr)
        self.assertFalse(
            (self.dist / f"rpi-{VERSION}-{TARGET}-standard.tar.gz").exists()
        )

    def test_missing_sidecar_fails(self) -> None:
        self.seed_plugins(self.profiles["standard"])
        (self.dist / f"rpi-todo-{VERSION}-{TARGET}.rpix.sha256").unlink()
        result = run(self.dist, self.binary, "--profiles", "standard")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("missing plugin sidecar", result.stderr)

    def test_stale_sidecar_fails(self) -> None:
        self.seed_plugins(self.profiles["standard"])
        make_rpix(
            self.dist,
            "rpi-todo",
            default_settings={},
            sidecar_value="0" * 64,
        )
        result = run(self.dist, self.binary, "--profiles", "standard")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("plugin sidecar mismatch", result.stderr)

    def test_manifest_name_mismatch_fails(self) -> None:
        self.seed_plugins(self.profiles["standard"])
        make_rpix(self.dist, "rpi-todo", manifest_name="someone-else")
        result = run(self.dist, self.binary, "--profiles", "standard")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("manifest name", result.stderr)

    def test_manifest_version_mismatch_fails(self) -> None:
        self.seed_plugins(self.profiles["standard"])
        make_rpix(self.dist, "rpi-todo", manifest_version="0.0.1")
        result = run(self.dist, self.binary, "--profiles", "standard")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("manifest version", result.stderr)

    def test_zip_branch(self) -> None:
        windows_target = "x86_64-pc-windows-msvc"
        self.seed_plugins(self.profiles["standard"], target=windows_target)
        binary = self.root / "rpi.exe"
        binary.write_bytes(b"MZ fake")
        result = run(
            self.dist,
            binary,
            "--profiles",
            "standard",
            "--format",
            "zip",
            target=windows_target,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        archive = self.dist / f"rpi-{VERSION}-{windows_target}-standard.zip"
        self.assertTrue(archive.is_file())
        members = zip_members(archive)
        self.assertIn("rpi.exe", members)
        self.assertNotIn("rpi", members)
        self.assertIn("settings.defaults.json", members)
        self.assertIn(
            f"extensions/rpi-todo-{VERSION}-{windows_target}.rpix", members
        )

    def test_static_archive_contains_only_binary(self) -> None:
        self.seed_plugins(self.profiles["standard"])
        static_binary = self.root / "rpi-static"
        static_binary.write_bytes(b"static rpi")
        result = run(
            self.dist,
            static_binary,
            "--static",
            target="x86_64-unknown-linux-musl",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        archive = self.dist / f"rpi-{VERSION}-x86_64-unknown-linux-musl-static.tar.gz"
        self.assertTrue(archive.is_file())
        self.assertEqual(set(tar_members(archive)), {"rpi"})
        self.assertEqual(
            tar_members(archive)["rpi"], b"static rpi"
        )
        self.assertTrue((self.dist / f"{archive.name}.sha256").is_file())

    def test_static_rejects_zip(self) -> None:
        result = run(self.dist, self.binary, "--static", "--format", "zip")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("tar.gz only", result.stderr)

    def test_missing_binary_fails(self) -> None:
        result = run(self.dist, self.root / "nope")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("binary not found", result.stderr)

    def test_profiles_json_shapes(self) -> None:
        standard = self.profiles["standard"]
        full = self.profiles["full"]
        self.assertEqual(self.profiles["mini"], [])
        self.assertEqual(full[:-1], standard)
        self.assertEqual(len(full), len(standard) + 1)

    def test_install_sh_constants_match_profiles(self) -> None:
        """install.sh cannot read JSON at runtime; keep its lists in lockstep."""
        text = INSTALL_SH.read_text()
        standard_match = None
        full_match = None
        for line in text.splitlines():
            if line.startswith("RPI_PROFILE_STANDARD_PLUGINS="):
                value = line.split("=", 1)[1]
                standard_match = value.strip().strip("'\"")
            elif line.startswith("RPI_PROFILE_FULL_PLUGINS="):
                full_match = line.split("=", 1)[1].strip().strip("'\"")
        self.assertIsNotNone(standard_match, "missing RPI_PROFILE_STANDARD_PLUGINS")
        self.assertIsNotNone(full_match, "missing RPI_PROFILE_FULL_PLUGINS")
        self.assertEqual(standard_match.split(), self.profiles["standard"])
        expanded = full_match
        if expanded.startswith("$RPI_PROFILE_STANDARD_PLUGINS"):
            expanded = expanded.replace(
                "$RPI_PROFILE_STANDARD_PLUGINS", standard_match, 1
            )
        self.assertEqual(expanded.split(), self.profiles["full"])


    def test_install_ps1_constants_match_profiles(self) -> None:
        """The Windows installer carries the same plugin list; keep them equal."""
        import re

        text = (REPO_ROOT / "install.ps1").read_text()
        start = text.index("function Get-StandardPluginList")
        end = text.index("function Get-ProfilePluginList")
        block = text[start:end]
        names = re.findall(r"'((?:rpi-)[a-z0-9-]+)'", block)
        self.assertEqual(names, self.profiles["standard"])
        full_text = text[end:]
        tail_end = full_text.index("\n}\n", full_text.index("'full'"))
        self.assertIn("'rpi-usage'", full_text[:tail_end])


if __name__ == "__main__":
    unittest.main(verbosity=2)