#!/usr/bin/env node
// Cross-implementation parity orchestrator (design §5.2, TE02 self-check
// item "同脚本驱动上游 Node 侧并 diff 帧序列与结果 JSON").
//
// For each scenario, drives BOTH clients against the SAME fixture server
// and diffs the normalized documents:
//   upstream side: upstream-runner.mjs  → pinned McpServerManager (Node)
//   rpi side:      cargo example parity_runner → this crate's manager
// Frame transcripts are recorded by the fixture server itself (normalized
// to `$id`), so the diff isolates client-implementation differences.
//
// Scenarios:
//   stdio          Node fixture server over stdin/stdout
//   http-streamable / http-fallback-404|405|406|415 / http-auth-401
//                  fixture server in HTTP mode; both clients connect to it
//
// Usage:
//   node scripts/mcp-parity/run-mcp-parity.mjs [--out-dir <dir>]
//
// Environment:
//   RPI_MCP_PARITY_DEPS   out-of-tree npm install root
//                         (default /tmp/rpi-mcp-parity-deps; created by
//                         scripts/mcp-parity/setup-deps.sh)
//   RPI_MCP_PARITY_CARGO  cargo binary (default `cargo`)
//
// Exits non-zero when any scenario's documents differ. Reports are written
// to <out-dir>/parity-report.md (default
// rpi/fixtures/generated/mcp-parity/, checked into git as the evidence
// chain).

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..", "..");
const UPSTREAM = join(REPO, "external", "pi-mcp-adapter");
const DEPS = process.env.RPI_MCP_PARITY_DEPS ?? "/tmp/rpi-mcp-parity-deps";
const CARGO = process.env.RPI_MCP_PARITY_CARGO ?? "cargo";

const args = process.argv.slice(2);
let outDir = join(REPO, "fixtures", "generated", "mcp-parity");
for (let i = 0; i < args.length; i++) {
  if (args[i] === "--out-dir" && args[i + 1]) outDir = resolve(args[i + 1]);
}

const SCENARIOS = [
  { name: "stdio", transport: "stdio" },
  { name: "http-streamable", transport: "http", profile: "streamable-json" },
  { name: "http-fallback-404", transport: "http", profile: "fallback-404" },
  { name: "http-fallback-405", transport: "http", profile: "fallback-405" },
  { name: "http-fallback-406", transport: "http", profile: "fallback-406" },
  { name: "http-fallback-415", transport: "http", profile: "fallback-415" },
  { name: "http-auth-401", transport: "http", profile: "auth-401" },
];

if (!existsSync(join(DEPS, "node_modules"))) {
  console.error(`Missing out-of-tree deps at ${DEPS}; run scripts/mcp-parity/setup-deps.sh first`);
  process.exit(2);
}

const tsxLoader = join(DEPS, "node_modules", "tsx", "dist", "loader.mjs");
if (!existsSync(tsxLoader)) {
  console.error(`tsx not installed under ${DEPS}; run scripts/mcp-parity/setup-deps.sh`);
  process.exit(2);
}

// Build the Rust runner once.
const build = spawnSync(CARGO, ["build", "-p", "rpi-ext-mcp-adapter", "--example", "parity_runner"], {
  cwd: REPO,
  encoding: "utf8",
});
if (build.status !== 0) {
  console.error(build.stdout + build.stderr);
  process.exit(2);
}
const rustRunner = join(REPO, "target", "debug", "examples", "parity_runner");

function runSide(command, argsList, env, label) {
  const result = spawnSync(command, argsList, {
    cwd: REPO,
    encoding: "utf8",
    env: { ...process.env, ...env },
    timeout: 120_000,
  });
  if (result.status !== 0) {
    return {
      error: `${label} exited ${result.status}\nstdout:\n${result.stdout}\nstderr:\n${result.stderr}`,
    };
  }
  try {
    return { document: JSON.parse(result.stdout) };
  } catch (error) {
    return { error: `${label} printed non-JSON stdout: ${error}\n${result.stdout.slice(0, 2000)}` };
  }
}

// Spawn the HTTP fixture on an ephemeral port (0) and read the chosen port
// from its first stdout line (avoids the bind/close reuse race).
import { spawn as spawnChild } from "node:child_process";
function startHttpFixture(profile, logPath) {
  const child = spawnChild(process.execPath, [join(HERE, "fixture-server.mjs")], {
    env: {
      ...process.env,
      RPI_MCP_FIXTURE_MODE: "http",
      RPI_MCP_FIXTURE_HTTP_PROFILE: profile,
      RPI_MCP_FIXTURE_PORT: "0",
      RPI_MCP_FIXTURE_LOG: logPath,
      RPI_MCP_FIXTURE_LOG_FRAMES: "1",
    },
    stdio: ["pipe", "pipe", "inherit"],
  });
  return new Promise((resolvePromise, reject) => {
    let buffer = "";
    const timer = setTimeout(() => reject(new Error("fixture http server did not report a port")), 10_000);
    child.stdout.on("data", (chunk) => {
      buffer += chunk;
      const index = buffer.indexOf("\n");
      if (index !== -1) {
        const port = Number(buffer.slice(0, index).trim());
        clearTimeout(timer);
        resolvePromise({ child, url: `http://127.0.0.1:${port}/mcp` });
      }
    });
    child.on("exit", () => clearTimeout(timer));
  });
}

function deepEqual(a, b) {
  return JSON.stringify(sortKeys(a)) === JSON.stringify(sortKeys(b));
}
function sortKeys(value) {
  if (Array.isArray(value)) return value.map(sortKeys);
  if (value && typeof value === "object") {
    return Object.fromEntries(
      Object.keys(value)
        .sort()
        .map((key) => [key, sortKeys(value[key])]),
    );
  }
  return value;
}

// Upstream discovery is `Promise.all`-concurrent (server-manager.ts:458-462),
// so the wire arrival order of tools/list / resources/list / prompts/list is
// scheduler-dependent (observed varying run-to-run); the rpi port sends them
// sequentially. JSON-RPC list requests are independent, so wire equivalence
// is the SET of discovery frames. Normalize contiguous discovery runs by
// method for comparison only — transcripts on disk stay verbatim.
const DISCOVERY_METHODS = new Set(["tools/list", "resources/list", "prompts/list"]);
function normalizeDiscoveryOrder(frames) {
  const out = [];
  let run = [];
  const flush = () => {
    if (run.length > 0) {
      run.sort((a, b) => (a.method ?? "").localeCompare(b.method ?? ""));
      out.push(...run);
      run = [];
    }
  };
  for (const frame of frames) {
    if (DISCOVERY_METHODS.has(frame?.method)) run.push(frame);
    else {
      flush();
      out.push(frame);
    }
  }
  flush();
  return out;
}

mkdirSync(outDir, { recursive: true });
const report = [];
let failed = 0;

for (const scenario of SCENARIOS) {
  const sandbox = mkdtempSync(join(tmpdir(), "rpi-mcp-parity-"));
  const logPath = join(sandbox, "frames.log");
  let fixture = null;
  let serverUrl = "";
  try {
    if (scenario.transport === "http") {
      fixture = await startHttpFixture(scenario.profile, logPath);
      serverUrl = fixture.url;
    }

    const sharedEnv = {
      RPI_MCP_PARITY_DEPS: DEPS,
      RPI_MCP_PARITY_UPSTREAM: UPSTREAM,
      RPI_MCP_FIXTURE_LOG: logPath,
    };
    const upstream = runSide(
      process.execPath,
      ["--import", tsxLoader, join(HERE, "upstream-runner.mjs")],
      {
        ...sharedEnv,
        RPI_MCP_PARITY_SCENARIO: scenario.transport,
        RPI_MCP_PARITY_SERVER_URL: serverUrl,
        RPI_MCP_PARITY_CWD: sandbox,
        // Upstream's in-memory credential store (same as its own
        // conformance driver.sh): keeps the auth scenarios independent of
        // the host OS keyring.
        PI_MCP_ADAPTER_TEST_AUTH_STORE: "memory",
      },
      "upstream-runner",
    );

    rmSync(logPath, { force: true });

    const rust = runSide(
      rustRunner,
      [],
      {
        ...sharedEnv,
        RPI_MCP_PARITY_SCENARIO: scenario.transport,
        RPI_MCP_PARITY_SERVER_URL: serverUrl,
        RPI_MCP_PARITY_FIXTURE_SERVER: join(HERE, "fixture-server.mjs"),
      },
      "parity_runner",
    );

    let verdict;
    let detail;
    if (upstream.error || rust.error) {
      verdict = "ERROR";
      detail = (upstream.error ?? "") + (rust.error ?? "");
      failed++;
    } else {
      writeFileSync(join(outDir, `parity-${scenario.name}-upstream.json`), JSON.stringify(upstream.document, null, 2) + "\n");
      writeFileSync(join(outDir, `parity-${scenario.name}-rpi.json`), JSON.stringify(rust.document, null, 2) + "\n");
      const framesMatch = deepEqual(
        normalizeDiscoveryOrder(upstream.document.frames ?? []),
        normalizeDiscoveryOrder(rust.document.frames ?? []),
      );
      const resultsMatch = deepEqual(upstream.document.results, rust.document.results);
      // auth-401 expected-diff (P0 scope cut, FR-P0-08): upstream's 401
      // handling enters the OAuth discovery flow during connect (fails here:
      // the stub resource_metadata points at an unreachable port, error is
      // probe-enriched), while the P0 rpi port stops at the needs-auth
      // connection status; the OAuth continuation is TE03 parity scope.
      const statusMatch =
        scenario.name === "http-auth-401"
          ? upstream.document.status === "error" && rust.document.status === "needs-auth"
          : upstream.document.status === rust.document.status;
      if (framesMatch && resultsMatch && statusMatch) {
        verdict = "MATCH";
        detail = `${upstream.document.frames.length} frames, status=${upstream.document.status}`;
      } else {
        verdict = "DIFF";
        failed++;
        const diffs = [];
        if (!framesMatch) diffs.push("frames");
        if (!resultsMatch) diffs.push("results");
        if (!statusMatch) diffs.push(`status(${upstream.document.status} vs ${rust.document.status})`);
        detail = diffs.join(", ");
      }
    }
    report.push({ scenario: scenario.name, verdict, detail });
    console.log(`${scenario.name.padEnd(20)} ${verdict}  ${detail}`);
  } finally {
    if (fixture) fixture.child.kill();
    rmSync(sandbox, { recursive: true, force: true });
  }
}

const lines = [
  "# MCP adapter cross-implementation parity report (design §5.2)",
  "",
  `Generated: ${new Date().toISOString()} (rerun: \`node scripts/mcp-parity/run-mcp-parity.mjs\`)`,
  `Upstream: pi-mcp-adapter @ 3d953f90 (server-manager.ts, McpServerManager)`,
  `rpi: crates/rpi-ext-mcp-adapter @ ${spawnSync("git", ["rev-parse", "--short", "HEAD"], { cwd: REPO, encoding: "utf8" }).stdout.trim()} (uncommitted working tree)`,
  "",
  "Normalization: JSON-RPC ids → `$id`; frame transcripts recorded by the shared fixture server.",
  "",
  "| Scenario | Verdict | Detail |",
  "| --- | --- | --- |",
  ...report.map((r) => `| ${r.scenario} | ${r.verdict} | ${r.detail} |`),
  "",
  failed === 0 ? "All scenarios MATCH." : `${failed} scenario(s) differ; see parity-*.json side documents.`,
  "",
];
writeFileSync(join(outDir, "parity-report.md"), lines.join("\n"));

process.exit(failed === 0 ? 0 : 1);
