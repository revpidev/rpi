# built-in MCP cross-implementation parity report (V16-08 §6 / G3)

Generated: 2026-10-03T03:03:02.732Z (rerun: `node scripts/mcp-parity/run-builtin-mcp-parity.mjs`)
Upstream: external/pi/packages/mcp @ a13d35a74 (McpClient + StdioTransport, tsx)
rpi: crates/rpi-mcp @ 7a62297 (uncommitted working tree)

Normalization: JSON-RPC ids → `$id`; `clientInfo.name` → `parity-client` (O1 brand exemption); frame transcripts recorded server-side by the shared fixture so a diff isolates the client.

| Scenario | Verdict | Detail |
| --- | --- | --- |
| stdio (initialize/tools list/tools call/error/resource read) | MATCH | 6 frames, 4 results, status=connected |

Frames: upstream 6, rpi 6.

Normalized documents MATCH.
