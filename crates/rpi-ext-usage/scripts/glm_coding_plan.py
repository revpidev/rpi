#!/usr/bin/env python3
"""rpi-usage provider script: GLM Coding Plan quota (TE44 / TE53).

Contract (host framework, V16-05 §7.2):
- stdin:  {"provider", "baseUrl"?, "apiKeyEnv"?, "model"?, "credentialKind"?}
- stdout: the schemaVersion=1 usage envelope (single JSON object)
- errors: exit 2 + `{"schemaVersion":1,"provider":...,"error":{"kind":...}}`
  for a classified failure; a short sanitized line on stderr is diagnostics
- displayText (TE50): compact windows "<label> <pct>%(~<reset-in>)" joined
  by " / ", no provider prefix; unchanged byte-for-byte for the recorded
  fixtures so old hosts keep working
- typed windows[] (04 §4.1): ids 5h / 7d / mcp with exactly one of a
  percentage or counted representation, a required status and an optional
  RFC3339 resetAt/durationSeconds.

Endpoint (pinned from the TE44 live capture, 2026-10-05; fixed origins per
TE53 04 §5.1 — the stdin `baseUrl` is never used to derive an endpoint):
- z.ai (international): GET https://api.z.ai/api/monitor/usage/quota/limit
- BigModel (China):     GET https://open.bigmodel.cn/api/monitor/usage/quota/limit
The region follows the resolved credential source (`ZAI_CODING_CN_API_KEY` /
the `zai-coding-cn` auth entry use BigModel), never a custom model URL.
Auth ships in two community-observed shapes; the script tries the historical
one for the host first and falls back on an HTTP error (z.ai: bare
`Authorization`, BigModel: `Authorization: Bearer`).

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
`percentage` is used percent; `currentValue`/`usage`/`remaining` are the
counted fallback (unit "requests" for TIME_LIMIT, "tokens" for TOKENS_LIMIT).

Endpoint policy: redirects refused, 262144-byte body cap, request timeout.
The test-only loopback override (`RPI_USAGE_TEST_MODE=1` +
`RPI_USAGE_TEST_BASE_URL` loopback + a `test-` credential) replaces the
origin and keeps the path; nothing else changes.

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
import math
import os
import socket
import ssl
import sys
import urllib.error
import urllib.parse
import urllib.request

PROVIDER = "glm-coding-plan"
ENV_NAMES = ("ZAI_API_KEY", "GLM_API_KEY", "ZAI_CODING_CN_API_KEY")
AUTH_IDS = ("zai", "zai-coding-cn")
INTL_BASE = "https://api.z.ai"
CN_BASE = "https://open.bigmodel.cn"
QUOTA_PATH = "/api/monitor/usage/quota/limit"
DEFAULT_TIMEOUT_MS = 8000
USER_AGENT = "rpi-usage/0.1.0"
MAX_BODY_BYTES = 262144

TEST_MODE_ENV = "RPI_USAGE_TEST_MODE"
TEST_BASE_ENV = "RPI_USAGE_TEST_BASE_URL"
LOOPBACK_HOSTS = ("127.0.0.1", "::1", "localhost")

FIVE_HOURS = 18_000
ONE_WEEK = 604_800


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    """Refuse every redirect: a usage read must not leave its origin."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise urllib.error.HTTPError(req.full_url, code, "redirect refused", headers, fp)


def _opener():
    return urllib.request.build_opener(
        _NoRedirect,
        urllib.request.HTTPSHandler(context=ssl.create_default_context()),
    )


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
            env_name = key[1:].strip()
            if env_name.startswith("{") and env_name.endswith("}"):
                env_name = env_name[1:-1].strip()
            value = os.environ.get(env_name)
            if value:
                return value, "auth.json:" + provider_id
            continue
        if key.startswith("!"):
            continue
        return key, "auth.json:" + provider_id
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
    return _auth_store_key()


def timeout_seconds():
    raw = os.environ.get("RPI_USAGE_TIMEOUT_MS")
    if raw:
        try:
            return max(0.5, min(60.0, int(raw) / 1000.0))
        except ValueError:
            pass
    return DEFAULT_TIMEOUT_MS / 1000.0


def endpoint_url(production_url, credential):
    """Production URL unless the test-only loopback override is active."""
    if os.environ.get(TEST_MODE_ENV) != "1":
        return production_url
    override = os.environ.get(TEST_BASE_ENV, "").strip()
    if not override or not credential or not credential.startswith("test-"):
        return production_url
    parsed = urllib.parse.urlparse(override)
    host = (parsed.hostname or "").lower()
    if parsed.scheme not in ("http", "https") or host not in LOOPBACK_HOSTS:
        return production_url
    if parsed.username or parsed.password:
        return production_url
    target = urllib.parse.urlparse(production_url)
    return urllib.parse.urlunparse((parsed.scheme, parsed.netloc, target.path, "", target.query, ""))


def http_get(url, headers, timeout):
    """GET with redirect refusal; answers (status, body, transport_kind)."""
    request = urllib.request.Request(url, headers=headers, method="GET")
    try:
        with _opener().open(request, timeout=timeout) as response:
            raw = response.read(MAX_BODY_BYTES + 1)
            if len(raw) > MAX_BODY_BYTES:
                return response.status, "", "oversized"
            return response.status, raw.decode("utf-8", "replace"), None
    except urllib.error.HTTPError as error:
        try:
            error.close()
        except OSError:
            pass
        return error.code, "", None
    except (TimeoutError, socket.timeout):
        return 0, "", "timeout"
    except urllib.error.URLError as error:
        if isinstance(error.reason, (TimeoutError, socket.timeout)):
            return 0, "", "timeout"
        return 0, "", "transport"
    except OSError:
        return 0, "", "transport"


def failure_kind(status, transport):
    if transport == "timeout":
        return "timeout"
    if transport == "oversized":
        return "shape-changed"
    if transport:
        return "transport"
    if status in (401, 403):
        return "unauthorized"
    if status == 429:
        return "rate-limited"
    return "transport"


def fail(kind, detail):
    """Classified failure envelope (exit 2) plus a short sanitized stderr line."""
    print(f"{PROVIDER}: {detail}", file=sys.stderr)
    envelope = {"schemaVersion": 1, "provider": PROVIDER, "error": {"kind": kind}}
    sys.stdout.write(json.dumps(envelope, separators=(",", ":")))
    sys.stdout.write("\n")
    return 2


def quota_url(source):
    """Fixed official origin; the region follows the credential source."""
    source = (source or "").lower()
    china = "zai_coding_cn" in source or source.endswith("zai-coding-cn")
    return (CN_BASE if china else INTL_BASE) + QUOTA_PATH


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


def epoch_seconds(value):
    """Epoch seconds from a milliseconds (>1e11) or seconds timestamp."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return value / 1000.0 if value > 1e11 else float(value)


def iso_reset(value):
    seconds = epoch_seconds(value)
    if seconds is None:
        return None
    try:
        return datetime.datetime.fromtimestamp(
            seconds, tz=datetime.timezone.utc
        ).strftime("%Y-%m-%dT%H:%M:%SZ")
    except (OverflowError, OSError, ValueError):
        return None


def now_epoch():
    """Fetch-time clock; the contract tests pin RPI_USAGE_NOW_MS."""
    raw = os.environ.get("RPI_USAGE_NOW_MS")
    if raw:
        try:
            return int(raw) / 1000.0
        except ValueError:
            pass
    return datetime.datetime.now(datetime.timezone.utc).timestamp()


def countdown_annotation(reset_epoch, now):
    """TE50 R17.2: "(~<reset-in>)" at fetch time, at most two units."""
    if reset_epoch is None:
        return ""
    delta = reset_epoch - now
    if delta >= 86400:
        days, hours = int(delta // 86400), int(delta % 86400 // 3600)
        return f"(~{days}d{hours}h)" if hours else f"(~{days}d)"
    if delta >= 3600:
        hours, minutes = int(delta // 3600), int(delta % 3600 // 60)
        return f"(~{hours}h{minutes}m)" if minutes else f"(~{hours}h)"
    return f"(~{max(1, int(delta // 60))}m)"


def finite_number(value):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    if not math.isfinite(float(value)):
        return None
    return float(value)


def valid_percent(value):
    """Finite 0..100 percentage, else None (out-of-range is invalid)."""
    if value is None:
        return None
    return value if 0.0 <= value <= 100.0 else None


def percent(limit):
    """Legacy display percent: `percentage`, else current/usage.

    An out-of-range or non-finite value is invalid (no clamping): a provider
    percentage outside 0..100 is a shape change, not a value to invent.
    """
    raw = valid_percent(finite_number(limit.get("percentage")))
    if raw is not None:
        return raw
    current = finite_number(limit.get("currentValue"))
    total = finite_number(limit.get("usage"))
    if current is not None and total is not None and total > 0:
        return valid_percent(current / total * 100.0)
    return None


def classify(limits):
    """(five_hour, weekly, mcp) entries by the pinned TE44 classification."""
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


def countable(limit):
    """Counted representation (used, total, remaining, unit) or None."""
    unit = "requests" if limit.get("type") == "TIME_LIMIT" else "tokens"
    used = finite_number(limit.get("currentValue"))
    total = finite_number(limit.get("usage"))
    remaining = finite_number(limit.get("remaining"))
    values = {}
    if used is not None and used >= 0:
        values["used"] = used
    if total is not None and total > 0:
        values["total"] = total
    if remaining is not None and remaining >= 0:
        values["remaining"] = remaining
    if "used" in values and "total" in values and values["used"] > values["total"]:
        return None
    if "remaining" in values and "total" in values and values["remaining"] > values["total"]:
        return None
    if not values:
        return None
    values["unit"] = unit
    return values


def typed_window(window_id, label, limit, duration):
    """One typed window from a GLM limit entry; None when it has no numbers."""
    raw_percent = finite_number(limit.get("percentage"))
    window = {"id": window_id, "label": label}
    if raw_percent is not None:
        used = valid_percent(raw_percent)
        if used is None:
            return None
        window["usedPercent"] = round(used, 4)
        window["remainingPercent"] = round(max(0.0, 100.0 - used), 4)
        limited = used >= 100.0
    else:
        counted = countable(limit)
        if counted is None:
            return None
        window.update(counted)
        limited = (
            "used" in counted
            and "total" in counted
            and counted["used"] >= counted["total"]
        )
    window["status"] = "rate-limited" if limited else "ok"
    reset = iso_reset(limit.get("nextResetTime"))
    if reset:
        window["resetAt"] = reset
    if duration:
        window["durationSeconds"] = duration
    return window


def typed_window_text(window):
    """Fallback compact text for a counted window with no legacy percent."""
    label = window["label"]
    if "usedPercent" in window:
        return f"{label} {fmt(window['usedPercent'])}%"
    unit = window.get("unit", "")
    if "total" in window and window["total"] > 0:
        if "used" in window:
            return f"{label} {fmt(window['used'])}/{fmt(window['total'])} {unit}"
        if "remaining" in window:
            return f"{label} {fmt(window['remaining'])}/{fmt(window['total'])} {unit}"
        return f"{label} {fmt(window['total'])} {unit}"
    if "used" in window:
        return f"{label} {fmt(window['used'])} {unit}"
    return f"{label} {fmt(window['remaining'])} {unit}"


def fmt(value):
    if value is None:
        return "n/a"
    return f"{value:.0f}" if abs(value - round(value)) < 0.05 else f"{value:.1f}"


def offline():
    """Defensive RPI_OFFLINE check (the host is the first boundary)."""
    value = os.environ.get("RPI_OFFLINE")
    return bool(value) and value.strip().lower() in ("1", "true", "yes")


def main():
    if offline():
        return fail("offline", "offline mode: usage scripts are disabled")
    context = read_context()
    key, source = resolve_key(context)
    if not key:
        return fail(
            "auth-unavailable",
            "no API key; set ZAI_API_KEY, GLM_API_KEY or ZAI_CODING_CN_API_KEY",
        )

    url = endpoint_url(quota_url(source), key)
    status = 0
    body = ""
    last_failure = None
    succeeded = False
    for _label, headers in auth_candidates(url, key):
        status, body, last_failure = http_get(url, headers, timeout_seconds())
        if last_failure == "timeout":
            break
        if last_failure is None and status == 200:
            succeeded = True
            break
    if not succeeded:
        return fail(failure_kind(status, last_failure), "request failed")

    try:
        data = json.loads(body)
    except ValueError:
        return fail("shape-changed", "endpoint answered non-JSON")
    if not isinstance(data, dict) or data.get("success") is False:
        return fail("shape-changed", "endpoint rejected the request")
    code = data.get("code")
    if isinstance(code, (int, float)) and code not in (0, 200):
        return fail("shape-changed", "endpoint answered an error code")

    payload = data.get("data")
    payload = payload if isinstance(payload, dict) else data
    limits = payload.get("limits")
    if not isinstance(limits, list):
        return fail("shape-changed", "response carries no limits")
    five_hour, weekly, mcp = classify(limits)

    now = now_epoch()
    windows = []
    for window_id, label, limit, duration in (
        ("5h", "5h", five_hour, FIVE_HOURS),
        ("7d", "7d", weekly, ONE_WEEK),
        ("mcp", "MCP", mcp, None),
    ):
        if limit is None:
            continue
        window = typed_window(window_id, label, limit, duration)
        if window is not None:
            windows.append(window)

    # Legacy display: percentage annotations only, exactly as TE50 produced.
    parts = []
    for window_id, limit in (("5h", five_hour), ("7d", weekly), ("mcp", mcp)):
        if limit is None:
            continue
        value = percent(limit)
        if value is None:
            continue
        label = {"5h": "5h", "7d": "7d", "mcp": "MCP"}[window_id]
        parts.append(
            f"{label} {fmt(value)}%"
            + countdown_annotation(epoch_seconds(limit.get("nextResetTime")), now)
        )
    if not parts and windows:
        # A count-only response had no legacy percent; keep a stable compact
        # line for old hosts while the typed windows carry the real values.
        parts = [
            typed_window_text(window)
            + countdown_annotation(
                iso_epoch_of(window.get("resetAt")), now
            )
            for window in windows
        ]
    if not parts:
        return fail("shape-changed", "response carries no recognized windows")
    display = " / ".join(parts)

    envelope = {
        "schemaVersion": 1,
        "provider": PROVIDER,
        "displayText": display,
        "windows": windows,
    }
    level = payload.get("level")
    if isinstance(level, str) and level.strip():
        envelope["plan"] = level.strip()
    primary = weekly or five_hour
    primary_pct = percent(primary) if primary is not None else None
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


def iso_epoch_of(value):
    """Epoch seconds for an already-emitted RFC3339 reset string."""
    if not isinstance(value, str):
        return None
    try:
        parsed = datetime.datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    if parsed.tzinfo is None:
        return None
    return parsed.timestamp()


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(130)