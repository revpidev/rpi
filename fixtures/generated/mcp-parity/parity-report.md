# MCP adapter cross-implementation parity report (design §5.2)

Generated: 2026-08-14T09:54:34.285Z (rerun: `node scripts/mcp-parity/run-mcp-parity.mjs`)
Upstream: pi-mcp-adapter @ 3d953f90 (server-manager.ts, McpServerManager)
rpi: crates/rpi-ext-mcp-adapter @ 2d79c0e (uncommitted working tree)

Normalization: JSON-RPC ids → `$id`; frame transcripts recorded by the shared fixture server.

| Scenario | Verdict | Detail |
| --- | --- | --- |
| stdio | MATCH | 8 frames, status=connected |
| http-streamable | MATCH | 8 frames, status=connected |
| http-fallback-404 | MATCH | 8 frames, status=connected |
| http-fallback-405 | MATCH | 8 frames, status=connected |
| http-fallback-406 | MATCH | 8 frames, status=connected |
| http-fallback-415 | MATCH | 8 frames, status=connected |
| http-auth-401 | MATCH | 0 frames, status=error |

All scenarios MATCH.
