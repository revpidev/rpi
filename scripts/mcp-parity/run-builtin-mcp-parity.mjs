#!/usr/bin/env node
// G3 built-in MCP parity orchestrator (V16-08 §6 hard gate).
//
// Drives BOTH sides against the SAME fixture MCP server and diffs the
// normalized documents:
//   upstream side: builtin-upstream-runner.mjs → pinned `external/pi/packages/mcp`
//                  `McpClient` + `StdioTransport` (@ a13d35a74, READ-ONLY)
//   rpi side:      cargo example `builtin_mcp_parity_runner` (crates/rpi-mcp)
// The fixture server (`fixture-server.mjs`) records the frame transcript on
// both runs, so a diff isolates the client implementation.
//
// Coverage: the initialize handshake, `tools/list`, `tools/call` (success +
// error result), and `resources/read` frames, plus the normalized results.
//
// Usage:
//   node scripts/mcp-parity/run-builtin-mcp-parity.mjs [--out-dir <dir>]
//
// Environment:
//   RPI_MCP_PARITY_DEPS   out-of-tree npm install root (default
//                         /tmp/rpi-mcp-parity-deps); tsx may instead be
//                         resolved from external/pi/node_modules.
//   RPI_MCP_PARITY_PI     upstream pi source root (default external/pi)
//   RPI_MCP_PARITY_CARGO  cargo binary (default `cargo`)
//
// Exits non-zero when the documents differ. Reports land in
// <out-dir>/builtin-parity-report.md (default rpi/fixtures/generated/mcp-parity/,
// committed to git as the evidence chain).

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..", "..");
const DEPS = process.env.RPI_MCP_PARITY_DEPS ?? "/tmp/rpi-mcp-parity-deps";
const PI = process.env.RPI_MCP_PARITY_PI ?? join(REPO, "external", "pi");
const PI_PIN = process.env.RPI_MCP_PARITY_PI_PIN ?? "a13d35a74";
const CARGO = process.env.RPI_MCP_PARITY_CARGO ?? "cargo";

const args = process.argv.slice(2);
let outDir = join(REPO, "fixtures", "generated", "mcp-parity");
for (let i = 0; i < args.length; i++) {
  if (args[i] === "--out-dir" && args[i + 1]) outDir = resolve(args[i + 1]);
}

// tsx runs the upstream TypeScript sources directly (non-erasable syntax);
// prefer the pinned parity deps, fall back to the upstream workspace install.
const tsxCandidates = [
  join(DEPS, "node_modules", "tsx", "dist", "loader.mjs"),
  join(PI, "node_modules", "tsx", "dist", "loader.mjs"),
];
const tsxLoader = tsxCandidates.find((candidate) => existsSync(candidate));
if (!tsxLoader) {
  console.error(`tsx not found; run scripts/mcp-parity/setup-deps.sh or install external/pi deps`);
  process.exit(2);
}
if (!existsSync(join(PI, "packages", "mcp", "src", "index.ts"))) {
  console.error(`missing ${PI}/packages/mcp — the pi submodule must be checked out`);
  process.exit(2);
}

const build = spawnSync(
  CARGO,
  ["build", "-p", "rpi-mcp", "--example", "builtin_mcp_parity_runner"],
  { cwd: REPO, encoding: "utf8" },
);
if (build.status !== 0) {
  console.error(build.stdout + build.stderr);
  process.exit(2);
}
const rustRunner = join(REPO, "target", "debug", "examples", "builtin_mcp_parity_runner");

function runSide(command, argsList, env, label) {
  const result = spawnSync(command, argsList, {
    cwd: REPO,
    encoding: "utf8",
    env: { ...process.env, ...env },
    timeout: 120_000,
  });
  if (result.status !== 0) {
    return { error: `${label} exited ${result.status}\nstdout:\n${result.stdout}\nstderr:\n${result.stderr}` };
  }
  try {
    return { document: JSON.parse(result.stdout) };
  } catch (error) {
    return { error: `${label} printed non-JSON stdout: ${error}\n${result.stdout.slice(0, 2000)}` };
  }
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
function deepEqual(a, b) {
  return JSON.stringify(sortKeys(a)) === JSON.stringify(sortKeys(b));
}

mkdirSync(outDir, { recursive: true });
const sandbox = mkdtempSync(join(tmpdir(), "rpi-builtin-mcp-parity-"));
const upstreamLog = join(sandbox, "upstream-frames.log");
const rpiLog = join(sandbox, "rpi-frames.log");

let verdict;
let detail;
let upstream;
let rpi;
try {
  const common = {
    RPI_MCP_PARITY_DEPS: DEPS,
    RPI_MCP_PARITY_PI: PI,
    RPI_MCP_FIXTURE_SERVER: join(HERE, "fixture-server.mjs"),
  };
  upstream = runSide(
    process.execPath,
    ["--import", tsxLoader, join(HERE, "builtin-upstream-runner.mjs")],
    { ...common, RPI_MCP_FIXTURE_LOG: upstreamLog },
    "builtin-upstream-runner",
  );
  rpi = runSide(
    rustRunner,
    [],
    { ...common, RPI_MCP_FIXTURE_LOG: rpiLog, RPI_MCP_PARITY_NODE_PATH: process.execPath },
    "builtin_mcp_parity_runner",
  );

  if (upstream.error || rpi.error) {
    verdict = "ERROR";
    detail = (upstream.error ?? "") + (rpi.error ?? "");
  } else {
    writeFileSync(
      join(outDir, "builtin-parity-stdio-upstream.json"),
      JSON.stringify(upstream.document, null, 2) + "\n",
    );
    writeFileSync(
      join(outDir, "builtin-parity-stdio-rpi.json"),
      JSON.stringify(rpi.document, null, 2) + "\n",
    );
    const framesMatch = deepEqual(upstream.document.frames ?? [], rpi.document.frames ?? []);
    const resultsMatch = deepEqual(upstream.document.results, rpi.document.results);
    const statusMatch = upstream.document.status === rpi.document.status;
    if (framesMatch && resultsMatch && statusMatch) {
      verdict = "MATCH";
      detail = `${upstream.document.frames.length} frames, ${Object.keys(upstream.document.results).length} results, status=${upstream.document.status}`;
    } else {
      verdict = "DIFF";
      const diffs = [];
      if (!framesMatch) diffs.push("frames");
      if (!resultsMatch) diffs.push("results");
      if (!statusMatch) diffs.push(`status(${upstream.document.status} vs ${rpi.document.status})`);
      detail = diffs.join(", ");
    }
  }
} finally {
  rmSync(sandbox, { recursive: true, force: true });
}

// The frames and results live in the side documents; read them back for the
// report detail even after the sandbox is gone.
const framesUpstream = upstream?.document?.frames?.length ?? 0;
const framesRpi = rpi?.document?.frames?.length ?? 0;

const lines = [
  "# built-in MCP cross-implementation parity report (V16-08 §6 / G3)",
  "",
  `Generated: ${new Date().toISOString()} (rerun: \`node scripts/mcp-parity/run-builtin-mcp-parity.mjs\`)`,
  `Upstream: external/pi/packages/mcp @ ${PI_PIN} (McpClient + StdioTransport, tsx)`,
  `rpi: crates/rpi-mcp @ ${spawnSync("git", ["rev-parse", "--short", "HEAD"], { cwd: REPO, encoding: "utf8" }).stdout.trim()}`,
  "",
  "Normalization: JSON-RPC ids → `$id`; `clientInfo.name` → `parity-client` (O1 brand exemption); " +
    "frame transcripts recorded server-side by the shared fixture so a diff isolates the client.",
  "",
  "| Scenario | Verdict | Detail |",
  "| --- | --- | --- |",
  `| stdio (initialize/tools list/tools call/error/resource read) | ${verdict} | ${detail} |`,
  "",
  `Frames: upstream ${framesUpstream}, rpi ${framesRpi}.`,
  "",
  verdict === "MATCH" ? "Normalized documents MATCH." : "Documents differ; see builtin-parity-stdio-*.json.",
  "",
];
writeFileSync(join(outDir, "builtin-parity-report.md"), lines.join("\n"));

console.log(`builtin-mcp stdio parity: ${verdict}  ${detail}`);
process.exit(verdict === "MATCH" ? 0 : 1);