# End-to-end parity report (design §5.3)

Generated: 2026-08-14T09:55:46.599Z (rerun: `node scripts/mcp-parity/run-e2e-parity.mjs`)
Upstream: `pi -p` 0.84.x + npm-hosted pi-mcp-adapter
rpi: `target/debug/rpi -p` + native rpi-ext-mcp-adapter
Normalization: fenced verbatim block of the model reply (models/prose differ).

| Mode | Verdict | Detail |
| --- | --- | --- |
| list | MATCH | 9 lines |
| search | MATCH | 7 lines |
| describe | MATCH | 7 lines |
| call | MATCH | 1 lines |
| status | DIFF | diff below |

## status

### upstream

```
MCP: 1/1 servers, 3 tools

✓ fixture (3 tools)

mcp({ server: "name" }) to list tools, mcp({ search: "..." }) to search
```

### rpi

```
{"server":"MCP","instructions":"MCP gateway — server status, tool search/describe, auth, and single MCP tool calls. When one request needs several MCP calls with logic between them, use mcpScript. Non-MCP Pi tools should be called directly, not through mcp.\n\nUsage:\n  mcp({ })                              → Show server status\n  mcp({ server: \"name\" })               → List tools from server\n  mcp({ search: \"...\" })                → Search MCP tools by name/description\n  mcp({ describe: \"tool_name\" })        → Show tool details and parameters\n  mcp({ instructions: \"name\" })         → Show full server usage instructions\n  mcp({ connect: \"server-name\" })       → Connect to a server and refresh metadata\n  mcp({ tool: \"name\", args: { key: \"value\" } })         → Call a tool (object args; JSON string also accepted)\n  mcp({ action: \"ui-messages\" })        → Retrieve accumulated messages from completed UI sessions\n  mcp({ action: \"auth-start\", server: \"name\" })      → Start manual OAuth and get a browser URL\n  mcp({ action: \"auth-complete\", server: \"name\", args: { redirectUrl: \"...\" } }) → Complete manual OAuth\n\nMode: action > tool (call) > connect > describe > instructions > search > server (list) > nothing (status)","status":"ok","servers":1,"tools":3,"toolCountByServer":{"fixture":3}}
```

1 mode(s) differ.
