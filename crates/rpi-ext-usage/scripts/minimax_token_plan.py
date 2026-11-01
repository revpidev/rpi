#!/usr/bin/env python3
"""rpi-usage provider script: MiniMax Token Plan quota (TE44 / TE53).

Contract (host framework, V16-05 §7.2):
- stdin:  {"provider", "baseUrl"?, "apiKeyEnv"?, "model"?, "credentialKind"?}
- stdout: the schemaVersion=1 usage envelope (single JSON object)
- errors: exit 2 + `{"schemaVersion":1,"provider":...,"error":{"kind":...}}`
  for a classified failure; a short sanitized line on stderr is diagnostics
- displayText (TE50): compact windows "<label> <pct>%(~<reset-in>)" joined
  by " / ", no provider prefix; unchanged byte-for-byte for the recorded
  fixtures so old hosts keep working
- typed windows[] (04 §4.1): ids interval / weekly with exactly one of a
  percentage, counted or unlimited representation, a required status and an
  optional RFC3339 resetAt/durationSeconds.

Endpoint (pinned from the TE44 live capture, 2026-10-05; fixed official
origins per TE53 04 §5.1 — the stdin `baseUrl` is never used):
- international: GET https://api.minimax.io/v1/token_plan/remains
- China:         GET https://api.minimaxi.com/v1/token_plan/remains
with `Authorization: Bearer <key>`. The region follows the resolved
credential source (`MINIMAX_CN_API_KEY` / the `minimax-cn` auth entry use
China), never a custom model URL.

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
success signal (1001 = no token plan). Status 3 is an unlimited window, status
2 is exhausted (rate-limited); a missing remaining percent falls back to the
`*_total_count` / `*_usage_count` counted window (unit "requests").

Endpoint policy: redirects refused, 262144-byte body cap, request timeout.
The test-only loopback override (`RPI_USAGE_TEST_MODE=1` +
`RPI_USAGE_TEST_BASE_URL` loopback + a `test-` credential) replaces the
origin and keeps the path.

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

PROVIDER = "minimax-token-plan"
ENV_NAMES = ("MINIMAX_API_KEY", "MINIMAX_CN_API_KEY")
AUTH_IDS = ("minimax", "minimax-cn")
INTL_BASE = "https://api.minimax.io"
CN_BASE = "https://api.minimaxi.com"
REMAINS_PATH = "/v1/token_plan/remains"
DEFAULT_TIMEOUT_MS = 8000
USER_AGENT = "rpi-usage/0.1.0"
MAX_BODY_BYTES = 262144

TEST_MODE_ENV = "RPI_USAGE_TEST_MODE"
TEST_BASE_ENV = "RPI_USAGE_TEST_BASE_URL"
LOOPBACK_HOSTS = ("127.0.0.1", "::1", "localhost")

STATUS_NORMAL = 1
STATUS_EXHAUSTED = 2
STATUS_UNLIMITED = 3


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


def remains_url(source):
    """Fixed official origin; the region follows the credential source."""
    source = (source or "").lower()
    china = "minimax_cn" in source or source.endswith("minimax-cn")
    return (CN_BASE if china else INTL_BASE) + REMAINS_PATH


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


def duration_seconds(start, end):
    """Positive whole seconds between two epoch-ms timestamps, else None."""
    if isinstance(start, bool) or isinstance(end, bool):
        return None
    if not isinstance(start, (int, float)) or not isinstance(end, (int, float)):
        return None
    seconds = (float(end) - float(start)) / 1000.0
    if seconds <= 0:
        return None
    return int(round(seconds))


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


def window_percent(bucket, prefix):
    """(used_percent | None, unlimited) for one window of a bucket (legacy).

    Only the documented statuses are interpreted: 1 normal, 2 exhausted,
    3 unlimited. Any other (or missing) status is `unknown` and produces no
    percentage; an out-of-range remaining percentage is invalid, not clamped.
    """
    status = bucket.get(f"current_{prefix}_status")
    if status == STATUS_UNLIMITED:
        return None, True
    if status == STATUS_EXHAUSTED:
        return 100.0, False
    if status != STATUS_NORMAL:
        return None, False
    remaining = finite_number(bucket.get(f"current_{prefix}_remaining_percent"))
    if remaining is None or remaining < 0.0 or remaining > 100.0:
        return None, False
    return 100.0 - remaining, False


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


def typed_window(bucket, prefix, window_id, label, time_prefix):
    """One typed window; None when the bucket carries no usable data.

    A missing/undocumented status becomes `unknown` without numbers (the
    footer skips it); only documented status 1 produces percentages/counts.
    """
    status_code = bucket.get(f"current_{prefix}_status")
    start = bucket.get(f"{time_prefix}start_time")
    end = bucket.get(f"{time_prefix}end_time")
    reset = iso_reset(end)
    duration = duration_seconds(start, end)
    if status_code == STATUS_UNLIMITED:
        window = {
            "id": window_id,
            "label": label,
            "status": "ok",
            "unlimited": True,
        }
    elif status_code == STATUS_EXHAUSTED:
        window = {
            "id": window_id,
            "label": label,
            "status": "rate-limited",
            "usedPercent": 100.0,
            "remainingPercent": 0.0,
        }
    elif status_code != STATUS_NORMAL:
        window = {"id": window_id, "label": label, "status": "unknown"}
    else:
        percent = window_percent(bucket, prefix)[0]
        if percent is not None:
            window = {
                "id": window_id,
                "label": label,
                "status": "rate-limited" if percent >= 100.0 else "ok",
                "usedPercent": round(percent, 4),
                "remainingPercent": round(max(0.0, 100.0 - percent), 4),
            }
        else:
            total = finite_number(bucket.get(f"current_{prefix}_total_count"))
            used = finite_number(bucket.get(f"current_{prefix}_usage_count"))
            # A 0/0 bucket is "no quota configured", not a counted window.
            if total is None or total <= 0 or (used is not None and used > total):
                return None
            window = {
                "id": window_id,
                "label": label,
                "status": "ok",
                "unit": "requests",
                "total": total,
            }
            if used is not None and used >= 0:
                window["used"] = used
                window["remaining"] = max(0.0, total - used)
                if used >= total:
                    window["status"] = "rate-limited"
    if reset:
        window["resetAt"] = reset
    if duration:
        window["durationSeconds"] = duration
    return window


def typed_window_text(window):
    """Fallback compact text for a counted/unlimited window."""
    label = window["label"]
    if window.get("unlimited"):
        return f"{label} unlimited"
    if "usedPercent" in window:
        return f"{label} {fmt(window['usedPercent'])}%"
    unit = window.get("unit", "requests")
    if "total" in window and window["total"] > 0:
        if "used" in window:
            return f"{label} {fmt(window['used'])}/{fmt(window['total'])} {unit}"
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
            "no API key; set MINIMAX_API_KEY or MINIMAX_CN_API_KEY",
        )

    status, body, transport = http_get(
        endpoint_url(remains_url(source), key),
        {
            "Authorization": "Bearer " + key,
            "Accept": "application/json",
            "User-Agent": USER_AGENT,
        },
        timeout_seconds(),
    )
    if transport is not None or status != 200:
        return fail(failure_kind(status, transport), "request failed")
    try:
        data = json.loads(body)
    except ValueError:
        return fail("shape-changed", "endpoint answered non-JSON")
    if not isinstance(data, dict):
        return fail("shape-changed", "unexpected response shape")
    base_resp = data.get("base_resp")
    status_code = base_resp.get("status_code") if isinstance(base_resp, dict) else None
    if isinstance(status_code, (int, float)) and status_code != 0:
        return fail("shape-changed", "endpoint answered an error status")
    remains = data.get("model_remains")
    if not isinstance(remains, list) or not remains:
        return fail("shape-changed", "response carries no model_remains")

    bucket = None
    for entry in remains:
        if isinstance(entry, dict) and entry.get("model_name") == "general":
            bucket = entry
            break
    if bucket is None:
        bucket = next((entry for entry in remains if isinstance(entry, dict)), None)
    if bucket is None:
        return fail("shape-changed", "response carries no usable bucket")

    interval_pct, interval_unlimited = window_percent(bucket, "interval")
    weekly_pct, weekly_unlimited = window_percent(bucket, "weekly")
    label = interval_label(bucket)

    windows = []
    interval = typed_window(bucket, "interval", "interval", label, "")
    if interval is not None:
        windows.append(interval)
    weekly = typed_window(bucket, "weekly", "weekly", "7d", "weekly_")
    if weekly is not None:
        windows.append(weekly)

    now = now_epoch()
    parts = []
    if interval_unlimited:
        parts.append(f"{label} unlimited")
    elif interval_pct is not None:
        parts.append(
            f"{label} {fmt(interval_pct)}%"
            + countdown_annotation(epoch_seconds(bucket.get("end_time")), now)
        )
    if weekly_unlimited:
        parts.append("7d unlimited")
    elif weekly_pct is not None:
        parts.append(
            f"7d {fmt(weekly_pct)}%"
            + countdown_annotation(epoch_seconds(bucket.get("weekly_end_time")), now)
        )
    if windows and all(window["status"] == "unknown" for window in windows):
        return fail("shape-changed", "response carries no usable windows")
    if not parts and windows:
        parts = [
            typed_window_text(window)
            for window in windows
            if window["status"] != "unknown"
        ]
    if not parts:
        return fail("shape-changed", "response carries no recognized windows")
    display = " / ".join(parts)

    envelope = {
        "schemaVersion": 1,
        "provider": PROVIDER,
        "plan": "Token Plan",
        "displayText": display,
        "windows": windows,
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