#!/usr/bin/env node
// Stub authorization server for the OAuth parity harness (TE02 self-check
// item 5 / TE03 G3 groundwork): a minimal RFC 8414 + RFC 6749 AS that
// records every request it receives into a JSON transcript so the upstream
// Node flow and the rpi Rust flow can be diffed.
//
// Endpoints (all under one ephemeral port, announced on stdout line 1):
//   GET  /authorize                 → 302 to <redirect_uri>?code=stub-code&state=…
//   POST /token                     → JSON tokens (records the form body)
//   POST /register                  → DCR response client_id stub-dcr-client
//   GET  /.well-known/oauth-authorization-server → RFC 8414 metadata
//
// The MCP "resource server" role is folded in: GET/POST /mcp → 401 with
// WWW-Authenticate pointing at this server's protected-resource metadata.
//
// Transcript: RPI_MCP_OAUTH_TRANSCRIPT (JSON lines: {kind, method, url,
// headers, body}); read back by the drivers after the flow completes.

import http from "node:http";
import { appendFileSync, writeFileSync } from "node:fs";

const transcript = process.env.RPI_MCP_OAUTH_TRANSCRIPT;
const port = Number(process.env.RPI_MCP_OAUTH_PORT ?? "0");

function record(entry) {
  if (!transcript) return;
  appendFileSync(transcript, `${JSON.stringify(entry)}\n`);
}

const server = http.createServer((req, res) => {
  const chunks = [];
  req.on("data", (chunk) => chunks.push(chunk));
  req.on("end", () => {
    handle(req, res, new URL(req.url, "http://stub.invalid"), Buffer.concat(chunks).toString("utf8"));
  });
});

function handle(req, res, url, body) {
  // The issuer MUST match the URL the client used (RFC 8414 §3.3 — both
  // sides enforce it), so derive it from the request's Host header instead
  // of hardcoding localhost vs 127.0.0.1.
  const host = req.headers.host ?? `localhost:${server.address()?.port ?? port}`;
  const base = `http://${host}`;

  if (req.method === "GET" && url.pathname === "/.well-known/oauth-authorization-server") {
    const metadata = {
      issuer: base,
      authorization_endpoint: `${base}/authorize`,
      token_endpoint: `${base}/token`,
      registration_endpoint: `${base}/register`,
      response_types_supported: ["code"],
      code_challenge_methods_supported: ["S256"],
      grant_types_supported: ["authorization_code", "refresh_token", "client_credentials"],
      token_endpoint_auth_methods_supported: ["none", "client_secret_post"],
      scopes_supported: ["mcp:read", "mcp:write"],
    };
    res.writeHead(200, { "Content-Type": "application/json" });
    res.end(JSON.stringify(metadata));
    return;
  }

  if (req.method === "GET" && url.pathname === "/authorize") {
    record({ kind: "authorization", method: "GET", url: req.url, params: Object.fromEntries(url.searchParams) });
    const redirectUri = url.searchParams.get("redirect_uri") ?? "";
    const state = url.searchParams.get("state") ?? "";
    const target = new URL(redirectUri);
    target.searchParams.set("code", "stub-code");
    if (state) target.searchParams.set("state", state);
    res.writeHead(302, { Location: target.href });
    res.end();
    return;
  }

  if (req.method === "POST" && url.pathname === "/token") {
    const params = Object.fromEntries(new URLSearchParams(body));
    record({ kind: "token", method: "POST", url: req.url, params });
    res.writeHead(200, { "Content-Type": "application/json" });
    res.end(
      JSON.stringify({
        access_token: "stub-access-token",
        token_type: "Bearer",
        expires_in: 3600,
        scope: params.scope ?? "mcp:read",
        ...(params.grant_type === "authorization_code" ? { refresh_token: "stub-refresh-token" } : {}),
      }),
    );
    return;
  }

  if (req.method === "POST" && url.pathname === "/register") {
    const params = {};
    try {
      Object.assign(params, JSON.parse(body));
    } catch {}
    record({ kind: "registration", method: "POST", url: req.url, params });
    res.writeHead(201, { "Content-Type": "application/json" });
    res.end(
      JSON.stringify({
        client_id: "stub-dcr-client",
        client_secret: "stub-dcr-secret",
        redirect_uris: params.redirect_uris ?? ["http://localhost:0/oauth/callback"],
        grant_types: ["authorization_code", "refresh_token"],
        token_endpoint_auth_method: "none",
      }),
    );
    return;
  }

  // MCP resource server: unauthenticated → OAuth challenge.
  res.writeHead(401, {
    "WWW-Authenticate": `Bearer resource_metadata="${base}/.well-known/oauth-protected-resource"`,
  });
  res.end();
}

server.listen(port, "127.0.0.1", () => {
  const bound = server.address().port;
  if (process.env.RPI_MCP_OAUTH_PID) {
    writeFileSync(process.env.RPI_MCP_OAUTH_PID, String(process.pid));
  }
  process.stdout.write(`${bound}\n`);
});
setInterval(() => {}, 1 << 30);
