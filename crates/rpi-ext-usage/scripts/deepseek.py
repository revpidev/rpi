#!/usr/bin/env python3
"""rpi-usage provider script: DeepSeek balance (TE44).

Contract (host framework, V16-05 §7.2):
- stdin:  {"provider", "baseUrl"?, "apiKeyEnv"?, "model"?}
- stdout: the schemaVersion=1 usage envelope (single JSON object)
- errors: a message on stderr + non-zero exit (the host degrades silently)

Endpoint (public docs): GET https://api.deepseek.com/user/balance with
`Authorization: Bearer <key>`; response:
  {"is_available": bool,
   "balance_infos": [{"currency", "total_balance", "granted_balance",
                      "topped_up_balance"}]}
All amount fields ship as strings; they are parsed leniently.

Credential resolution order (never printed): the context `apiKeyEnv`, the
conventional environment variables below, then the rpi credential store
`<RPI_CODING_AGENT_DIR|~/.rpi/agent>/auth.json` (read-only, api_key entries;
`$VAR` references resolve through the environment, command references are
skipped).

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

PROVIDER = "deepseek"
ENV_NAMES = ("DEEPSEEK_API_KEY",)
AUTH_IDS = ("deepseek",)
DEFAULT_BASE = "https://api.deepseek.com"
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


def balance_url(base):
    base = (base or DEFAULT_BASE).strip().rstrip("/")
    if base.endswith("/user/balance"):
        return base
    if base.endswith("/v1"):
        base = base[: -len("/v1")]
    return base + "/user/balance"


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


def main():
    context = read_context()
    key, _source = resolve_key(context)
    if not key:
        print(
            f"{PROVIDER}: no API key; set {' or '.join(ENV_NAMES)} "
            "(or log in with rpi)",
            file=sys.stderr,
        )
        return 1

    status, body = http_get(
        balance_url(context.get("baseUrl")),
        {
            "Authorization": "Bearer " + key,
            "Accept": "application/json",
            "User-Agent": USER_AGENT,
        },
        timeout_seconds(),
    )
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

    infos = data.get("balance_infos")
    if not isinstance(infos, list) or not infos:
        print(f"{PROVIDER}: response carries no balance_infos", file=sys.stderr)
        return 1

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
        print(f"{PROVIDER}: response carries no usable balance entries", file=sys.stderr)
        return 1

    available = data.get("is_available")
    display = f"{PROVIDER}: " + " · ".join(labels)
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