#!/usr/bin/env python3
"""rpi-usage provider script: MiniMax Token Plan quota (TE44).

Contract (host framework, V16-05 §7.2):
- stdin:  {"provider", "baseUrl"?, "apiKeyEnv"?, "model"?}
- stdout: the schemaVersion=1 usage envelope (single JSON object)
- errors: a message on stderr + non-zero exit (the host degrades silently)

Endpoint (pinned from the TE44 live capture, 2026-10-05):
- international: GET https://api.minimax.io/v1/token_plan/remains
- China:         GET https://api.minimaxi.com/v1/token_plan/remains
with `Authorization: Bearer <key>`. The region follows the context baseUrl
when given, else the credential source (`MINIMAX_CN_API_KEY` -> China).

Response (live):
  {"base_resp": {"status_code": 0, "status_msg": "success"},
   "model_remains": [{
     "model_name": "general",
     "start_time": <epoch ms>, "end_time": <epoch ms>,
     "current_interval_remaining_percent": 99,   # 0..100 remaining
     "current_interval_status": 1,               # 1 normal, 2 exhausted, 3 unlimited
     "current_interval_total_count": 0, "current_interval_usage_count": 0,
     "weekly_start_time": <epoch ms>, "weekly_end_time": <epoch ms>,
     "current_weekly_remaining_percent": 100,
     "current_weekly_status": 3,
     "current_weekly_total_count": 0, "current_weekly_usage_count": 0}]}
HTTP stays 200 for rejected credentials: `base_resp.status_code` is the real
success signal (1001 = no token plan).

Credential resolution order (never printed): the context `apiKeyEnv`, the
conventional environment variables below, then the rpi credential store
`<RPI_CODING_AGENT_DIR|~/.rpi/agent>/auth.json` (read-only, api_key entries;
`$VAR` references resolve through the environment, command references are
skipped).

Tests: crates/rpi-ext-usage/tests/scripts_contract.rs drives this script
against a local stub HTTP server with recorded fixtures (no network).
"""

import datetime
import json
import os
import ssl
import sys
import urllib.error
import urllib.parse
import urllib.request

PROVIDER = "minimax-token-plan"
ENV_NAMES = ("MINIMAX_API_KEY", "MINIMAX_CN_API_KEY")
AUTH_IDS = ("minimax", "minimax-cn")
INTL_BASE = "https://api.minimax.io"
CN_BASE = "https://api.minimaxi.com"
REMAINS_PATH = "/v1/token_plan/remains"
DEFAULT_TIMEOUT_MS = 8000
USER_AGENT = "rpi-usage/0.1.0"

STATUS_NORMAL = 1
STATUS_EXHAUSTED = 2
STATUS_UNLIMITED = 3


def read_context():
    raw = sys.stdin.buffer.read().decode("utf-8", "replace").strip()
    if not raw:
        return {}
    try:
        value = json.loads(raw)
    except ValueError:
        return {}
    return value if isinstance(value, dict) else {}


def _auth_store_key():
    base = os.environ.get("RPI_CODING_AGENT_DIR")
    if not base:
        base = os.path.join(os.path.expanduser("~"), ".rpi", "agent")
    try:
        with open(os.path.join(base, "auth.json"), encoding="utf-8") as handle:
            data = json.load(handle)
    except (OSError, ValueError):
        return None, None
    for provider_id in AUTH_IDS:
        entry = data.get(provider_id)
        if not isinstance(entry, dict) or entry.get("type") != "api_key":
            continue
        key = entry.get("key")
        if not isinstance(key, str) or not key.strip():
            continue
        key = key.strip()
        if key.startswith("$"):
            value = os.environ.get(key[1:].strip())
            if value:
                return value, provider_id
            continue
        if key.startswith("!"):
            continue
        return key, provider_id
    return None, None


def resolve_key(context):
    names = []
    if context.get("apiKeyEnv"):
        names.append(str(context["apiKeyEnv"]))
    names.extend(ENV_NAMES)
    for name in names:
        value = os.environ.get(name)
        if value and value.strip():
            return value.strip(), name
    key, provider_id = _auth_store_key()
    if key:
        return key, "auth.json:" + provider_id
    return None, None


def timeout_seconds():
    raw = os.environ.get("RPI_USAGE_TIMEOUT_MS")
    if raw:
        try:
            return max(0.5, min(60.0, int(raw) / 1000.0))
        except ValueError:
            pass
    return DEFAULT_TIMEOUT_MS / 1000.0


def http_get(url, headers, timeout):
    request = urllib.request.Request(url, headers=headers, method="GET")
    try:
        with urllib.request.urlopen(  # noqa: S310 - fixed https endpoint
            request, timeout=timeout, context=ssl.create_default_context()
        ) as response:
            return response.status, response.read(262144).decode("utf-8", "replace")
    except urllib.error.HTTPError as error:
        return error.code, error.read(65536).decode("utf-8", "replace")
    except (urllib.error.URLError, TimeoutError, OSError):
        return 0, ""


def remains_url(base, source):
    base = (base or "").strip().rstrip("/")
    if base.endswith(REMAINS_PATH):
        return base
    host = (urllib.parse.urlparse(base).hostname or "").lower()
    if host.endswith("minimaxi.com"):
        return CN_BASE + REMAINS_PATH
    if host.endswith("minimax.io"):
        return INTL_BASE + REMAINS_PATH
    if base:
        return base + REMAINS_PATH
    return (CN_BASE if "cn" in (source or "").lower() else INTL_BASE) + REMAINS_PATH


def iso_reset(value):
    if not isinstance(value, (int, float)):
        return None
    seconds = value / 1000.0 if value > 1e11 else float(value)
    return datetime.datetime.fromtimestamp(
        seconds, tz=datetime.timezone.utc
    ).strftime("%Y-%m-%dT%H:%M:%SZ")


def window(bucket, prefix):
    """(used_percent | None, unlimited) for one window of a bucket."""
    status = bucket.get(f"current_{prefix}_status")
    if status == STATUS_UNLIMITED:
        return None, True
    if status == STATUS_EXHAUSTED:
        return 100.0, False
    remaining = bucket.get(f"current_{prefix}_remaining_percent")
    if not isinstance(remaining, (int, float)):
        return None, False
    return max(0.0, min(100.0, 100.0 - float(remaining))), False


def interval_label(bucket):
    start = bucket.get("start_time")
    end = bucket.get("end_time")
    if isinstance(start, (int, float)) and isinstance(end, (int, float)) and end > start:
        hours = (end - start) / 3_600_000.0
        if hours >= 1 and abs(hours - round(hours)) < 0.05:
            return f"{round(hours)}h"
        minutes = (end - start) / 60_000.0
        return f"{round(minutes)}m"
    return "5h"


def fmt(value):
    if value is None:
        return "n/a"
    return f"{value:.0f}" if abs(value - round(value)) < 0.05 else f"{value:.1f}"


def main():
    context = read_context()
    key, source = resolve_key(context)
    if not key:
        print(
            f"{PROVIDER}: no API key; set one of {', '.join(ENV_NAMES)} "
            "(or log in with rpi)",
            file=sys.stderr,
        )
        return 1

    url = remains_url(context.get("baseUrl"), source)
    status, body = http_get(
        url,
        {
            "Authorization": "Bearer " + key,
            "Accept": "application/json",
            "User-Agent": USER_AGENT,
        },
        timeout_seconds(),
    )
    if status == 0:
        print(f"{PROVIDER}: request failed (timeout or transport error)", file=sys.stderr)
        return 1
    if status != 200:
        print(f"{PROVIDER}: endpoint answered HTTP {status}", file=sys.stderr)
        return 1
    try:
        data = json.loads(body)
    except ValueError:
        print(f"{PROVIDER}: endpoint answered non-JSON", file=sys.stderr)
        return 1
    if not isinstance(data, dict):
        print(f"{PROVIDER}: unexpected response shape", file=sys.stderr)
        return 1
    base_resp = data.get("base_resp")
    status_code = base_resp.get("status_code") if isinstance(base_resp, dict) else None
    if isinstance(status_code, (int, float)) and status_code != 0:
        print(
            f"{PROVIDER}: endpoint answered status {status_code:.0f}",
            file=sys.stderr,
        )
        return 1
    remains = data.get("model_remains")
    if not isinstance(remains, list) or not remains:
        print(f"{PROVIDER}: response carries no model_remains", file=sys.stderr)
        return 1

    bucket = None
    for entry in remains:
        if isinstance(entry, dict) and entry.get("model_name") == "general":
            bucket = entry
            break
    if bucket is None:
        bucket = next((entry for entry in remains if isinstance(entry, dict)), None)
    if bucket is None:
        print(f"{PROVIDER}: response carries no usable bucket", file=sys.stderr)
        return 1

    interval_pct, interval_unlimited = window(bucket, "interval")
    weekly_pct, weekly_unlimited = window(bucket, "weekly")
    label = interval_label(bucket)

    interval_text = "unlimited" if interval_unlimited else f"{fmt(interval_pct)}% used"
    weekly_text = "unlimited" if weekly_unlimited else f"{fmt(weekly_pct)}% used"
    display = f"{PROVIDER}: {label} {interval_text} · 7d {weekly_text}"

    envelope = {
        "schemaVersion": 1,
        "provider": PROVIDER,
        "plan": "Token Plan",
        "displayText": display,
    }
    if weekly_pct is not None and not weekly_unlimited:
        envelope["quota"] = {
            "used": round(weekly_pct, 4),
            "total": 100.0,
            "remaining": round(max(0.0, 100.0 - weekly_pct), 4),
            "unit": "%",
        }
        envelope["used"] = round(weekly_pct, 4)
        reset = iso_reset(bucket.get("weekly_end_time"))
        if reset:
            envelope["resetAt"] = reset
    elif interval_pct is not None and not interval_unlimited:
        envelope["quota"] = {
            "used": round(interval_pct, 4),
            "total": 100.0,
            "remaining": round(max(0.0, 100.0 - interval_pct), 4),
            "unit": "%",
        }
        envelope["used"] = round(interval_pct, 4)
        reset = iso_reset(bucket.get("end_time"))
        if reset:
            envelope["resetAt"] = reset

    sys.stdout.write(json.dumps(envelope, separators=(",", ":")))
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(130)