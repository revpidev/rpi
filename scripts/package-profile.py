#!/usr/bin/env python3
"""Assemble the install-profile archives for one release target.

The release workflow builds the binary and the first-party `.rpix` plugin
archives first; this script composes them into the single archives the
installer downloads (see rpi-docs/install-packaging.md §4/§5):

  rpi-<version>-<target>-standard.tar.gz|zip
      rpi (or rpi.exe), settings.defaults.json,
      extensions/<plugin>-<version>-<target>.rpix(+.sha256)
  rpi-<version>-<target>-full.tar.gz|zip
      same layout with the full plugin set
  rpi-<version>-<target>-static.tar.gz
      the statically linked musl fallback binary only

`scripts/profiles.json` is the single source of truth for the plugin sets.
`settings.defaults.json` is generated from the packed `.rpix` manifests:
`packages` carries the registry names in profile order, and every
`defaultSettings` object is merged top-level first-wins (the same rule
`SettingsManager::apply_global_defaults` uses).

Every archive gets a basename-only `.sha256` sidecar. Missing plugins,
missing/stale sidecars, malformed manifests and unexpected archive members
fail the packaging run instead of producing a partial bundle.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import os
import sys
import tarfile
import zipfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
PROFILES_PATH = REPO_ROOT / "scripts" / "profiles.json"
PROFILE_NAMES = ("mini", "standard", "full")
STATIC_SUFFIX = "-static"
RELEASE_FORMATS = ("tar.gz", "zip")

# Fixed metadata keeps the archives reproducible (the sidecar hash is the
# only thing installers pin, but stable bytes make release diffs reviewable).
TAR_MTIME = 0
ZIP_DATE_TIME = (1980, 1, 1, 0, 0, 0)
FILE_MODE = 0o644
EXEC_MODE = 0o755


def fail(message: str) -> "object":
    print(f"error: {message}", file=sys.stderr)
    raise SystemExit(1)


def load_profiles() -> dict[str, list[str]]:
    try:
        doc = json.loads(PROFILES_PATH.read_text(encoding="utf-8"))
    except OSError as exc:
        fail(f"cannot read {PROFILES_PATH}: {exc}")
    except json.JSONDecodeError as exc:
        fail(f"{PROFILES_PATH} is not valid JSON: {exc}")
    if doc.get("schemaVersion") != 1:
        fail(f"{PROFILES_PATH}: unsupported schemaVersion {doc.get('schemaVersion')!r}")
    profiles = doc.get("profiles")
    if not isinstance(profiles, dict):
        fail(f"{PROFILES_PATH}: missing profiles object")
    if set(profiles) != set(PROFILE_NAMES):
        fail(f"{PROFILES_PATH}: profiles must be exactly {', '.join(PROFILE_NAMES)}")
    result: dict[str, list[str]] = {}
    for name in PROFILE_NAMES:
        plugins = profiles[name]
        if not isinstance(plugins, list) or not all(
            isinstance(p, str) and p for p in plugins
        ):
            fail(f"{PROFILES_PATH}: profiles.{name} must be a list of plugin names")
        if len(set(plugins)) != len(plugins):
            fail(f"{PROFILES_PATH}: profiles.{name} contains duplicate plugins")
        result[name] = plugins
    standard, full = result["standard"], result["full"]
    if result["mini"]:
        fail(f"{PROFILES_PATH}: the mini profile must not install plugins")
    if full[: len(standard)] != standard or len(full) != len(standard) + 1:
        fail(
            f"{PROFILES_PATH}: full must extend standard with exactly one plugin "
            f"(standard={standard}, full={full})"
        )
    return result


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def read_sidecar_sha256(path: Path) -> str:
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as exc:
        fail(f"cannot read sidecar {path}: {exc}")
    token = text.split(None, 1)[0] if text.split() else ""
    if len(token) != 64 or any(c not in "0123456789abcdefABCDEF" for c in token):
        fail(f"malformed sha256 sidecar {path}")
    return token.lower()


def rpix_manifest(rpix: Path, expected_name: str) -> dict:
    try:
        with tarfile.open(rpix, "r:gz") as archive:
            member = archive.getmember("rpi-extension.json")
            if not member.isfile():
                fail(f"{rpix}: rpi-extension.json is not a regular file")
            handle = archive.extractfile(member)
            if handle is None:
                fail(f"{rpix}: cannot read rpi-extension.json")
            manifest = json.load(handle)
    except KeyError:
        fail(f"{rpix}: missing rpi-extension.json")
    except (tarfile.TarError, OSError, json.JSONDecodeError) as exc:
        fail(f"{rpix}: cannot read manifest: {exc}")
    if not isinstance(manifest, dict):
        fail(f"{rpix}: rpi-extension.json must be an object")
    if manifest.get("name") != expected_name:
        fail(f"{rpix}: manifest name {manifest.get('name')!r} != {expected_name!r}")
    version = manifest.get("version")
    if not isinstance(version, str) or not version:
        fail(f"{rpix}: manifest version is missing")
    defaults = manifest.get(
        "defaultSettings", manifest.get("defaultSettingsJson", {})
    )
    if defaults is None:
        defaults = {}
    if not isinstance(defaults, dict):
        fail(f"{rpix}: defaultSettings must be an object")
    return {"name": expected_name, "version": version, "defaultSettings": defaults}


def collect_profile(
    dist: Path, version: str, target: str, plugins: list[str]
) -> tuple[list[Path], bytes]:
    """Return the ordered .rpix paths and the generated settings.defaults.json."""
    rpix_paths: list[Path] = []
    merged: dict[str, object] = {}
    for plugin in plugins:
        basename = f"{plugin}-{version}-{target}.rpix"
        rpix = dist / basename
        sidecar = dist / f"{basename}.sha256"
        if not rpix.is_file():
            fail(f"missing plugin archive {rpix}")
        if not sidecar.is_file():
            fail(f"missing plugin sidecar {sidecar}")
        expected = read_sidecar_sha256(sidecar)
        actual = sha256_file(rpix)
        if actual != expected:
            fail(f"plugin sidecar mismatch for {rpix}: {actual} != {expected}")
        manifest = rpix_manifest(rpix, plugin)
        if manifest["version"] != version:
            fail(
                f"{rpix}: manifest version {manifest['version']!r} != release {version!r}"
            )
        for key, value in manifest["defaultSettings"].items():
            if key == "packages":
                fail(f"{rpix}: defaultSettings must not carry the packages key")
            merged.setdefault(key, value)
        rpix_paths.append(rpix)
        rpix_paths.append(sidecar)

    defaults: dict[str, object] = {"packages": list(plugins)}
    defaults.update(merged)
    text = json.dumps(defaults, indent=2, ensure_ascii=False) + "\n"
    return rpix_paths, text.encode("utf-8")


def archive_members(
    binary: Path, settings: bytes | None, plugins: list[Path]
) -> list[tuple[str, bytes, int]]:
    """(archive name, content, mode) triples; settings precedes extensions."""
    binary_name = "rpi.exe" if binary.suffix.lower() == ".exe" else "rpi"
    members: list[tuple[str, bytes, int]] = [
        (binary_name, binary.read_bytes(), EXEC_MODE)
    ]
    if settings is not None:
        members.append(("settings.defaults.json", settings, FILE_MODE))
        for plugin in plugins:
            members.append((f"extensions/{plugin.name}", plugin.read_bytes(), FILE_MODE))
    return members


def write_tar_gz(path: Path, members: list[tuple[str, bytes, int]]) -> None:
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.PAX_FORMAT) as archive:
        for name, content, mode in members:
            info = tarfile.TarInfo(name)
            info.size = len(content)
            info.mtime = TAR_MTIME
            info.uid = 0
            info.gid = 0
            info.uname = ""
            info.gname = ""
            info.mode = mode
            archive.addfile(info, io.BytesIO(content))
    with path.open("wb") as handle:
        with gzip.GzipFile(fileobj=handle, mode="wb", mtime=0) as gz:
            gz.write(raw.getvalue())


def write_zip(path: Path, members: list[tuple[str, bytes, int]]) -> None:
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for name, content, mode in members:
            info = zipfile.ZipInfo(name, date_time=ZIP_DATE_TIME)
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = (0o100000 | mode) << 16
            archive.writestr(info, content)


def write_archive(
    dist: Path, archive_name: str, fmt: str, members: list[tuple[str, bytes, int]]
) -> Path:
    path = dist / archive_name
    if fmt == "tar.gz":
        write_tar_gz(path, members)
    else:
        write_zip(path, members)
    sidecar = dist / f"{archive_name}.sha256"
    # Basename only: install.sh verifies from a directory where the archive
    # lives under its basename (a dist/ prefix would break the check).
    sidecar.write_text(f"{sha256_file(path)}  {archive_name}\n", encoding="utf-8")
    return path


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Assemble install-profile archives for one release target."
    )
    parser.add_argument("--dist", required=True, type=Path, help="dist/ directory")
    parser.add_argument("--version", required=True, help="release version, e.g. 0.1.8")
    parser.add_argument("--target", required=True, help="release target triple")
    parser.add_argument(
        "--binary", required=True, type=Path, help="binary to place at the archive root"
    )
    parser.add_argument(
        "--format",
        choices=RELEASE_FORMATS,
        default="tar.gz",
        help="archive format (zip on Windows)",
    )
    parser.add_argument(
        "--profiles",
        nargs="+",
        choices=("standard", "full"),
        default=["standard", "full"],
        help="profiles to build (default: both)",
    )
    parser.add_argument(
        "--static",
        action="store_true",
        help="build the musl <target>-static fallback archive instead of profiles",
    )
    args = parser.parse_args()

    if "/" in args.version or args.version in ("", ".", ".."):
        fail(f"invalid version {args.version!r}")
    binary = args.binary
    if not binary.is_file():
        fail(f"binary not found: {binary}")
    if not os.access(binary, os.R_OK):
        fail(f"binary is not readable: {binary}")
    dist = args.dist
    dist.mkdir(parents=True, exist_ok=True)

    if args.static:
        if args.format != "tar.gz":
            fail("the static fallback is published as .tar.gz only")
        archive_name = f"rpi-{args.version}-{args.target}{STATIC_SUFFIX}.tar.gz"
        members = archive_members(binary, None, [])
        write_archive(dist, archive_name, "tar.gz", members)
        print(archive_name)
        return 0

    profiles = load_profiles()
    for profile in args.profiles:
        plugins = profiles[profile]
        plugin_paths, settings = collect_profile(
            dist, args.version, args.target, plugins
        )
        members = archive_members(binary, settings, plugin_paths)
        archive_name = f"rpi-{args.version}-{args.target}-{profile}.{args.format}"
        write_archive(dist, archive_name, args.format, members)
        print(archive_name)
    return 0


if __name__ == "__main__":
    sys.exit(main())