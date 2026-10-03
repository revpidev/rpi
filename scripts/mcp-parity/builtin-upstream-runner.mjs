#!/usr/bin/env node
// G3 built-in MCP parity: UPSTREAM (Node) side runner (V16-08 §6).
//
// Drives the pinned upstream package client `external/pi/packages/mcp`
// (@ a13d35a74, read-only) against the shared fixture server
// (`fixture-server.mjs`) and prints one normalized JSON result document to
// stdout with the same shape as the rpi side
// (`cargo run --example builtin_mcp_parity_runner -p rpi-mcp`):
//
//   { side, transport, frames: [...], results: {...}, status, error? }
//
// Steps (identical on both sides): connect → tools/list → tools/call echo →
// tools/call fail → resources/read. Frames are recorded server-side by the
// fixture (`RPI_MCP_FIXTURE_LOG_FRAMES=1`) and normalized here (`id` →
// `$id`, `clientInfo.name` → `parity-client`).
//
// Run via `node scripts/mcp-parity/run-builtin-mcp-parity.mjs` — never
// directly (it needs the tsx loader and the orchestrator env).

import { readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";
import { join } from "node:path";

const PI = process.env.RPI_MCP_PARITY_PI;
const FIXTURE = process.env.RPI_MCP_FIXTURE_SERVER;
const LOG = process.env.RPI_MCP_FIXTURE_LOG;
if (!PI || !FIXTURE || !LOG) {
  console.error("orchestrator env (RPI_MCP_PARITY_PI/RPI_MCP_FIXTURE_SERVER/RPI_MCP_FIXTURE_LOG) missing");
  process.exit(2);
}

const mcpEntry = pathToFileURL(join(PI, "packages", "mcp", "src", "index.ts")).href;
const { McpClient, StdioTransport } = await import(mcpEntry);

function normalizeValue(value) {
  if (Array.isArray(value)) return value.map(normalizeValue);
  if (value && typeof value === "object") {
    const out = {};
    for (const [key, item] of Object.entries(value)) {
      if (key === "id" && (typeof item === "number" || typeof item === "string")) {
        out[key] = "$id";
      } else if (
        key === "clientInfo" &&
        item &&
        typeof item === "object" &&
        typeof item.name === "string"
      ) {
        out[key] = { ...item, name: "parity-client" };
      } else {
        out[key] = normalizeValue(item);
      }
    }
    return out;
  }
  return value;
}

async function run() {
  const transport = new StdioTransport({
    command: process.env.RPI_MCP_PARITY_NODE_PATH ?? process.execPath,
    args: [FIXTURE],
    env: {
      RPI_MCP_FIXTURE_LOG: LOG,
      RPI_MCP_FIXTURE_LOG_FRAMES: "1",
    },
  });
  const client = new McpClient({ name: "rpi-mcp-parity", version: "1.0.0" });
  const output = { side: "upstream", transport: "stdio", frames: [], results: {}, status: "" };
  try {
    await client.connect(transport);
    output.status = "connected";

    output.results.tools = await client.listTools({ timeoutMs: 10_000 });
    output.results.echo = normalizeValue(
      await client.callTool("echo", { query: "hello" }, { timeoutMs: 10_000 }),
    );

    let failShape;
    try {
      const result = await client.callTool("fail", {}, { timeoutMs: 10_000 });
      failShape = { threw: false, result: normalizeValue(result) };
    } catch (error) {
      failShape = { threw: true, name: error?.constructor?.name ?? "?" };
    }
    output.results.failCall = failShape;

    output.results.readResource = normalizeValue(
      await client.readResource("fixture://config", { timeoutMs: 10_000 }),
    );
  } catch (error) {
    output.status = "error";
    output.error = `${error?.constructor?.name ?? ""}: ${error?.message ?? String(error)}`;
  } finally {
    try {
      await client.close();
    } catch {}
  }

  try {
    output.frames = readFileSync(LOG, "utf8")
      .split("\n")
      .filter(Boolean)
      .map((line) => JSON.parse(line))
      .map(normalizeValue);
  } catch {
    output.frames = [];
  }
  return output;
}

process.stdout.write(`${JSON.stringify(await run(), null, 2)}\n`);