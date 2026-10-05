#!/usr/bin/env python3
"""rpi-usage provider script: GLM Coding Plan quota (TE44).

Contract (host framework, V16-05 §7.2):
- stdin:  {"provider", "baseUrl"?, "apiKeyEnv"?, "model"?}
- stdout: the schemaVersion=1 usage envelope (single JSON object)
- errors: a message on stderr + non-zero exit (the host degrades silently)

Endpoint (pinned from the TE44 live capture, 2026-10-05):
- z.ai (international): GET https://api.z.ai/api/monitor/usage/quota/limit
- BigModel (China):     GET https://open.bigmodel.cn/api/monitor/usage/quota/limit
Auth ships in two community-observed shapes; the script tries the historical
one for the host first and falls back on 401/403 (z.ai: bare `Authorization`,
BigModel: `Authorization: Bearer`).

Response (live):
  {"code": 200, "success": true, "msg": "...", "data": {
     "level": "max",
     "limits": [
       {"type": "TOKENS_LIMIT", "unit": 3, "number": 5, "percentage": 0},
       {"type": "TOKENS_LIMIT", "unit": 6, "number": 1, "percentage": 9,
        "nextResetTime": <epoch ms>},
       {"type": "TIME_LIMIT", "unit": 5, "number": 1, "percentage": 22,
        "currentValue": 224, "usage": 1000, "remaining": 776,
        "nextResetTime": <epoch ms>, "usageDetails": [...]}
     ]}}
Windows: unit 3/number 5 = 5-hour, unit 6/number 1 = weekly; the
`TIME_LIMIT` unit 5/number 1 entry is the monthly MCP-tools bucket.
`percentage` is used percent; `currentValue`/`usage` is the fallback.

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

PROVIDER = "glm-coding-plan"
ENV_NAMES = ("ZAI_API_KEY", "GLM_API_KEY", "ZAI_CODING_CN_API_KEY")
AUTH_IDS = ("zai", "zai-coding-cn")
DEFAULT_BASE = "https://api.z.ai"
QUOTA_PATH = "/api/monitor/usage/quota/limit"
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
            value = os.environ.get(key[1:].strip())
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
            return value.strip()
    return _auth_store_key()


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


def quota_url(base):
    base = (base or DEFAULT_BASE).strip().rstrip("/")
    if base.endswith(QUOTA_PATH):
        return base
    host = (urllib.parse.urlparse(base).hostname or "").lower()
    if host.endswith("bigmodel.cn"):
        return "https://open.bigmodel.cn" + QUOTA_PATH
    if host.endswith("z.ai"):
        return "https://api.z.ai" + QUOTA_PATH
    return base + QUOTA_PATH


def auth_candidates(url, key):
    """(label, headers) attempts in host-appropriate order."""
    host = (urllib.parse.urlparse(url).hostname or "").lower()
    bearer = {"Authorization": "Bearer " + key}
    bare = {"Authorization": key}
    if host.endswith("z.ai"):
        candidates = [("bare", bare), ("bearer", bearer)]
    else:
        candidates = [("bearer", bearer), ("bare", bare)]
    return [
        (label, {**headers, "Accept": "application/json", "User-Agent": USER_AGENT})
        for label, headers in candidates
    ]


def iso_reset(value):
    if not isinstance(value, (int, float)):
        return None
    seconds = value / 1000.0 if value > 1e11 else float(value)
    return datetime.datetime.fromtimestamp(
        seconds, tz=datetime.timezone.utc
    ).strftime("%Y-%m-%dT%H:%M:%SZ")


def percent(limit):
    raw = limit.get("percentage")
    if isinstance(raw, (int, float)):
        return max(0.0, min(100.0, float(raw)))
    current = limit.get("currentValue")
    total = limit.get("usage")
    if isinstance(current, (int, float)) and isinstance(total, (int, float)) and total > 0:
        return max(0.0, min(100.0, float(current) / float(total) * 100.0))
    return None


def classify(limits):
    five_hour = weekly = mcp = None
    for limit in limits:
        if not isinstance(limit, dict):
            continue
        kind = limit.get("type")
        unit = limit.get("unit")
        number = limit.get("number")
        if kind == "TIME_LIMIT" and unit == 5 and number == 1:
            mcp = limit
        elif unit == 3 and number == 5:
            five_hour = limit
        elif unit == 6 and number == 1:
            weekly = limit
    return five_hour, weekly, mcp


def fmt(value):
    if value is None:
        return "n/a"
    return f"{value:.0f}" if abs(value - round(value)) < 0.05 else f"{value:.1f}"


def main():
    context = read_context()
    key = resolve_key(context)
    if not key:
        print(
            f"{PROVIDER}: no API key; set one of {', '.join(ENV_NAMES)} "
            "(or log in with rpi)",
            file=sys.stderr,
        )
        return 1

    url = quota_url(context.get("baseUrl"))
    status = 0
    body = ""
    last_error = None
    for _label, headers in auth_candidates(url, key):
        status, body = http_get(url, headers, timeout_seconds())
        if status == 200:
            last_error = None
            break
        last_error = status
    if last_error == 0:
        print(f"{PROVIDER}: request failed (timeout or transport error)", file=sys.stderr)
        return 1
    if last_error is not None:
        print(f"{PROVIDER}: endpoint answered HTTP {last_error}", file=sys.stderr)
        return 1
    try:
        data = json.loads(body)
    except ValueError:
        print(f"{PROVIDER}: endpoint answered non-JSON", file=sys.stderr)
        return 1
    if not isinstance(data, dict) or data.get("success") is False:
        print(f"{PROVIDER}: endpoint rejected the request", file=sys.stderr)
        return 1
    code = data.get("code")
    if isinstance(code, (int, float)) and code not in (0, 200):
        print(f"{PROVIDER}: endpoint answered code {code:.0f}", file=sys.stderr)
        return 1

    payload = data.get("data")
    payload = payload if isinstance(payload, dict) else data
    limits = payload.get("limits")
    if not isinstance(limits, list):
        print(f"{PROVIDER}: response carries no limits", file=sys.stderr)
        return 1
    five_hour, weekly, mcp = classify(limits)
    if five_hour is None and weekly is None and mcp is None:
        print(f"{PROVIDER}: response carries no recognized windows", file=sys.stderr)
        return 1

    five_pct = percent(five_hour) if five_hour else None
    weekly_pct = percent(weekly) if weekly else None
    mcp_pct = percent(mcp) if mcp else None

    parts = []
    if five_pct is not None:
        parts.append(f"5h {fmt(five_pct)}% used")
    if weekly_pct is not None:
        parts.append(f"7d {fmt(weekly_pct)}% used")
    if mcp_pct is not None:
        parts.append(f"MCP {fmt(mcp_pct)}% used")
    display = f"{PROVIDER}: " + " · ".join(parts)

    envelope = {
        "schemaVersion": 1,
        "provider": PROVIDER,
        "displayText": display,
    }
    level = payload.get("level")
    if isinstance(level, str) and level.strip():
        envelope["plan"] = level.strip()
    primary = weekly or five_hour
    primary_pct = weekly_pct if weekly is not None else five_pct
    if primary is not None and primary_pct is not None:
        envelope["quota"] = {
            "used": round(primary_pct, 4),
            "total": 100.0,
            "remaining": round(max(0.0, 100.0 - primary_pct), 4),
            "unit": "%",
        }
        envelope["used"] = round(primary_pct, 4)
        reset = iso_reset(primary.get("nextResetTime"))
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