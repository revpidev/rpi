#!/usr/bin/env node
// OAuth cross-implementation parity orchestrator (TE02 self-check item 5 /
// TE03 G3 groundwork): runs BOTH OAuth authorization-code+PKCE flows
// against one stub authorization server and diffs the normalized request
// transcripts (authorization URL params + token request form params).
//
//   upstream side: oauth-upstream-driver.mjs → pinned mcp-auth-flow.ts
//                  startAuth + completeAuthFromInput (SDK 2.0 auth)
//   rpi side:      cargo example oauth_parity_runner → oauth.rs authenticate
//
// Normalization (volatile per-run values → markers):
//   code_challenge / state / code_verifier / code → $challenge / $state /
//     $verifier / $code (random or stub-issued)
//   redirect_uri port → $port (ephemeral callback listener)
//   resource host port → $asport (ephemeral stub AS)
//
// Usage: node scripts/mcp-parity/run-oauth-parity.mjs
// Report: rpi/fixtures/generated/mcp-parity/oauth-parity.md

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { createServer } from "node:net";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..", "..");
const UPSTREAM = join(REPO, "external", "pi-mcp-adapter");
const DEPS = process.env.RPI_MCP_PARITY_DEPS ?? "/tmp/rpi-mcp-parity-deps";
const CARGO = process.env.RPI_MCP_PARITY_CARGO ?? "cargo";
const OUT_DIR = join(REPO, "fixtures", "generated", "mcp-parity");

if (!existsSync(join(DEPS, "node_modules"))) {
  console.error(`Missing out-of-tree deps at ${DEPS}; run scripts/mcp-parity/setup-deps.sh first`);
  process.exit(2);
}

const build = spawnSync(CARGO, ["build", "-p", "rpi-ext-mcp-adapter", "--example", "oauth_parity_runner"], {
  cwd: REPO,
  encoding: "utf8",
});
if (build.status !== 0) {
  console.error(build.stdout + build.stderr);
  process.exit(2);
}

const freePort = () =>
  new Promise((resolvePromise, reject) => {
    const server = createServer();
    server.listen(0, "127.0.0.1", () => {
      const port = server.address().port;
      server.close(() => resolvePromise(port));
    });
    server.on("error", reject);
  });

function spawnJson(command, argsList, env, label) {
  const result = spawnSync(command, argsList, { cwd: REPO, encoding: "utf8", env: { ...process.env, ...env }, timeout: 60_000 });
  if (result.status !== 0) {
    return { error: `${label} exited ${result.status}\n${result.stdout}\n${result.stderr}` };
  }
  try {
    return { document: JSON.parse(result.stdout) };
  } catch (error) {
    return { error: `${label} non-JSON stdout: ${error}\n${result.stdout.slice(0, 2000)}\n${result.stderr.slice(0, 2000)}` };
  }
}

mkdirSync(OUT_DIR, { recursive: true });
// The stub AS must run concurrently with the drivers — spawn it detached
// with a manual process handle instead of spawnSync.
import { spawn } from "node:child_process";

const sandbox = mkdtempSync(join(tmpdir(), "rpi-oauth-parity-"));
const transcript = join(sandbox, "transcript.jsonl");
const stubPort = await freePort();
const stub = spawn(process.execPath, [join(HERE, "oauth-stub-server.mjs")], {
  env: { ...process.env, RPI_MCP_OAUTH_PORT: String(stubPort), RPI_MCP_OAUTH_TRANSCRIPT: transcript },
  stdio: ["ignore", "ignore", "inherit"],
});
const asUrl = `http://127.0.0.1:${stubPort}`;
await new Promise((r) => setTimeout(r, 500));

let upstream;
let rpi;
try {
  upstream = spawnJson(
    process.execPath,
    ["--import", join(DEPS, "node_modules", "tsx", "dist", "loader.mjs"), join(HERE, "oauth-upstream-driver.mjs")],
    {
      RPI_MCP_PARITY_DEPS: DEPS,
      RPI_MCP_PARITY_UPSTREAM: UPSTREAM,
      RPI_MCP_OAUTH_SERVER_URL: asUrl,
      RPI_MCP_OAUTH_TRANSCRIPT: transcript,
      PI_MCP_ADAPTER_TEST_AUTH_STORE: "memory",
    },
    "oauth-upstream-driver",
  );

  rmSync(transcript, { force: true });
  const rpiStore = join(sandbox, "rpi-store");
  mkdirSync(rpiStore, { recursive: true });
  rpi = spawnJson(
    join(REPO, "target", "debug", "examples", "oauth_parity_runner"),
    [],
    {
      RPI_MCP_OAUTH_SERVER_URL: asUrl,
      RPI_MCP_OAUTH_TRANSCRIPT: transcript,
      RPI_MCP_OAUTH_STORE_DIR: rpiStore,
    },
    "oauth_parity_runner",
  );
} finally {
  stub.kill();
}

const lines = [];
let verdict;
if (upstream.error || rpi.error) {
  verdict = "ERROR";
  lines.push(upstream.error ?? "", rpi.error ?? "");
} else {
  writeFileSync(join(OUT_DIR, "oauth-parity-upstream.json"), JSON.stringify(upstream.document, null, 2) + "\n");
  writeFileSync(join(OUT_DIR, "oauth-parity-rpi.json"), JSON.stringify(rpi.document, null, 2) + "\n");
  // Key-order-insensitive comparison: the stub AS derives params from
  // URLSearchParams / form parsing, so object key order carries no
  // semantics.
  const canon = (value) =>
    JSON.stringify(value, (k, v) =>
      v && typeof v === "object" && !Array.isArray(v)
        ? Object.fromEntries(Object.keys(v).sort().map((key) => [key, v[key]]))
        : v,
    );
  verdict = canon(upstream.document.entries) === canon(rpi.document.entries) ? "MATCH" : "DIFF";
}

console.log(`oauth-authorization-code  ${verdict}`);
const report = [
  "# OAuth cross-implementation parity report (TE02 item 5 / TE03 groundwork)",
  "",
  `Generated: ${new Date().toISOString()} (rerun: \`node scripts/mcp-parity/run-oauth-parity.mjs\`)`,
  `Upstream: pi-mcp-adapter @ 3d953f90 (mcp-auth-flow.ts via SDK 2.0 auth)`,
  `rpi: crates/rpi-ext-mcp-adapter oauth.rs`,
  "",
  "Stub AS transcript (authorization URL params + token form params),",
  "normalized: challenge/state/verifier/code → markers, ports → $port/$asport.",
  "",
  `Verdict: ${verdict}`,
  "",
];
if (upstream.error || rpi.error) {
  report.push("## Errors", "", "```", String(upstream.error ?? ""), String(rpi.error ?? ""), "```");
} else {
  report.push("## Upstream entries", "", "```json", JSON.stringify(upstream.document.entries, null, 2), "```", "");
  report.push("## rpi entries", "", "```json", JSON.stringify(rpi.document.entries, null, 2), "```");
}
writeFileSync(join(OUT_DIR, "oauth-parity.md"), report.join("\n") + "\n");

rmSync(sandbox, { recursive: true, force: true });
process.exit(verdict === "MATCH" ? 0 : 1);
