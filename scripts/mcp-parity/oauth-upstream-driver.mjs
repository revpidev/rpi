#!/usr/bin/env node
// OAuth parity: UPSTREAM (Node) side driver (TE02 self-check item 5).
//
// Drives the pinned upstream authorization-code flow
// (external/pi-mcp-adapter mcp-auth-flow.ts startAuth +
// completeAuthFromInput, on the SDK 2.0 auth primitives) against the stub
// authorization server, then prints the normalized transcript of what the
// flow sent: authorization URL params + token request params. The rpi side
// is the `oauth_parity_runner` crate example; the orchestrator diffs.
//
// Normalization (volatile per-run values):
//   code_challenge / state        → hashed to a stable marker (length)
//   redirect_uri port             → $port (ephemeral callback port)
//   code / code_verifier          → $pkce (stub-issued / random)
//   client_id                     → kept (stub-issued "stub-dcr-client"
//                                    after DCR, or the configured id)
//
// Env (set by the orchestrator):
//   RPI_MCP_PARITY_DEPS / RPI_MCP_PARITY_UPSTREAM / RPI_MCP_PARITY_HOOKS
//   RPI_MCP_OAUTH_SERVER_URL   stub AS base URL
//   RPI_MCP_OAUTH_TRANSCRIPT   stub AS transcript path

import { register } from "node:module";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const DEPS = process.env.RPI_MCP_PARITY_DEPS;
const UPSTREAM = process.env.RPI_MCP_PARITY_UPSTREAM;
const serverUrl = process.env.RPI_MCP_OAUTH_SERVER_URL;
const transcriptPath = process.env.RPI_MCP_OAUTH_TRANSCRIPT;
if (!DEPS || !UPSTREAM || !serverUrl || !transcriptPath) {
  console.error("orchestrator env missing");
  process.exit(2);
}

register(pathToFileURL(join(fileURLToPath(import.meta.url), "..", "parity-hooks.mjs")).href);

const normalizeMap = {
  code_challenge: (v) => (typeof v === "string" && v.length >= 40 ? "$challenge" : v),
  state: (v) => (typeof v === "string" && v.length >= 8 ? "$state" : v),
  redirect_uri: (v) =>
    typeof v === "string" ? v.replace(/localhost:\d+/, "localhost:$port") : v,
  code: (v) => (v === "stub-code" ? "$code" : v),
  code_verifier: (v) => (typeof v === "string" && v.length >= 40 ? "$verifier" : v),
  resource: (v) => (typeof v === "string" ? v.replace(/localhost:\d+/, "localhost:$asport") : v),
  redirect_uris: (v) =>
    Array.isArray(v) ? v.map((u) => String(u).replace(/localhost:\d+/, "localhost:$port")) : v,
  // O1 brand exemption (design §6): upstream registers as "Pi Coding Agent"
  // with the adapter repo as client_uri; rpi uses its own product identity.
  client_name: () => "$client_name",
  client_uri: () => "$client_uri",
};

function normalizeParams(params) {
  const out = {};
  for (const [key, value] of Object.entries(params ?? {})) {
    out[key] = normalizeMap[key] ? normalizeMap[key](value) : value;
  }
  return out;
}

const SERVER_NAME = "fixture-oauth";

async function main() {
  const { startAuth, completeAuthFromInput, initializeOAuth, shutdownOAuth } = await import(
    pathToFileURL(join(UPSTREAM, "mcp-auth-flow.ts")).href
  );

  const definition = { url: serverUrl, oauth: {} };
  const runtime = await initializeOAuth();
  let authorizationUrl = "";
  let callbackUrl = "";
  let status = "";
  try {
    const started = await startAuth(SERVER_NAME, serverUrl, definition, {
      runtime,
      authStorageOptions: {},
    });
    authorizationUrl = started.authorizationUrl;
    if (!authorizationUrl) throw new Error("startAuth returned an empty authorization URL");

    // Play the browser: follow the authorization URL (stub AS 302s into the
    // adapter's real localhost callback server), then complete the flow with
    // the full redirect URL (state validation + code exchange).
    const authResponse = await fetch(authorizationUrl, { redirect: "manual" });
    const location = authResponse.headers.get("location");
    if (!location) {
      throw new Error(`stub AS did not redirect (status ${authResponse.status})`);
    }
    callbackUrl = new URL(location, authorizationUrl).toString();
    const callbackResponse = await fetch(callbackUrl, { redirect: "manual" });
    if (callbackResponse.status >= 400) {
      throw new Error(`callback failed with ${callbackResponse.status}`);
    }
    status = await completeAuthFromInput(SERVER_NAME, callbackUrl, { runtime, authStorageOptions: {} });
  } finally {
    try {
      await shutdownOAuth(runtime);
    } catch {}
  }

  const entries = readFileSync(transcriptPath, "utf8")
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line))
    .map((entry) => ({ kind: entry.kind, params: normalizeParams(entry.params) }));

  process.stdout.write(
    `${JSON.stringify({ side: "upstream", status, authorizationUrl: undefined, entries }, null, 2)}\n`,
  );
}

main().catch((error) => {
  console.error(error?.stack ?? String(error));
  process.exit(1);
});
