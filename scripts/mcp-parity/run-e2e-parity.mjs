#!/usr/bin/env node
// End-to-end parity (design §5.3): the SAME fixture MCP server config drives
// `pi -p` (upstream CLI + npm-hosted pi-mcp-adapter) and `rpi -p` (this
// repo's CLI + the native rpi-ext-mcp-adapter cdylib), each running the
// five proxy-tool modes, then diffs the normalized tool-result texts.
//
// Both CLIs run in isolated sandboxes (separate HOME/agent dirs, temp cwd
// with the .mcp.json fixture config). The frame transcripts recorded by the
// fixture server serve as the protocol-level evidence; the tool-result text
// diff is the user-visible parity surface.
//
// Prerequisites (documented as conclusions in the TE02 task file):
//   - `pi` on PATH (upstream 0.84.x with the npm-hosted adapter installed)
//   - rpi built (`cargo build --workspace`) — needs target/debug/rpi and
//     target/debug/librpi_ext_mcp_adapter.so
//   - a working model provider auth for BOTH CLIs (~/.rpi/agent/auth.json
//     is copied into the rpi sandbox; pi uses its own login state)
//
// Normalization for the diff:
//   - strip the model's surrounding prose: keep only the fenced ``` block
//     (or the text after the first blank line) — the models differ
//   - toolPrefix is identical on both sides (default "server")
//
// Usage: node scripts/mcp-parity/run-e2e-parity.mjs
// Report: rpi/fixtures/generated/mcp-parity/e2e-parity.md

import { spawnSync } from "node:child_process";
import { copyFileSync, cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..", "..");
const OUT_DIR = join(REPO, "fixtures", "generated", "mcp-parity");

const MODES = [
  { key: "list", prompt: 'Use the mcp tool with { "server": "fixture" } and report the raw text of the tool result verbatim.' },
  { key: "search", prompt: 'Use the mcp tool with { "search": "echo" } and report the raw text of the tool result verbatim.' },
  { key: "describe", prompt: 'Use the mcp tool with { "describe": "fixture_echo" } and report the raw text of the tool result verbatim.' },
  { key: "call", prompt: 'Use the mcp tool with { "tool": "fixture_echo", "args": { "query": "pong" } } and report the raw text of the tool result verbatim.' },
  { key: "status", prompt: 'Use the mcp tool with { "status": true } and report the raw text of the tool result verbatim.' },
];

function extractToolResult(output) {
  // The model wraps the verbatim tool text in a fenced block; fall back to
  // everything after the first "verbatim" marker.
  const fence = output.match(/```[a-z]*\n([\s\S]*?)```/);
  if (fence) return fence[1].trim();
  const marker = output.indexOf("verbatim:");
  if (marker !== -1) return output.slice(marker + "verbatim:".length).trim();
  return output.trim();
}

function runCli(label, command, argsList, cwd, env, timeoutMs) {
  const result = spawnSync(command, argsList, { cwd, encoding: "utf8", env: { ...process.env, ...env }, timeout: timeoutMs });
  if (result.status !== 0) {
    return { error: `${label} exited ${result.status}\n${result.stdout}\n${result.stderr}`.slice(0, 3000) };
  }
  return { text: extractToolResult(result.stdout) };
}

const rows = [];
let failed = 0;

for (const mode of MODES) {
  // upstream side — PI_CODING_AGENT_DIR isolates the agent dir (fresh
  // mcp-cache.json → bootstrap-all on both sides, same as the rpi sandbox).
  // The npm package store is COPIED from the real HOME (read-only evidence
  // into the sandbox, same policy as the auth file — a symlink would make
  // the sandbox's package-manager writes pierce the real HOME) and the
  // login state is copied.
  const upSandbox = mkdtempSync(join(tmpdir(), "rpi-e2e-pi-"));
  const upAgent = join(upSandbox, "agent-home");
  mkdirSync(upAgent, { recursive: true });
  const piNpm = join(process.env.HOME ?? "", ".pi", "agent", "npm");
  if (existsSync(piNpm)) {
    cpSync(piNpm, join(upAgent, "npm"), { recursive: true });
    writeFileSync(
      join(upAgent, "settings.json"),
      `${JSON.stringify({ packages: ["npm:pi-mcp-adapter"] }, null, 2)}\n`,
    );
  }
  const piAuth = join(process.env.HOME ?? "", ".pi", "agent", "auth.json");
  if (existsSync(piAuth)) copyFileSync(piAuth, join(upAgent, "auth.json"));
  writeFileSync(join(upSandbox, ".mcp.json"), fixtureConfig(upSandbox));
  const up = runCli(
    `pi-${mode.key}`,
    "pi",
    ["-p", mode.prompt],
    upSandbox,
    { PI_CODING_AGENT_DIR: upAgent },
    120_000,
  );

  // rpi side
  const rpSandbox = mkdtempSync(join(tmpdir(), "rpi-e2e-rpi-"));
  const agent = join(rpSandbox, "agent");
  mkdirSync(join(agent, "extensions", "rpi-mcp-adapter"), { recursive: true });
  const so = join(REPO, "target", "debug", "librpi_ext_mcp_adapter.so");
  if (!existsSync(so)) {
    console.error(`missing ${so} — run cargo build --workspace`);
    process.exit(2);
  }
  copyFileSync(so, join(agent, "extensions", "rpi-mcp-adapter", "librpi_ext_mcp_adapter.so"));
  copyFileSync(
    join(REPO, "crates", "rpi-ext-mcp-adapter", "rpi-extension.json"),
    join(agent, "extensions", "rpi-mcp-adapter", "rpi-extension.json"),
  );
  const auth = join(process.env.HOME ?? "", ".rpi", "agent", "auth.json");
  if (existsSync(auth)) copyFileSync(auth, join(agent, "auth.json"));
  writeFileSync(join(rpSandbox, ".mcp.json"), fixtureConfig(rpSandbox));
  const rp = runCli(
    `rpi-${mode.key}`,
    join(REPO, "target", "debug", "rpi"),
    ["-p", mode.prompt],
    rpSandbox,
    { RPI_CODING_AGENT_DIR: agent },
    120_000,
  );

  let verdict;
  let detail;
  if (up.error || rp.error) {
    verdict = "ERROR";
    detail = (up.error ?? "") + (rp.error ?? "");
    failed++;
  } else if (up.text === rp.text) {
    verdict = "MATCH";
    detail = `${up.text.split("\n").length} lines`;
  } else {
    verdict = "DIFF";
    detail = "see e2e-parity.md";
    failed++;
  }
  rows.push({ mode: mode.key, verdict, detail, up, rp });
  console.log(`${mode.key.padEnd(10)} ${verdict}  ${verdict === "MATCH" ? detail : ""}`);
  rmSync(upSandbox, { recursive: true, force: true });
  rmSync(rpSandbox, { recursive: true, force: true });
}

function fixtureConfig(sandbox) {
  return `${JSON.stringify(
    {
      mcpServers: {
        fixture: {
          command: "node",
          args: [join(HERE, "fixture-server.mjs")],
          env: {
            RPI_MCP_FIXTURE_LOG: join(sandbox, "frames.log"),
            RPI_MCP_FIXTURE_LOG_FRAMES: "1",
          },
        },
      },
    },
    null,
    2,
  )}\n`;
}

mkdirSync(OUT_DIR, { recursive: true });
const lines = [
  "# End-to-end parity report (design §5.3)",
  "",
  `Generated: ${new Date().toISOString()} (rerun: \`node scripts/mcp-parity/run-e2e-parity.mjs\`)`,
  "Upstream: `pi -p` 0.84.x + npm-hosted pi-mcp-adapter",
  "rpi: `target/debug/rpi -p` + native rpi-ext-mcp-adapter",
  "Normalization: fenced verbatim block of the model reply (models/prose differ).",
  "",
  "| Mode | Verdict | Detail |",
  "| --- | --- | --- |",
  ...rows.map((r) => `| ${r.mode} | ${r.verdict} | ${r.verdict === "MATCH" ? r.detail : "diff below"} |`),
  "",
];
for (const row of rows) {
  if (row.verdict === "DIFF" || row.verdict === "ERROR") {
    lines.push(`## ${row.mode}`, "", "### upstream", "", "```", String(row.up.error ?? row.up.text), "```", "");
    lines.push("### rpi", "", "```", String(row.rp.error ?? row.rp.text), "```", "");
  }
}
lines.push(failed === 0 ? "All five modes MATCH." : `${failed} mode(s) differ.`, "");
writeFileSync(join(OUT_DIR, "e2e-parity.md"), lines.join("\n"));
process.exit(failed === 0 ? 0 : 1);
