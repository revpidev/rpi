#!/usr/bin/env python3
"""rpi-usage provider script: OpenAI Codex subscription usage (TE53).

Contract (host framework, V16-05 §7.2):
- stdin:  {"provider", "baseUrl"?, "apiKeyEnv"?, "model"?, "credentialKind"?}
- stdout: the schemaVersion=1 usage envelope (single JSON object)
- errors: exit 2 + `{"schemaVersion":1,"provider":...,"error":{"kind":...}}`
  for a classified failure; a short sanitized line on stderr is diagnostics
- displayText: compact windows "<label> <pct>%(~<reset-in>)" joined by
  " / ", no provider prefix
- typed windows[] (04 §4.1): ids primary / secondary, percentage
  representation, a required status and an optional RFC3339
  resetAt/durationSeconds; plan from `plan_type`.

Credential (OAuth only; 03 §5.1):
- `RPI_USAGE_OAUTH_ACCESS` / `RPI_USAGE_OAUTH_ACCOUNT_ID` when the host
  injects them (the host already resolved the same complete credential);
- otherwise the read-only rpi credential store
  `<RPI_CODING_AGENT_DIR|~/.rpi/agent>/auth.json` entry `openai-codex` with
  `type: "oauth"`, **top-level** `access` and **top-level** `accountId`
  (rpi's flattened OAuth `extra`; there is no nested `extra.accountId`).
  `expires` (epoch ms) must be in the future. The refresh token is never
  read or sent and no refresh request is made.
- A native `openai` API key is not a Codex subscription credential: this
  script never reads one.

Endpoint (fixed official origin; the stdin `baseUrl` is never used):
  GET https://chatgpt.com/backend-api/wham/usage
headers `Authorization: Bearer <access>` and `ChatGPT-Account-Id: <account>`.
The earned-reset `consume` endpoint is never called: it is a write and would
spend the subscription.

Response:
  {"plan_type": "pro",
   "rate_limit": {"allowed": true, "limit_reached": false,
     "primary_window": {"used_percent": 42.5,
                        "limit_window_seconds": 18000,
                        "reset_after_seconds": 2880,
                        "reset_at": <epoch seconds>},
     "secondary_window": {...}}}
`reset_at` is epoch seconds; when it is absent the reset is
`RPI_USAGE_NOW_MS` (production wall clock) + `reset_after_seconds * 1000`,
normalized to RFC3339. `allowed === false` or `limit_reached === true` makes
the windows rate-limited; only an explicit `allowed === true` is `ok`, and
anything else stays `unknown` (values are never guessed).

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

PROVIDER = "openai-codex"
USAGE_URL = "https://chatgpt.com/backend-api/wham/usage"
DEFAULT_TIMEOUT_MS = 8000
USER_AGENT = "rpi-usage/0.1.0"
MAX_BODY_BYTES = 262144

OAUTH_ACCESS_ENV = "RPI_USAGE_OAUTH_ACCESS"
OAUTH_ACCOUNT_ENV = "RPI_USAGE_OAUTH_ACCOUNT_ID"
AUTH_ID = "openai-codex"

TEST_MODE_ENV = "RPI_USAGE_TEST_MODE"
TEST_BASE_ENV = "RPI_USAGE_TEST_BASE_URL"
LOOPBACK_HOSTS = ("127.0.0.1", "::1", "localhost")

WINDOWS = (("primary_window", "primary"), ("secondary_window", "secondary"))


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
    return int(value) if value > 1e11 else int(float(value) * 1000)


def _auth_store_entry():
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
    return entry


def resolve_credential():
    """(access, account_id) from one complete credential source, else None.

    The host injects `access` and `accountId` together from the same auth.json
    entry. If only the access is present, the account is *not* taken from a
    different local entry: mixing two credential sources could authorize one
    account's request with another account's id. Without an injected access,
    both values come from the same read-only auth.json entry.
    """
    injected_access = os.environ.get(OAUTH_ACCESS_ENV)
    injected_account = os.environ.get(OAUTH_ACCOUNT_ENV)
    if injected_access and injected_access.strip():
        account = (injected_account or "").strip()
        if not account:
            return None
        return injected_access.strip(), account
    entry = _auth_store_entry()
    if entry is None:
        return None
    raw_access = entry.get("access")
    if not isinstance(raw_access, str) or not raw_access.strip():
        return None
    expires = _expiry_ms(entry.get("expires"))
    if expires is None or expires <= now_ms():
        return None
    account = entry.get("accountId")
    if not isinstance(account, str) or not account.strip():
        return None
    return raw_access.strip(), account.strip()


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


def iso_from_epoch(seconds):
    try:
        return datetime.datetime.fromtimestamp(
            seconds, tz=datetime.timezone.utc
        ).strftime("%Y-%m-%dT%H:%M:%SZ")
    except (OverflowError, OSError, ValueError):
        return None


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


def duration_label(seconds):
    """Display label from a positive window length, else None."""
    if not isinstance(seconds, int) or isinstance(seconds, bool) or seconds <= 0:
        return None
    if seconds % 86400 == 0:
        return f"{seconds // 86400}d"
    if seconds % 3600 == 0:
        return f"{seconds // 3600}h"
    if seconds % 60 == 0:
        return f"{seconds // 60}m"
    return f"{seconds}s"


def reset_seconds(window):
    """RFC3339 reset from `reset_at` epoch seconds or the reset-after fallback."""
    reset_at = window.get("reset_at")
    if reset_at is not None:
        if isinstance(reset_at, bool) or not isinstance(reset_at, (int, float)):
            return "invalid", None
        if not math.isfinite(float(reset_at)) or float(reset_at) <= 0:
            return "invalid", None
        value = float(reset_at)
        if value > 1e11:
            value /= 1000.0
        if value > 253_402_300_799:
            return "invalid", None
        return "ok", value
    reset_after = window.get("reset_after_seconds")
    if reset_after is not None:
        if isinstance(reset_after, bool) or not isinstance(reset_after, (int, float)):
            return "invalid", None
        if not math.isfinite(float(reset_after)) or float(reset_after) < 0:
            return "invalid", None
        return "ok", (now_ms() + int(float(reset_after) * 1000)) / 1000.0
    return "absent", None


def offline():
    """Defensive RPI_OFFLINE check (the host is the first boundary)."""
    value = os.environ.get("RPI_OFFLINE")
    return bool(value) and value.strip().lower() in ("1", "true", "yes")


def main():
    if offline():
        return fail("offline", "offline mode: usage scripts are disabled")
    credential = resolve_credential()
    if credential is None:
        return fail(
            "auth-unavailable",
            "no unexpired Codex OAuth access/accountId; run /login",
        )
    access, account = credential

    status, body, transport = http_get(
        endpoint_url(USAGE_URL, access),
        {
            "Authorization": "Bearer " + access,
            "ChatGPT-Account-Id": account,
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
    rate_limit = data.get("rate_limit")
    if not isinstance(rate_limit, dict):
        return fail("shape-changed", "response carries no rate_limit")

    allowed = rate_limit.get("allowed")
    limit_reached = rate_limit.get("limit_reached")
    if allowed is False or limit_reached is True:
        window_status = "rate-limited"
    elif allowed is True:
        window_status = "ok"
    else:
        window_status = "unknown"

    windows = []
    for field, window_id in WINDOWS:
        entry = rate_limit.get(field)
        if entry is None:
            continue
        if not isinstance(entry, dict):
            return fail("shape-changed", "window entry is not an object")
        used = strict_percent(entry.get("used_percent"))
        if used is None:
            return fail("shape-changed", "window used_percent is missing or invalid")
        reset_state, reset = reset_seconds(entry)
        if reset_state == "invalid":
            return fail("shape-changed", "window reset is invalid")
        duration = entry.get("limit_window_seconds")
        duration_seconds = None
        if duration is not None:
            if isinstance(duration, bool) or not isinstance(duration, (int, float)):
                return fail("shape-changed", "window duration is invalid")
            if not math.isfinite(float(duration)) or float(duration) < 0:
                return fail("shape-changed", "window duration is invalid")
            if float(duration) > 0:
                duration_seconds = int(round(float(duration)))
        label = duration_label(duration_seconds) or window_id
        window = {
            "id": window_id,
            "label": label,
            "status": window_status,
            "usedPercent": round(used, 4),
            "remainingPercent": round(max(0.0, 100.0 - used), 4),
        }
        if reset is not None:
            reset_at = iso_from_epoch(reset)
            if reset_at is None:
                return fail("shape-changed", "window reset is invalid")
            window["resetAt"] = reset_at
        if duration_seconds is not None:
            window["durationSeconds"] = duration_seconds
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
    plan_type = data.get("plan_type")
    if isinstance(plan_type, str) and plan_type.strip():
        envelope["plan"] = plan_type.strip()

    sys.stdout.write(json.dumps(envelope, separators=(",", ":")))
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(130)