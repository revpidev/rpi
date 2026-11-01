#!/usr/bin/env python3
"""rpi-usage provider script: Anthropic subscription usage (TE53).

Contract (host framework, V16-05 §7.2):
- stdin:  {"provider", "baseUrl"?, "apiKeyEnv"?, "model"?, "credentialKind"?}
- stdout: the schemaVersion=1 usage envelope (single JSON object)
- errors: exit 2 + `{"schemaVersion":1,"provider":...,"error":{"kind":...}}`
  for a classified failure; a short sanitized line on stderr is diagnostics
- displayText: compact windows "<label> <pct>%(~<reset-in>)" joined by
  " / ", no provider prefix
- typed windows[] (04 §4.1): ids five_hour / seven_day (labels 5h / 7d),
  percentage representation, a required status and an optional RFC3339
  resetAt/durationSeconds.

Credential (OAuth only; 03 §5.1):
- `RPI_USAGE_OAUTH_ACCESS` when the host injects it (the host already
  resolved and validated the account identity);
- otherwise the read-only rpi credential store
  `<RPI_CODING_AGENT_DIR|~/.rpi/agent>/auth.json` entry `anthropic` with
  `type: "oauth"`. Only `access` is consumed and `expires` (epoch ms) must be
  in the future. The refresh token is never read, logged or sent, and no
  refresh request is ever made.
- A plain `ANTHROPIC_API_KEY` is not a subscription credential: this script
  never reads it and reports `auth-unavailable` without it.

Endpoint (fixed official origin; the stdin `baseUrl` is never used):
  GET https://api.anthropic.com/api/oauth/usage
headers `Authorization: Bearer <access>` and
`anthropic-beta: oauth-2025-04-20`. This private endpoint contract comes from
the documented first-party/third-party capture; it is not verified against a
real subscription by CI.

Response:
  {"five_hour": {"utilization": 39, "resets_at": "<RFC3339>"},
   "seven_day": {"utilization": 31.5, "resets_at": "<RFC3339>"}}
`utilization` is the used percentage (0..100). A missing window keeps the
other one; a missing/invalid utilization or reset is `shape-changed` (never a
guessed value). `/v1/messages` is never called: it could consume the
subscription.

Endpoint policy: redirects refused, 262144-byte body cap, request timeout.
The test-only loopback override (`RPI_USAGE_TEST_MODE=1` +
`RPI_USAGE_TEST_BASE_URL` loopback + a `test-` credential) replaces the
origin and keeps the path.

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
import time
import urllib.error
import urllib.parse
import urllib.request

PROVIDER = "anthropic"
USAGE_URL = "https://api.anthropic.com/api/oauth/usage"
ANTHROPIC_BETA = "oauth-2025-04-20"
DEFAULT_TIMEOUT_MS = 8000
USER_AGENT = "rpi-usage/0.1.0"
MAX_BODY_BYTES = 262144

OAUTH_ACCESS_ENV = "RPI_USAGE_OAUTH_ACCESS"
AUTH_ID = "anthropic"

TEST_MODE_ENV = "RPI_USAGE_TEST_MODE"
TEST_BASE_ENV = "RPI_USAGE_TEST_BASE_URL"
LOOPBACK_HOSTS = ("127.0.0.1", "::1", "localhost")

FIVE_HOURS = 18_000
ONE_WEEK = 604_800
WINDOWS = (("five_hour", "5h", FIVE_HOURS), ("seven_day", "7d", ONE_WEEK))


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


def now_ms():
    """Fetch-time clock in epoch ms; the contract tests pin RPI_USAGE_NOW_MS."""
    raw = os.environ.get("RPI_USAGE_NOW_MS")
    if raw:
        try:
            return int(raw)
        except ValueError:
            pass
    return int(time.time() * 1000)


def _expiry_ms(value):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    if not math.isfinite(float(value)) or value <= 0:
        return None
    # rpi stores epoch milliseconds; tolerate an epoch-seconds value.
    return int(value) if value > 1e11 else int(float(value) * 1000)


def _auth_store_access():
    base = os.environ.get("RPI_CODING_AGENT_DIR")
    if not base:
        base = os.path.join(os.path.expanduser("~"), ".rpi", "agent")
    try:
        with open(os.path.join(base, "auth.json"), encoding="utf-8") as handle:
            data = json.load(handle)
    except (OSError, ValueError):
        return None
    entry = data.get(AUTH_ID)
    if not isinstance(entry, dict) or entry.get("type") != "oauth":
        return None
    access = entry.get("access")
    if not isinstance(access, str) or not access.strip():
        return None
    expires = _expiry_ms(entry.get("expires"))
    if expires is None or expires <= now_ms():
        return None
    return access.strip()


def resolve_access():
    """Host-injected access first, else the read-only rpi OAuth entry."""
    injected = os.environ.get(OAUTH_ACCESS_ENV)
    if injected and injected.strip():
        return injected.strip()
    return _auth_store_access()


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


def now_epoch():
    """Fetch-time clock; the contract tests pin RPI_USAGE_NOW_MS."""
    raw = os.environ.get("RPI_USAGE_NOW_MS")
    if raw:
        try:
            return int(raw) / 1000.0
        except ValueError:
            pass
    return time.time()


def countdown_annotation(reset_epoch, now):
    """"(~<reset-in>)" at fetch time, at most two units."""
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


def fmt(value):
    if value is None:
        return "n/a"
    return f"{value:.0f}" if abs(value - round(value)) < 0.05 else f"{value:.1f}"


def strict_percent(value):
    """Finite 0..100 percentage or None (a shape change upstream)."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    number = float(value)
    if not math.isfinite(number) or number < 0.0 or number > 100.0:
        return None
    return number


def iso_reset(value):
    """Canonical RFC3339 UTC reset from an ISO string, else None."""
    if not isinstance(value, str) or not value.strip():
        return None
    text = value.strip().replace("Z", "+00:00")
    try:
        parsed = datetime.datetime.fromisoformat(text)
    except ValueError:
        return None
    if parsed.tzinfo is None:
        return None
    return parsed.astimezone(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def iso_epoch(value):
    if not isinstance(value, str):
        return None
    try:
        parsed = datetime.datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    if parsed.tzinfo is None:
        return None
    return parsed.timestamp()


def offline():
    """Defensive RPI_OFFLINE check (the host is the first boundary)."""
    value = os.environ.get("RPI_OFFLINE")
    return bool(value) and value.strip().lower() in ("1", "true", "yes")


def main():
    if offline():
        return fail("offline", "offline mode: usage scripts are disabled")
    access = resolve_access()
    if not access:
        return fail(
            "auth-unavailable",
            "no unexpired Anthropic OAuth access; run /login",
        )

    status, body, transport = http_get(
        endpoint_url(USAGE_URL, access),
        {
            "Authorization": "Bearer " + access,
            "anthropic-beta": ANTHROPIC_BETA,
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

    windows = []
    for window_id, label, duration in WINDOWS:
        entry = data.get(window_id)
        if entry is None:
            continue
        if not isinstance(entry, dict):
            return fail("shape-changed", "window entry is not an object")
        used = strict_percent(entry.get("utilization"))
        if used is None:
            return fail("shape-changed", "window utilization is missing or invalid")
        reset_at = None
        if entry.get("resets_at") is not None:
            reset_at = iso_reset(entry.get("resets_at"))
            if reset_at is None:
                return fail("shape-changed", "window reset is invalid")
        window = {
            "id": window_id,
            "label": label,
            "status": "rate-limited" if used >= 100.0 else "ok",
            "usedPercent": round(used, 4),
            "remainingPercent": round(max(0.0, 100.0 - used), 4),
            "durationSeconds": duration,
        }
        if reset_at:
            window["resetAt"] = reset_at
        windows.append(window)
    if not windows:
        return fail("shape-changed", "response carries no recognized windows")

    now = now_epoch()
    parts = [
        f"{window['label']} {fmt(window['usedPercent'])}%"
        + countdown_annotation(iso_epoch(window.get("resetAt")), now)
        for window in windows
    ]
    envelope = {
        "schemaVersion": 1,
        "provider": PROVIDER,
        "displayText": " / ".join(parts),
        "windows": windows,
    }
    sys.stdout.write(json.dumps(envelope, separators=(",", ":")))
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(130)