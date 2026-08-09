#!/usr/bin/env python3
"""Generate the resetpi.com static site from rpi's built-in provider data.

Run from the repository root:

    python3 deploy/resetpi/scripts/generate-site.py

Output (Cloudflare Pages project rooted at deploy/resetpi/):

- api/models/providers/{providerId}.json — one catalog per built-in provider.
  The rpi client fetches `GET {catalogBaseUrl}/api/models/providers/{id}`
  (remote_catalog_provider.rs): the body may be a bare array,
  `{"models": [...]}` or a keyed object of Model objects carrying `id` —
  this script emits `{"models": [...]}`. The data files are nested
  `{apiKind: {modelId: Model}}`, so the model objects are flattened.
- api/latest-version.json — `{"version","packageName","note"}` consumed by
  the version check (version_check.rs: `version` is required, non-2xx or a
  missing version means "no update"). Defaults to the workspace version;
  override with --version when publishing a release.

Static assets get ETag / Last-Modified from Cloudflare Pages automatically,
so the client's If-None-Match revalidation (4h window) works as-is.
"""
import argparse
import json
import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
DATA = REPO / "crates/rpi-ai/src/providers/data"
SITE = REPO / "deploy/resetpi"
OUT_PROVIDERS = SITE / "api/models/providers"
OUT_VERSION = SITE / "api/latest-version.json"

PACKAGE_NAME = "rpi"


def workspace_version() -> str:
    text = (REPO / "Cargo.toml").read_text(encoding="utf-8")
    match = re.search(r'^version\s*=\s*"([^"]+)"', text, re.MULTILINE)
    if not match:
        raise SystemExit("workspace version not found in Cargo.toml")
    return match.group(1)


def generate_catalogs() -> list[str]:
    OUT_PROVIDERS.mkdir(parents=True, exist_ok=True)
    generated = []
    for data_file in sorted(DATA.glob("*.json")):
        if data_file.name.startswith("."):
            continue  # .manifest.json — the catalog manifest, not a provider
        provider_id = data_file.stem  # kebab-case, matches the client's encodeURIComponent'd id
        payload = json.loads(data_file.read_text(encoding="utf-8"))
        models = []
        for api_models in payload.values():  # api kind -> {modelId: Model}
            models.extend(api_models.values())
        (OUT_PROVIDERS / f"{provider_id}.json").write_text(
            json.dumps({"models": models}, ensure_ascii=False, indent=2),
            encoding="utf-8",
        )
        generated.append(provider_id)
    return generated


def generate_latest_version(version: str, note: str | None) -> None:
    OUT_VERSION.parent.mkdir(parents=True, exist_ok=True)
    payload = {"version": version, "packageName": PACKAGE_NAME}
    if note:
        payload["note"] = note
    OUT_VERSION.write_text(
        json.dumps(payload, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--version",
        default=None,
        help="published version for api/latest-version.json (default: workspace Cargo.toml version)",
    )
    parser.add_argument(
        "--note",
        default=None,
        help="optional release note shown by the client's update banner",
    )
    args = parser.parse_args()

    generated = generate_catalogs()
    version = args.version or workspace_version()
    generate_latest_version(version, args.note)
    print(f"catalogs: {len(generated)} providers under {OUT_PROVIDERS.relative_to(REPO)}")
    print(f"version:  {OUT_VERSION.relative_to(REPO)} -> v{version}")
    print("deploy:   npx wrangler pages deploy deploy/resetpi --project-name=resetpi")
    return 0


if __name__ == "__main__":
    sys.exit(main())
