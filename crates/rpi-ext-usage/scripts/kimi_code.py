#!/usr/bin/env python3
"""rpi-usage provider script: Kimi Code quota (TE44).

Contract (host framework, V16-05 §7.2):
- stdin:  {"provider", "baseUrl"?, "apiKeyEnv"?, "model"?}
- stdout: the schemaVersion=1 usage envelope (single JSON object)
- errors: a message on stderr + non-zero exit (the host degrades silently)

Endpoint (pinned from the TE44 live capture, 2026-10-05):
  GET https://api.kimi.com/coding/v1/usages
The Kimi Code console key (`sk-kimi-*`) goes in `x-api-key`; OAuth access
tokens use `Authorization: Bearer` (both shapes are tried).

Response (live):
  {"usage": {"limit": "100", "remaining": "100", "resetTime": "<ISO>"},
   "limits": [{"window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"},
               "detail": {"limit": "100", "remaining": "100",
                          "resetTime": "<ISO>"}}],
   "usages": {"limit_5h": {"used_ratio": 0.0, "reset_time": "<ISO>"},
              "limit_7d": {"used_ratio": 0.0, "reset_time": "<ISO>"}}}
`usage` is the Code 7-day quota; each `limits[]` entry is a rolling window
(300 minutes = 5-hour). Counts ship as strings; ratios are 0..1 used.

Credential resolution order (never printed): the context `apiKeyEnv`, the
conventional environment variables below, then the rpi credential store
`<RPI_CODING_AGENT_DIR|~/.rpi/agent>/auth.json` (read-only, api_key entries;
`$VAR` references resolve through the environment, command references are
skipped). The Kimi Code OAuth login performed through rpi lands in the same
store under `kimi-coding`; this script consumes the `key` of api_key
entries only.

Tests: crates/rpi-ext-usage/tests/scripts_contract.rs drives this script
against a local stub HTTP server with recorded fixtures (no network).
"""

import json
import os
import ssl
import sys
import urllib.error
import urllib.parse
import urllib.request

PROVIDER = "kimi-code"
ENV_NAMES = ("KIMI_API_KEY", "KIMI_CODING_API_KEY")
AUTH_IDS = ("kimi-coding",)
DEFAULT_BASE = "https://api.kimi.com/coding/v1"
USAGES_SUFFIX = "/usages"
DEFAULT_TIMEOUT_MS = 8000
USER_AGENT = "rpi-usage/0.1.0"


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
        return None
    for provider_id in AUTH_IDS:
        entry = data.get(provider_id)
        if not isinstance(entry, dict) or entry.get("type") != "api_key":
            continue
        key = entry.get("key")
        if not isinstance(key, str) or not key.strip():
            continue
        key = key.strip()
        if key.startswith("$"):
            env_name = key[1:].strip()
            if env_name.startswith("{") and env_name.endswith("}"):
                env_name = env_name[1:-1].strip()
            value = os.environ.get(env_name)
            if value:
                return value
            continue
        if key.startswith("!"):
            continue
        return key
    return None


def resolve_key(context):
    names = []
    if context.get("apiKeyEnv"):
        names.append(str(context["apiKeyEnv"]))
    names.extend(ENV_NAMES)
    for name in names:
        value = os.environ.get(name)
        if value and value.strip():
            return value.strip(), name
    key = _auth_store_key()
    if key:
        return key, "auth.json:kimi-coding"
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


def usages_url(base):
    base = (base or DEFAULT_BASE).strip().rstrip("/")
    if base.endswith(USAGES_SUFFIX):
        return base
    if base.endswith("/coding/v1"):
        return base + USAGES_SUFFIX
    if base.endswith("/coding"):
        return base + "/v1" + USAGES_SUFFIX
    return base + "/coding/v1" + USAGES_SUFFIX


def number(value):
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, str):
        try:
            return float(value.strip())
        except ValueError:
            return None
    return None


def ratio(value):
    parsed = number(value)
    if parsed is None:
        return None
    if parsed <= 1.0:
        parsed *= 100.0
    return max(0.0, min(100.0, parsed))


def window_label(window):
    if not isinstance(window, dict):
        return "5h"
    duration = number(window.get("duration"))
    unit = str(window.get("timeUnit") or "").lower().removeprefix("time_unit_").rstrip("s")
    if duration is None or duration <= 0:
        return "5h"
    if unit == "minute" and duration >= 60 and duration % 60 == 0:
        duration /= 60.0
        unit = "hour"
    count = int(duration)
    if unit == "hour":
        return f"{count}h"
    if unit == "day":
        return f"{count}d"
    if unit == "minute":
        return f"{count}m"
    return "5h"


def count_used(detail):
    limit = number(detail.get("limit"))
    used = number(detail.get("used"))
    remaining = number(detail.get("remaining"))
    if limit is None or limit <= 0:
        return None
    if used is None:
        if remaining is None:
            return None
        used = limit - remaining
    return max(0.0, min(limit, used))


def count_percent(detail):
    limit = number(detail.get("limit"))
    used = count_used(detail)
    if limit is None or limit <= 0 or used is None:
        return None
    return max(0.0, min(100.0, used / limit * 100.0))


def plan_name(data):
    user = data.get("user")
    membership = user.get("membership") if isinstance(user, dict) else None
    level = membership.get("level") if isinstance(membership, dict) else None
    if isinstance(level, str) and level.strip():
        cleaned = level.strip().removeprefix("LEVEL_").replace("_", " ").lower()
        return cleaned.title() or "Kimi Code"
    return "Kimi Code"


def fmt(value):
    if value is None:
        return "n/a"
    return f"{value:.0f}" if abs(value - round(value)) < 0.05 else f"{value:.1f}"


def main():
    context = read_context()
    key, _source = resolve_key(context)
    if not key:
        print(
            f"{PROVIDER}: no API key; set one of {', '.join(ENV_NAMES)} "
            "(or log in with rpi)",
            file=sys.stderr,
        )
        return 1

    url = usages_url(context.get("baseUrl"))
    candidates = [
        {"x-api-key": key},
        {"Authorization": "Bearer " + key},
    ]
    status = 0
    body = ""
    for headers in candidates:
        status, body = http_get(
            url,
            {**headers, "Accept": "application/json", "User-Agent": USER_AGENT},
            timeout_seconds(),
        )
        if status == 200:
            break
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

    usages = data.get("usages") if isinstance(data.get("usages"), dict) else {}
    weekly_detail = data.get("usage") if isinstance(data.get("usage"), dict) else {}
    weekly_pct = count_percent(weekly_detail)
    if weekly_pct is None:
        limit_7d = usages.get("limit_7d")
        weekly_pct = ratio(
            limit_7d.get("used_ratio") if isinstance(limit_7d, dict) else None
        )

    limits = data.get("limits") if isinstance(data.get("limits"), list) else []
    first = limits[0] if limits and isinstance(limits[0], dict) else {}
    five_label = window_label(first.get("window"))
    five_detail = first.get("detail") if isinstance(first.get("detail"), dict) else {}
    five_pct = count_percent(five_detail)
    if five_pct is None:
        limit_5h = usages.get("limit_5h")
        five_pct = ratio(
            limit_5h.get("used_ratio") if isinstance(limit_5h, dict) else None
        )

    if five_pct is None and weekly_pct is None:
        print(f"{PROVIDER}: response carries no recognized windows", file=sys.stderr)
        return 1

    parts = []
    if five_pct is not None:
        parts.append(f"{five_label} {fmt(five_pct)}% used")
    if weekly_pct is not None:
        parts.append(f"7d {fmt(weekly_pct)}% used")
    display = f"{PROVIDER}: " + " · ".join(parts)

    envelope = {
        "schemaVersion": 1,
        "provider": PROVIDER,
        "plan": plan_name(data),
        "displayText": display,
    }
    limit = number(weekly_detail.get("limit"))
    remaining = number(weekly_detail.get("remaining"))
    used_count = count_used(weekly_detail)
    if weekly_pct is not None and limit is not None and limit > 0 and used_count is not None:
        quota = {"used": used_count, "total": limit, "unit": "requests"}
        if remaining is not None:
            quota["remaining"] = remaining
        envelope["quota"] = quota
        envelope["used"] = round(weekly_pct, 4)
        reset = weekly_detail.get("resetTime")
        if isinstance(reset, str) and reset.strip():
            envelope["resetAt"] = reset.strip()
    elif weekly_pct is not None:
        envelope["used"] = round(weekly_pct, 4)
        reset = weekly_detail.get("resetTime")
        if isinstance(reset, str) and reset.strip():
            envelope["resetAt"] = reset.strip()

    sys.stdout.write(json.dumps(envelope, separators=(",", ":")))
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(130)