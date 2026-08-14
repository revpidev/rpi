#!/usr/bin/env node
// Cross-implementation parity: UPSTREAM (Node) side runner (design §5.2).
//
// Drives the pinned upstream `McpServerManager`
// (external/pi-mcp-adapter/server-manager.ts @ 3d953f90) against the shared
// fixture server and prints one normalized JSON result document to stdout:
//
//   { transport, frames: [...], results: {...}, status, error? }
//
// The rpi side is `scripts/mcp-parity/rust-runner` (same scenario set); the
// orchestrator (`run-mcp-parity.mjs`) diffs the two documents.
//
// Scenario steps are shared by both sides: connect → echo call → fail call
// → resource read. Frames are recorded server-side by the fixture server
// itself (RPI_MCP_FIXTURE_LOG_FRAMES=1); the runner normalizes the volatile
// JSON-RPC id sequence to `$id` so the diff is stable.
//
// Run via `node scripts/mcp-parity/run-mcp-parity.mjs` — never directly
// (it needs RPI_MCP_PARITY_DEPS and the hooks registered first).

import { register } from "node:module";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const DEPS = process.env.RPI_MCP_PARITY_DEPS;
const UPSTREAM = process.env.RPI_MCP_PARITY_UPSTREAM;
if (!DEPS || !UPSTREAM) {
  console.error("orchestrator env (RPI_MCP_PARITY_DEPS/RPI_MCP_PARITY_UPSTREAM) missing");
  process.exit(2);
}

register(new URL("./parity-hooks.mjs", import.meta.url));

const scenario = process.env.RPI_MCP_PARITY_SCENARIO ?? "stdio";
const cwd = process.env.RPI_MCP_PARITY_CWD ?? process.cwd();

const here = join(fileURLToPath(import.meta.url), "..");

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
        // O1 brand exemption: upstream `pi-mcp-<server>` vs rpi
        // `rpi-mcp-<server>` (protocol.rs client_info).
        out[key] = { ...item, name: "parity-client" };
      } else {
        out[key] = normalizeValue(item);
      }
    }
    return out;
  }
  return value;
}

async function runUpstream() {
  const { pathToFileURL } = await import("node:url");
  const { McpServerManager } = await import(pathToFileURL(join(UPSTREAM, "server-manager.ts")).href);

  const definition =
    scenario === "stdio"
      ? {
          command: process.env.RPI_MCP_PARITY_NODE_PATH ?? process.execPath,
          args: [join(here, "fixture-server.mjs")],
          env: {
            RPI_MCP_FIXTURE_LOG: process.env.RPI_MCP_FIXTURE_LOG,
            RPI_MCP_FIXTURE_LOG_FRAMES: "1",
          },
        }
      : { url: process.env.RPI_MCP_PARITY_SERVER_URL };

  const manager = new McpServerManager(cwd);
  const output = { side: "upstream", transport: scenario, frames: [], results: {}, status: "" };
  try {
    const connection = await manager.connect("fixture", definition);
    output.status = connection.status;

    const echo = await connection.client.callTool(
      { name: "echo", arguments: { query: "hello" } },
      { timeout: 10_000 },
    );
    output.results.echo = normalizeValue(echo);

    let failShape;
    try {
      const result = await connection.client.callTool(
        { name: "fail", arguments: {} },
        { timeout: 10_000 },
      );
      failShape = { threw: false, result: normalizeValue(result) };
    } catch (error) {
      failShape = { threw: true, name: error?.constructor?.name ?? "?" };
    }
    output.results.failCall = failShape;

    const resource = await connection.client.readResource(
      { uri: "fixture://config" },
      { timeout: 10_000 },
    );
    output.results.readResource = normalizeValue(resource);
  } catch (error) {
    output.status = "error";
    output.error = `${error?.constructor?.name ?? ""}: ${error?.message ?? String(error)}`;
  } finally {
    try {
      await manager.closeAll();
    } catch {}
  }
  // The fixture server (both transports) records the frame transcript
  // server-side; read it back after the connection drained.
  try {
    const raw = readFileSync(process.env.RPI_MCP_FIXTURE_LOG, "utf8");
    output.frames = raw
      .split("\n")
      .filter(Boolean)
      .map((line) => JSON.parse(line))
      .map(normalizeValue);
  } catch {
    output.frames = [];
  }
  return output;
}

const document = await runUpstream();
process.stdout.write(`${JSON.stringify(document, null, 2)}\n`);
