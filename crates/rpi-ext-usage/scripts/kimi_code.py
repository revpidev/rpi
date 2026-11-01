#!/usr/bin/env python3
"""rpi-usage provider script: Kimi Code quota (TE44 / TE53).

Contract (host framework, V16-05 §7.2):
- stdin:  {"provider", "baseUrl"?, "apiKeyEnv"?, "model"?, "credentialKind"?}
- stdout: the schemaVersion=1 usage envelope (single JSON object)
- errors: exit 2 + `{"schemaVersion":1,"provider":...,"error":{"kind":...}}`
  for a classified failure; a short sanitized line on stderr is diagnostics
- displayText (TE50): compact windows "<label> <pct>%(~<reset-in>)" joined
  by " / ", no provider prefix; unchanged byte-for-byte for the recorded
  fixtures so old hosts keep working
- typed windows[] (04 §4.1): ids 5h / 7d with a percentage representation,
  a required status and an optional RFC3339 resetAt/durationSeconds
- optional ledgers[]: a `prepaid` `booster_wallet` ledger when the response
  carries `extraUsage` (`{balanceCents,totalCents,currency,...}`). The booster
  wallet is never merged with the quota windows; it is only exposed as a
  separate typed ledger.

Endpoint (pinned from the TE44 live capture, 2026-10-05; fixed official
origin per TE53 04 §5.1 — the stdin `baseUrl` is never used):
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

Endpoint policy: redirects refused, 262144-byte body cap, request timeout.
The test-only loopback override (`RPI_USAGE_TEST_MODE=1` +
`RPI_USAGE_TEST_BASE_URL` loopback + a `test-` credential) replaces the
origin and keeps the path.

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

PROVIDER = "kimi-code"
ENV_NAMES = ("KIMI_API_KEY", "KIMI_CODING_API_KEY")
AUTH_IDS = ("kimi-coding",)
DEFAULT_BASE = "https://api.kimi.com"
USAGES_PATH = "/coding/v1/usages"
DEFAULT_TIMEOUT_MS = 8000
USER_AGENT = "rpi-usage/0.1.0"
MAX_BODY_BYTES = 262144

TEST_MODE_ENV = "RPI_USAGE_TEST_MODE"
TEST_BASE_ENV = "RPI_USAGE_TEST_BASE_URL"
LOOPBACK_HOSTS = ("127.0.0.1", "::1", "localhost")

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


def number(value):
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        return float(value)
    if isinstance(value, str):
        try:
            return float(value.strip())
        except ValueError:
            return None
    return None


def ratio(value):
    """Used percent from a 0..1 ratio (or a legacy 0..100 percentage).

    Out-of-range or non-finite values are invalid, not clamped: a provider
    ratio outside the contract is drift, never a fabricated 0/100 window.
    """
    parsed = number(value)
    if parsed is None or not math.isfinite(parsed):
        return None
    if 0.0 <= parsed <= 1.0:
        parsed *= 100.0
    if not 0.0 <= parsed <= 100.0:
        return None
    return parsed


def finite_number(value):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    if not math.isfinite(float(value)):
        return None
    return float(value)


def window_label(window):
    if not isinstance(window, dict):
        return "5h"
    duration = number(window.get("duration"))
    unit = str(window.get("timeUnit") or "").lower()
    if unit.startswith("time_unit_"):
        unit = unit[len("time_unit_"):]
    unit = unit.rstrip("s")
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


def window_duration_seconds(window):
    """Positive duration of a rolling window, else None."""
    if not isinstance(window, dict):
        return None
    duration = number(window.get("duration"))
    unit = str(window.get("timeUnit") or "").lower()
    if unit.startswith("time_unit_"):
        unit = unit[len("time_unit_"):]
    unit = unit.rstrip("s")
    if duration is None or duration <= 0:
        return None
    factor = {"second": 1, "minute": 60, "hour": 3600, "day": 86400}.get(unit)
    if factor is None:
        return None
    return int(round(duration * factor))


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
        cleaned = level.strip()
        if cleaned.startswith("LEVEL_"):
            cleaned = cleaned[len("LEVEL_"):]
        return cleaned.replace("_", " ").lower().title() or "Kimi Code"
    return "Kimi Code"


def fmt(value):
    if value is None:
        return "n/a"
    return f"{value:.0f}" if abs(value - round(value)) < 0.05 else f"{value:.1f}"


def iso_epoch(value):
    """Epoch seconds from an ISO-8601 reset timestamp (TE50)."""
    if not isinstance(value, str) or not value.strip():
        return None
    text = value.strip().replace("Z", "+00:00")
    try:
        parsed = datetime.datetime.fromisoformat(text)
    except ValueError:
        return None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=datetime.timezone.utc)
    return parsed.timestamp()


def iso_from_epoch(seconds):
    """Canonical RFC3339 UTC string for an epoch timestamp, else None."""
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


def percent_window(window_id, label, used, reset, duration):
    used = max(0.0, min(100.0, used))
    window = {
        "id": window_id,
        "label": label,
        "status": "rate-limited" if used >= 100.0 else "ok",
        "usedPercent": round(used, 4),
        "remainingPercent": round(max(0.0, 100.0 - used), 4),
    }
    reset_at = iso_from_epoch(reset)
    if reset_at:
        window["resetAt"] = reset_at
    if duration:
        window["durationSeconds"] = duration
    return window


def booster_ledger(extra):
    """Separate prepaid ledger from `extraUsage` cents, never merged."""
    if not isinstance(extra, dict):
        return None
    currency = extra.get("currency")
    if not isinstance(currency, str) or not currency.strip():
        currency = "USD"
    currency = currency.strip()
    remaining = number(extra.get("balanceCents"))
    total = number(extra.get("totalCents"))
    ledger = {
        "id": "booster_wallet",
        "label": "Booster",
        "kind": "prepaid",
        "unit": currency,
        "currency": currency,
    }
    if remaining is not None and remaining >= 0:
        ledger["remaining"] = round(remaining / 100.0, 4)
    if total is not None and total > 0:
        ledger["total"] = round(total / 100.0, 4)
        if remaining is not None and remaining >= 0:
            ledger["used"] = round(max(0.0, total - remaining) / 100.0, 4)
    if "remaining" not in ledger and "total" not in ledger:
        return None
    return ledger


def offline():
    """Defensive RPI_OFFLINE check (the host is the first boundary)."""
    value = os.environ.get("RPI_OFFLINE")
    return bool(value) and value.strip().lower() in ("1", "true", "yes")


def main():
    if offline():
        return fail("offline", "offline mode: usage scripts are disabled")
    context = read_context()
    key, _source = resolve_key(context)
    if not key:
        return fail(
            "auth-unavailable",
            "no API key; set KIMI_API_KEY or KIMI_CODING_API_KEY",
        )

    url = endpoint_url(DEFAULT_BASE + USAGES_PATH, key)
    candidates = [
        {"x-api-key": key},
        {"Authorization": "Bearer " + key},
    ]
    status = 0
    body = ""
    transport = None
    succeeded = False
    for headers in candidates:
        status, body, transport = http_get(
            url,
            {**headers, "Accept": "application/json", "User-Agent": USER_AGENT},
            timeout_seconds(),
        )
        if transport == "timeout":
            break
        if transport is None and status == 200:
            succeeded = True
            break
    if not succeeded:
        return fail(failure_kind(status, transport), "request failed")

    try:
        data = json.loads(body)
    except ValueError:
        return fail("shape-changed", "endpoint answered non-JSON")
    if not isinstance(data, dict):
        return fail("shape-changed", "unexpected response shape")

    usages = data.get("usages") if isinstance(data.get("usages"), dict) else {}
    weekly_detail = data.get("usage") if isinstance(data.get("usage"), dict) else {}
    weekly_pct = count_percent(weekly_detail)
    weekly_reset = iso_epoch(weekly_detail.get("resetTime"))
    limit_7d = usages.get("limit_7d") if isinstance(usages.get("limit_7d"), dict) else {}
    if weekly_pct is None:
        weekly_pct = ratio(limit_7d.get("used_ratio"))
    if weekly_reset is None:
        weekly_reset = iso_epoch(limit_7d.get("reset_time"))

    limits = data.get("limits") if isinstance(data.get("limits"), list) else []
    first = limits[0] if limits and isinstance(limits[0], dict) else {}
    five_window = first.get("window")
    five_label = window_label(five_window)
    five_detail = first.get("detail") if isinstance(first.get("detail"), dict) else {}
    five_pct = count_percent(five_detail)
    five_reset = iso_epoch(five_detail.get("resetTime"))
    limit_5h = usages.get("limit_5h") if isinstance(usages.get("limit_5h"), dict) else {}
    if five_pct is None:
        five_pct = ratio(limit_5h.get("used_ratio"))
    if five_reset is None:
        five_reset = iso_epoch(limit_5h.get("reset_time"))

    windows = []
    if five_pct is not None:
        windows.append(
            percent_window(
                "5h",
                five_label,
                five_pct,
                five_reset,
                window_duration_seconds(five_window),
            )
        )
    if weekly_pct is not None:
        windows.append(
            percent_window("7d", "7d", weekly_pct, weekly_reset, ONE_WEEK)
        )

    now = now_epoch()
    parts = []
    if five_pct is not None:
        parts.append(
            f"{five_label} {fmt(five_pct)}%" + countdown_annotation(five_reset, now)
        )
    if weekly_pct is not None:
        parts.append(f"7d {fmt(weekly_pct)}%" + countdown_annotation(weekly_reset, now))
    if not parts:
        return fail("shape-changed", "response carries no recognized windows")
    display = " / ".join(parts)

    envelope = {
        "schemaVersion": 1,
        "provider": PROVIDER,
        "plan": plan_name(data),
        "displayText": display,
        "windows": windows,
    }
    ledger = booster_ledger(data.get("extraUsage"))
    if ledger is not None:
        envelope["ledgers"] = [ledger]
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