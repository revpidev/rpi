# OAuth cross-implementation parity report (TE02 item 5 / TE03 groundwork)

Generated: 2026-08-14T09:54:35.392Z (rerun: `node scripts/mcp-parity/run-oauth-parity.mjs`)
Upstream: pi-mcp-adapter @ 3d953f90 (mcp-auth-flow.ts via SDK 2.0 auth)
rpi: crates/rpi-ext-mcp-adapter oauth.rs

Stub AS transcript (authorization URL params + token form params),
normalized: challenge/state/verifier/code → markers, ports → $port/$asport.

Verdict: MATCH

## Upstream entries

```json
[
  {
    "kind": "registration",
    "params": {
      "redirect_uris": [
        "http://localhost:$port/callback"
      ],
      "client_name": "$client_name",
      "client_uri": "$client_uri",
      "grant_types": [
        "authorization_code",
        "refresh_token"
      ],
      "response_types": [
        "code"
      ],
      "token_endpoint_auth_method": "none",
      "application_type": "native"
    }
  },
  {
    "kind": "authorization",
    "params": {
      "response_type": "code",
      "client_id": "stub-dcr-client",
      "code_challenge": "$challenge",
      "code_challenge_method": "S256",
      "redirect_uri": "http://localhost:$port/callback",
      "state": "$state"
    }
  },
  {
    "kind": "token",
    "params": {
      "grant_type": "authorization_code",
      "code": "$code",
      "code_verifier": "$verifier",
      "redirect_uri": "http://localhost:$port/callback",
      "client_id": "stub-dcr-client",
      "client_secret": "stub-dcr-secret"
    }
  }
]
```

## rpi entries

```json
[
  {
    "kind": "registration",
    "params": {
      "redirect_uris": [
        "http://localhost:$port/callback"
      ],
      "client_name": "$client_name",
      "client_uri": "$client_uri",
      "grant_types": [
        "authorization_code",
        "refresh_token"
      ],
      "response_types": [
        "code"
      ],
      "token_endpoint_auth_method": "none",
      "application_type": "native"
    }
  },
  {
    "kind": "authorization",
    "params": {
      "response_type": "code",
      "client_id": "stub-dcr-client",
      "redirect_uri": "http://localhost:$port/callback",
      "code_challenge": "$challenge",
      "code_challenge_method": "S256",
      "state": "$state"
    }
  },
  {
    "kind": "token",
    "params": {
      "grant_type": "authorization_code",
      "code": "$code",
      "redirect_uri": "http://localhost:$port/callback",
      "client_id": "stub-dcr-client",
      "code_verifier": "$verifier",
      "client_secret": "stub-dcr-secret"
    }
  }
]
```
