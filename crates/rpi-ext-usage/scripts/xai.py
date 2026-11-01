#!/usr/bin/env python3
"""rpi-usage provider script: xAI Grok subscription usage (TE53).

Contract (host framework, V16-05 §7.2):
- stdin:  {"provider", "baseUrl"?, "apiKeyEnv"?, "model"?, "credentialKind"?}
- stdout: the schemaVersion=1 usage envelope (single JSON object)
- errors: exit 2 + `{"schemaVersion":1,"provider":...,"error":{"kind":...}}`
  for a classified failure; a short sanitized line on stderr is diagnostics
- displayText: compact windows "<label> <pct>%(~<reset-in>)" joined by
  " / ", no provider prefix
- typed windows[]/ledgers[] (04 §4.1): `included_allowance` (overall
  creditUsagePercent), an optional separate `product_grok_build` window, and
  independent `on_demand` / `prepaid` USD ledgers.

Credential (OAuth only; 03 §5.1):
- `RPI_USAGE_OAUTH_ACCESS` when the host injects it, else the read-only rpi
  credential store `<RPI_CODING_AGENT_DIR|~/.rpi/agent>/auth.json` entry
  `xai` with `type: "oauth"` and an unexpired `expires` (epoch ms). The
  refresh token is never read or sent and no refresh request is made. A
  third-party CLI directory (`~/.grok`) is never read.

Endpoints (fixed official origins; the stdin `baseUrl` is never used):
1. GET https://cli-chat-proxy.grok.com/v1/user?include=subscription
   with Bearer, `X-XAI-Token-Auth: xai-grok-cli`, the Grok client headers
   and **no** `x-userid`. A non-empty top-level `userId` proves the account;
   a failed user request stops the run before billing.
2. GET https://cli-chat-proxy.grok.com/v1/billing?format=credits
   with the same headers plus the required `x-userid`. `userId` is transient:
   it is used for this one request and never logged, cached or emitted.

Billing mapping (`config` from the GetGrokCreditsConfig shape):
- `creditUsagePercent` -> `included_allowance` (percent = used);
- `currentPeriod.type` decides the label (`USAGE_PERIOD_TYPE_WEEKLY` -> 7d,
  `USAGE_PERIOD_TYPE_MONTHLY` -> 30d), `end` is resetAt, `end - start` is
  durationSeconds;
- a recognized `productUsage[].product` (`PRODUCT_GROK_BUILD` / `GrokBuild`)
  becomes the separate `product_grok_build` window using its own
  `usagePercent`; it is never merged, summed or substituted for the overall
  value (42.5 vs 61.2 stay two windows);
- `onDemandCap`/`onDemandUsed`/`prepaidBalance` are proto `{val: USD cents}`
  (an omitted zero arrives as `{}`); they become independent ledgers after
  dividing by 100;
- plan prefers the user response `subscriptionTier`, then the billing
  response top-level `subscriptionTier`.
When `creditUsagePercent` is absent: a complete, recognized currentPeriod
means the overall value is the contract's proven zero, otherwise product
windows (if any) are emitted without claiming an overall percentage; with
neither a period nor product data the response is `shape-changed`.

Endpoint policy: redirects refused, 262144-byte body cap, request timeout.
The test-only loopback override (`RPI_USAGE_TEST_MODE=1` +
`RPI_USAGE_TEST_BASE_URL` loopback + a `test-` credential) replaces the
origin of both URLs and keeps each path/query.

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

PROVIDER = "xai"
USER_URL = "https://cli-chat-proxy.grok.com/v1/user?include=subscription"
BILLING_URL = "https://cli-chat-proxy.grok.com/v1/billing?format=credits"
DEFAULT_TIMEOUT_MS = 8000
USER_AGENT = "rpi-usage/0.1.0"
MAX_BODY_BYTES = 262144

# Grok CLI client headers (xai-grok-shell billing/usage requests).
GROK_TOKEN_AUTH = "xai-grok-cli"
GROK_CLIENT_VERSION = "0.2.0"
GROK_CLIENT_IDENTIFIER = "grok-shell"
GROK_CLIENT_MODE = "interactive"

OAUTH_ACCESS_ENV = "RPI_USAGE_OAUTH_ACCESS"
AUTH_ID = "xai"

TEST_MODE_ENV = "RPI_USAGE_TEST_MODE"
TEST_BASE_ENV = "RPI_USAGE_TEST_BASE_URL"
LOOPBACK_HOSTS = ("127.0.0.1", "::1", "localhost")

PERIOD_TYPES = {
    "USAGE_PERIOD_TYPE_WEEKLY": ("7d", 604_800),
    "USAGE_PERIOD_TYPE_MONTHLY": ("30d", 2_592_000),
}
GROK_BUILD_PRODUCTS = ("PRODUCT_GROK_BUILD", "GrokBuild")


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


def resolve_access():
    """Host-injected access first, else the read-only rpi OAuth entry."""
    injected = os.environ.get(OAUTH_ACCESS_ENV)
    if injected and injected.strip():
        return injected.strip()
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


def grok_headers(access):
    return {
        "Authorization": "Bearer " + access,
        "X-XAI-Token-Auth": GROK_TOKEN_AUTH,
        "x-grok-client-version": GROK_CLIENT_VERSION,
        "x-grok-client-identifier": GROK_CLIENT_IDENTIFIER,
        "x-grok-client-mode": GROK_CLIENT_MODE,
        "Accept": "application/json",
        "User-Agent": "grok-cli/" + GROK_CLIENT_VERSION + " " + USER_AGENT,
    }


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


def parse_period(config):
    """Period info from `config.currentPeriod`; `"invalid"` on a bad reset."""
    period = config.get("currentPeriod")
    if not isinstance(period, dict):
        return None
    period_type = period.get("type")
    known = PERIOD_TYPES.get(period_type) if isinstance(period_type, str) else None
    reset_at = None
    end = period.get("end")
    if end is not None:
        reset_at = iso_reset(end)
        if reset_at is None:
            return "invalid"
    duration = None
    complete = False
    if known is not None:
        duration = known[1]
        start = iso_epoch(period.get("start"))
        end_epoch = iso_epoch(end)
        if start is not None and end_epoch is not None:
            complete = True
            seconds = int(round(end_epoch - start))
            if seconds > 0:
                duration = seconds
    return {
        "known": known is not None,
        "label": known[0] if known is not None else None,
        "duration": duration,
        "reset": reset_at,
        "complete": complete and reset_at is not None,
    }


def percent_window(window_id, label, used, reset_at, duration):
    window = {
        "id": window_id,
        "label": label,
        "status": "rate-limited" if used >= 100.0 else "ok",
        "usedPercent": round(used, 4),
        "remainingPercent": round(max(0.0, 100.0 - used), 4),
    }
    if reset_at:
        window["resetAt"] = reset_at
    if duration:
        window["durationSeconds"] = duration
    return window


def cent_value(value):
    """USD cents from a proto `{val}` (an omitted zero arrives as `{}`)."""
    if value is None:
        return None
    if not isinstance(value, dict):
        return "invalid"
    raw = value.get("val", 0)
    if isinstance(raw, bool) or not isinstance(raw, (int, float)):
        return "invalid"
    if not math.isfinite(float(raw)) or float(raw) < 0:
        return "invalid"
    return float(raw)


def usd(cents):
    return round(cents / 100.0, 2)


def build_ledgers(config):
    ledgers = []
    cap = cent_value(config.get("onDemandCap"))
    used = cent_value(config.get("onDemandUsed"))
    prepaid = cent_value(config.get("prepaidBalance"))
    if "invalid" in (cap, used, prepaid):
        return "invalid"
    if cap is not None and cap > 0:
        on_demand = {
            "id": "on_demand",
            "label": "On-demand",
            "kind": "on-demand",
            "unit": "USD",
            "currency": "USD",
            "total": usd(cap),
        }
        if used is not None:
            on_demand["used"] = usd(used)
            on_demand["remaining"] = usd(max(0.0, cap - used))
        ledgers.append(on_demand)
    if prepaid is not None:
        ledgers.append(
            {
                "id": "prepaid",
                "label": "Prepaid",
                "kind": "prepaid",
                "unit": "USD",
                "currency": "USD",
                "remaining": usd(prepaid),
            }
        )
    return ledgers


def product_entries(config):
    products = config.get("productUsage")
    return products if isinstance(products, list) else []


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
            "no unexpired xAI OAuth access; run /login",
        )

    headers = grok_headers(access)
    status, body, transport = http_get(
        endpoint_url(USER_URL, access), headers, timeout_seconds()
    )
    if transport is not None or status != 200:
        return fail(failure_kind(status, transport), "user request failed")
    try:
        user = json.loads(body)
    except ValueError:
        return fail("shape-changed", "user endpoint answered non-JSON")
    if not isinstance(user, dict):
        return fail("shape-changed", "unexpected user response shape")
    user_id = user.get("userId")
    if not isinstance(user_id, str) or not user_id.strip():
        return fail("shape-changed", "user response carries no userId")
    user_id = user_id.strip()

    billing_headers = dict(headers)
    billing_headers["x-userid"] = user_id
    status, body, transport = http_get(
        endpoint_url(BILLING_URL, access), billing_headers, timeout_seconds()
    )
    if transport is not None or status != 200:
        return fail(failure_kind(status, transport), "billing request failed")
    try:
        billing = json.loads(body)
    except ValueError:
        return fail("shape-changed", "billing endpoint answered non-JSON")
    if not isinstance(billing, dict):
        return fail("shape-changed", "unexpected billing response shape")
    config = billing.get("config")
    if not isinstance(config, dict):
        config = {}

    period = parse_period(config)
    if period == "invalid":
        return fail("shape-changed", "billing period reset is invalid")

    windows = []
    overall = config.get("creditUsagePercent")
    if overall is not None:
        used = strict_percent(overall)
        if used is None:
            return fail("shape-changed", "creditUsagePercent is invalid")
        # The overall percentage only has a meaning inside a recognized
        # weekly/monthly period; without one its label/reset would be guessed.
        if not isinstance(period, dict) or not period["known"]:
            return fail(
                "shape-changed",
                "creditUsagePercent without a recognized currentPeriod",
            )
        windows.append(
            percent_window(
                "included_allowance",
                period["label"],
                used,
                period["reset"],
                period["duration"],
            )
        )
    elif isinstance(period, dict) and period["complete"]:
        # A complete, recognized period (type + start + end) proves the
        # omitted proto zero.
        windows.append(
            percent_window(
                "included_allowance",
                period["label"],
                0.0,
                period["reset"],
                period["duration"],
            )
        )

    for entry in product_entries(config):
        if not isinstance(entry, dict):
            continue
        product = entry.get("product")
        if product not in GROK_BUILD_PRODUCTS:
            continue
        used = strict_percent(entry.get("usagePercent"))
        if used is None:
            return fail("shape-changed", "Grok Build usagePercent is invalid")
        windows.append(
            percent_window(
                "product_grok_build",
                "Grok Build",
                used,
                period["reset"] if isinstance(period, dict) else None,
                period["duration"] if isinstance(period, dict) else None,
            )
        )
        break
    if not windows:
        return fail("shape-changed", "response carries no recognized usage windows")

    ledgers = build_ledgers(config)
    if ledgers == "invalid":
        return fail("shape-changed", "billing ledger values are invalid")

    envelope = {
        "schemaVersion": 1,
        "provider": PROVIDER,
        "displayText": " / ".join(
            f"{window['label']} {fmt(window['usedPercent'])}%"
            + countdown_annotation(iso_epoch(window.get("resetAt")), now_epoch())
            for window in windows
        ),
        "windows": windows,
    }
    plan = user.get("subscriptionTier")
    if not isinstance(plan, str) or not plan.strip():
        plan = billing.get("subscriptionTier")
    if isinstance(plan, str) and plan.strip():
        envelope["plan"] = plan.strip()
    if ledgers:
        envelope["ledgers"] = ledgers

    sys.stdout.write(json.dumps(envelope, separators=(",", ":")))
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(130)