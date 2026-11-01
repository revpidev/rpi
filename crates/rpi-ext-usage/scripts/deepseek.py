#!/usr/bin/env python3
"""rpi-usage provider script: DeepSeek balance (TE44 / TE53).

Contract (host framework, V16-05 §7.2):
- stdin:  {"provider", "baseUrl"?, "apiKeyEnv"?, "model"?, "credentialKind"?}
- stdout: the schemaVersion=1 usage envelope (single JSON object)
- errors: exit 2 + `{"schemaVersion":1,"provider":...,"error":{"kind":...}}`
  for a classified failure (the host reads it before the success shape); a
  short sanitized line on stderr is diagnostics only
- displayText (TE50): compact balance "<currency> <total>" entries joined by
  " / ", no provider prefix; unchanged byte-for-byte for the recorded
  fixtures so old hosts keep rendering the same line
- typed balance[] (04 §4.1): multi-currency total/granted/toppedUp entries.
  DeepSeek is a balance provider and has no quota windows.

Endpoint (public docs): GET https://api.deepseek.com/user/balance with
`Authorization: Bearer <key>`; response:
  {"is_available": bool,
   "balance_infos": [{"currency", "total_balance", "granted_balance",
                      "topped_up_balance"}]}
All amount fields ship as strings; they are parsed leniently.

Endpoint policy (TE53 04 §5.1):
- the URL is a fixed official HTTPS origin; the stdin `baseUrl` is never
  used to derive an endpoint (a custom/proxy model origin cannot inherit the
  subscription usage identity);
- redirects are refused, the body is capped at 262144 bytes and every
  request has a timeout;
- the test-only loopback override (`RPI_USAGE_TEST_MODE=1` +
  `RPI_USAGE_TEST_BASE_URL=http://127.0.0.1:PORT` + a credential that starts
  with `test-`) replaces the origin and keeps the path, nothing else.

Credential resolution order (never printed): the context `apiKeyEnv`, the
conventional environment variables below, then the rpi credential store
`<RPI_CODING_AGENT_DIR|~/.rpi/agent>/auth.json` (read-only, api_key entries;
`$VAR` references resolve through the environment, command references are
skipped). The host-injected env value is the same read-only identity.

Tests: crates/rpi-ext-usage/tests/scripts_contract.rs drives this script
against a local stub HTTP server with recorded fixtures (no network).
"""

import json
import os
import socket
import ssl
import sys
import urllib.error
import urllib.parse
import urllib.request

PROVIDER = "deepseek"
ENV_NAMES = ("DEEPSEEK_API_KEY",)
AUTH_IDS = ("deepseek",)
DEFAULT_BASE = "https://api.deepseek.com"
BALANCE_PATH = "/user/balance"
DEFAULT_TIMEOUT_MS = 8000
USER_AGENT = "rpi-usage/0.1.0"
MAX_BODY_BYTES = 262144

TEST_MODE_ENV = "RPI_USAGE_TEST_MODE"
TEST_BASE_ENV = "RPI_USAGE_TEST_BASE_URL"
LOOPBACK_HOSTS = ("127.0.0.1", "::1", "localhost")


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
    path = os.path.join(base, "auth.json")
    try:
        with open(path, encoding="utf-8") as handle:
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
            # Command references are not executed by scripts (host-side face).
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


def number(value):
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, str):
        try:
            return float(value.strip())
        except ValueError:
            return None
    return None


def money(value):
    if value is None:
        return "?"
    return f"{value:.2f}"


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
            "no API key; set DEEPSEEK_API_KEY (or log in with rpi)",
        )

    status, body, transport = http_get(
        endpoint_url(DEFAULT_BASE + BALANCE_PATH, key),
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

    infos = data.get("balance_infos")
    if not isinstance(infos, list) or not infos:
        return fail("shape-changed", "response carries no balance_infos")

    entries = []
    labels = []
    for info in infos:
        if not isinstance(info, dict):
            continue
        currency = info.get("currency")
        if not isinstance(currency, str) or not currency.strip():
            continue
        total = number(info.get("total_balance"))
        entry = {"currency": currency.strip()}
        if total is not None:
            entry["total"] = total
        granted = number(info.get("granted_balance"))
        if granted is not None:
            entry["granted"] = granted
        topped_up = number(info.get("topped_up_balance"))
        if topped_up is not None:
            entry["toppedUp"] = topped_up
        entries.append(entry)
        labels.append(f"{entry['currency']} {money(total)}")
    if not entries:
        return fail("shape-changed", "response carries no usable balance entries")

    available = data.get("is_available")
    display = " / ".join(labels)
    if available is False:
        display += " (unavailable)"

    envelope = {
        "schemaVersion": 1,
        "provider": PROVIDER,
        "balance": entries,
        "displayText": display,
    }
    sys.stdout.write(json.dumps(envelope, separators=(",", ":")))
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(130)