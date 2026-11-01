#!/usr/bin/env python3
"""rpi-usage provider script: OpenCode Go allowance (TE53).

Contract (host framework, V16-05 §7.2):
- stdin:  {"provider", "baseUrl"?, "apiKeyEnv"?, "model"?, "credentialKind"?}
- stdout: the schemaVersion=1 usage envelope (single JSON object)
- errors: exit 2 + `{"schemaVersion":1,"provider":...,"error":{"kind":...}}`
  for a classified failure; a short sanitized line on stderr is diagnostics
- displayText: compact windows "<label> <pct>%(~<reset-in>)" joined by
  " / ", no provider prefix
- typed windows[] (04 §4.1): ids rolling / weekly / monthly, percentage
  representation, a required status and an optional RFC3339 resetAt.

Endpoint (fixed official origin; the stdin `baseUrl` is never derived from):
  GET https://opencode.ai/zen/go/v1/usage
with `Authorization: Bearer <API key>`. This is the API-key path only: no
OpenCode Console workspace id, OAuth workspace header or custom origin is
used (an API key does not require a workspace id).

Response (`packages/core/src/usage-limits.ts` in the OpenCode first-party
source):
  {"usage": {"rolling":  {"status": "ok", "percent": 12.5, "resetsAt": "<ISO>"},
             "weekly":   {"status": "ok", "percent": 68,   "resetsAt": "<ISO>"},
             "monthly":  {"status": "rate-limited", "percent": 100,
                          "resetsAt": "<ISO>"}}}
`percent` is the used percentage. `rolling` is a rolling allowance and its
window is *not* hardcoded to five hours: the id/label stay `rolling`. A
status other than `ok` / `rate-limited` is typed `unknown`; a missing status
or a missing/invalid percent is a `shape-changed` failure (never a guessed
value). One missing window keeps the remaining windows.

Endpoint policy: redirects refused, 262144-byte body cap, request timeout.
The test-only loopback override (`RPI_USAGE_TEST_MODE=1` +
`RPI_USAGE_TEST_BASE_URL` loopback + a `test-` credential) replaces the
origin and keeps the path.

Credential resolution order (never printed): the context `apiKeyEnv`, the
conventional environment variable `OPENCODE_API_KEY`, then the rpi credential
store `<RPI_CODING_AGENT_DIR|~/.rpi/agent>/auth.json` (read-only, the
`opencode-go` api_key entry; `$VAR` references resolve through the
environment, command references are skipped).

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

PROVIDER = "opencode-go"
ENV_NAMES = ("OPENCODE_API_KEY",)
AUTH_IDS = ("opencode-go",)
USAGE_URL = "https://opencode.ai/zen/go/v1/usage"
DEFAULT_TIMEOUT_MS = 8000
USER_AGENT = "rpi-usage/0.1.0"
MAX_BODY_BYTES = 262144

TEST_MODE_ENV = "RPI_USAGE_TEST_MODE"
TEST_BASE_ENV = "RPI_USAGE_TEST_BASE_URL"
LOOPBACK_HOSTS = ("127.0.0.1", "::1", "localhost")

WINDOWS = ("rolling", "weekly", "monthly")


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
        value = os.environ.get(names[-1])
        if value and value.strip():
            return value.strip(), names[-1]
    for name in ENV_NAMES:
        value = os.environ.get(name)
        if value and value.strip():
            return value.strip(), name
    key = _auth_store_key()
    if key:
        return key, "auth.json:opencode-go"
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


def typed_status(value):
    if value == "ok":
        return "ok"
    if value == "rate-limited":
        return "rate-limited"
    if isinstance(value, str) and value.strip():
        return "unknown"
    return None


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
        return fail("auth-unavailable", "no API key; set OPENCODE_API_KEY")

    status, body, transport = http_get(
        endpoint_url(USAGE_URL, key),
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
    usage = data.get("usage")
    if not isinstance(usage, dict):
        return fail("shape-changed", "response carries no usage windows")

    windows = []
    for window_id in WINDOWS:
        entry = usage.get(window_id)
        if entry is None:
            continue
        if not isinstance(entry, dict):
            return fail("shape-changed", "window entry is not an object")
        window_status = typed_status(entry.get("status"))
        if window_status is None:
            return fail("shape-changed", "window status is missing")
        used = strict_percent(entry.get("percent"))
        if used is None:
            return fail("shape-changed", "window percent is missing or invalid")
        reset_at = None
        if entry.get("resetsAt") is not None:
            reset_at = iso_reset(entry.get("resetsAt"))
            if reset_at is None:
                return fail("shape-changed", "window reset is invalid")
        window = {
            "id": window_id,
            "label": window_id,
            "status": window_status,
            "usedPercent": round(used, 4),
            "remainingPercent": round(max(0.0, 100.0 - used), 4),
        }
        if reset_at:
            window["resetAt"] = reset_at
        windows.append(window)
    if not windows:
        return fail("shape-changed", "response carries no recognized windows")

    if all(window["status"] == "unknown" for window in windows):
        return fail("shape-changed", "response carries no usable windows")
    now = now_epoch()
    parts = [
        f"{window['label']} {fmt(window['usedPercent'])}%"
        + countdown_annotation(iso_epoch(window.get("resetAt")), now)
        for window in windows
        if window["status"] != "unknown"
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