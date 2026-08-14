#!/usr/bin/env node
// Node twin of `crates/rpi-ext-mcp-adapter/examples/fixture_stdio_server.rs`
// for the cross-implementation parity harness (design §5.2).
//
// Both sides of the parity run (upstream Node McpServerManager and the rpi
// Rust crate) talk to byte-identical fixture servers, so any difference in
// the recorded frame transcripts or result JSON is attributable to the
// client implementations alone.
//
// Responses mirror the Rust fixture server exactly (initialize → 2025-03-26
// + resources/prompts capabilities + "fixture instructions"; tools echo/fail;
// fixture://config resource; standup prompt; -32601 for anything else).
//
// Modes:
//   RPI_MCP_FIXTURE_MODE=stdio (default)  JSON-RPC over stdin/stdout
//   RPI_MCP_FIXTURE_MODE=http             HTTP server on RPI_MCP_FIXTURE_PORT:
//     RPI_MCP_FIXTURE_HTTP_PROFILE=streamable-json  POST /mcp → JSON body,
//       Mcp-Session-Id: sess-1 on initialize
//     RPI_MCP_FIXTURE_HTTP_PROFILE=fallback-404|405|406|415
//       POST /mcp → <status>; legacy SSE: GET /sse endpoint event →
//       POST /message?sessionId=… → 202 + response on the SSE stream
//     RPI_MCP_FIXTURE_HTTP_PROFILE=auth-401
//       POST /mcp → 401 + WWW-Authenticate resource_metadata (needs-auth)
//
// Env: RPI_MCP_FIXTURE_LOG (frame transcript; RPI_MCP_FIXTURE_LOG_FRAMES=1
// → full JSON frames, one per line), RPI_MCP_FIXTURE_PID.

import http from "node:http";
import { appendFileSync, writeFileSync } from "node:fs";

const mode = process.env.RPI_MCP_FIXTURE_MODE ?? "stdio";
const httpProfile = process.env.RPI_MCP_FIXTURE_HTTP_PROFILE ?? "streamable-json";
const port = Number(process.env.RPI_MCP_FIXTURE_PORT ?? "0");
const logPath = process.env.RPI_MCP_FIXTURE_LOG;
const logFrames = process.env.RPI_MCP_FIXTURE_LOG_FRAMES === "1";

function logFrame(raw) {
  if (!logPath) return;
  if (logFrames) {
    appendFileSync(logPath, `${JSON.stringify(raw)}\n`);
  } else {
    appendFileSync(logPath, `${raw.method ?? String(raw)}\n`);
  }
}

if (process.env.RPI_MCP_FIXTURE_PID) {
  writeFileSync(process.env.RPI_MCP_FIXTURE_PID, String(process.pid));
}

const METHOD_NOT_FOUND = Symbol("method-not-found");

function resultFor(message) {
  const method = message.method ?? "";
  switch (method) {
    case "initialize":
      return {
        protocolVersion: "2025-03-26",
        capabilities: { tools: {}, resources: {}, prompts: {} },
        serverInfo: { name: "fixture", version: "0.1" },
        instructions: "fixture instructions",
      };
    case "ping":
      return {};
    case "tools/list":
      return {
        tools: [
          {
            name: "echo",
            description: "Echo the query back",
            inputSchema: {
              type: "object",
              properties: { query: { type: "string" } },
              required: ["query"],
            },
          },
          {
            name: "fail",
            description: "Always fails",
            inputSchema: { type: "object", properties: {} },
          },
        ],
      };
    case "tools/call": {
      const name = message.params?.name ?? "";
      if (name === "fail") {
        return { isError: true, content: [{ type: "text", text: "boom" }] };
      }
      return { content: [{ type: "text", text: message.params?.arguments?.query ?? "" }] };
    }
    case "resources/list":
      return { resources: [{ uri: "fixture://config", name: "Config" }] };
    case "resources/read":
      return { contents: [{ uri: "fixture://config", text: "resource-body" }] };
    case "prompts/list":
      return { prompts: [{ name: "standup", description: "Standup notes" }] };
    default:
      return METHOD_NOT_FOUND;
  }
}

function respond(message) {
  const { id } = message;
  if (id === undefined || id === null) return undefined;
  const result = resultFor(message);
  if (result === METHOD_NOT_FOUND) {
    return {
      jsonrpc: "2.0",
      id,
      error: { code: -32601, message: "Method not found" },
    };
  }
  return { jsonrpc: "2.0", id, result };
}

if (mode === "stdio") {
  let buffer = "";
  process.stdin.setEncoding("utf8");
  process.stdin.on("data", (chunk) => {
    buffer += chunk;
    let index;
    while ((index = buffer.indexOf("\n")) !== -1) {
      const line = buffer.slice(0, index);
      buffer = buffer.slice(index + 1);
      handleMessage(line);
    }
  });
  process.stdin.on("end", () => process.exit(0));
} else if (mode === "http") {
  serveHttp();
} else {
  console.error(`Unknown RPI_MCP_FIXTURE_MODE: ${mode}`);
  process.exit(1);
}

function handleMessage(line) {
  const text = line.trim();
  if (!text) return;
  let message;
  try {
    message = JSON.parse(text);
  } catch {
    return;
  }
  logFrame(message);
  const response = respond(message);
  if (response) {
    process.stdout.write(`${JSON.stringify(response)}\n`);
  }
}

function serveHttp() {
  const sseStreams = new Map(); // sessionId -> ServerResponse
  const server = http.createServer((req, res) => {
    const url = new URL(req.url, "http://fixture.invalid");
    const chunks = [];
    req.on("data", (chunk) => chunks.push(chunk));
    req.on("end", () => {
      handleHttp(req, res, url, Buffer.concat(chunks).toString("utf8"));
    });
  });
  server.listen(port, "127.0.0.1", () => {
    process.stdout.write(`${server.address().port}\n`);
  });
  // Keep the process alive for the orchestrator regardless of stdin.
  setInterval(() => {}, 1 << 30);

  function handleHttp(req, res, url, body) {
    if (httpProfile.startsWith("fallback-")) {
      if (req.method === "POST" && url.pathname === "/mcp") {
        const status = Number(httpProfile.slice("fallback-".length));
        res.writeHead(status, { "Content-Type": "application/json" });
        res.end("{}");
        return;
      }
      // Legacy SSE transport: the SDK GETs the ORIGINAL url as the event
      // stream (no separate /sse path); the `endpoint` event then carries
      // the POST message endpoint.
      if (req.method === "GET") {
        const sessionId = "sess-1";
        res.writeHead(200, {
          "Content-Type": "text/event-stream",
          "Cache-Control": "no-cache",
          Connection: "keep-alive",
        });
        res.write(
          `event: endpoint\ndata: ${url.pathname.replace(/\/$/, "")}/message?sessionId=${sessionId}\n\n`,
        );
        sseStreams.set(sessionId, res);
        return;
      }
      if (req.method === "POST" && url.pathname.endsWith("/message")) {
        const sessionId = url.searchParams.get("sessionId") ?? "sess-1";
        let message;
        try {
          message = JSON.parse(body);
        } catch {
          res.writeHead(400);
          res.end();
          return;
        }
        logFrame(message);
        const response = respond(message);
        res.writeHead(202);
        res.end();
        const stream = sseStreams.get(sessionId);
        if (stream && response) {
          stream.write(`data: ${JSON.stringify(response)}\n\n`);
        }
        return;
      }
      res.writeHead(404);
      res.end();
      return;
    }

    if (httpProfile === "auth-401") {
      res.writeHead(401, {
        "WWW-Authenticate":
          'Bearer resource_metadata="http://127.0.0.1:1/.well-known/oauth-protected-resource"',
      });
      res.end();
      return;
    }

    // streamable-json
    if (req.method === "POST" && url.pathname === "/mcp") {
      let message;
      try {
        message = JSON.parse(body);
      } catch {
        res.writeHead(400);
        res.end();
        return;
      }
      logFrame(message);
      const response = respond(message);
      const headers = { "Content-Type": "application/json" };
      if (message.method === "initialize") headers["Mcp-Session-Id"] = "sess-1";
      res.writeHead(200, headers);
      res.end(JSON.stringify(response ?? {}));
      return;
    }
    res.writeHead(405);
    res.end();
  }
}
