# MCP adapter cross-implementation parity report (design §5.2)

Generated: 2026-09-11T06:38:22.423Z (rerun: `node scripts/mcp-parity/run-mcp-parity.mjs`)
Upstream: pi-mcp-adapter @ 10a45367 (server-manager.ts, McpServerManager)
rpi: crates/rpi-ext-mcp-adapter @ bee63d6 (uncommitted working tree)

Normalization: JSON-RPC ids → `$id`; frame transcripts recorded by the shared fixture server; contiguous discovery-request runs order-insensitive; deferred P2 `io.modelcontextprotocol/ui` capability advertisement excluded (rpi-docs 03-extensions-requirements §6, rebase P2 [DEFER]).

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
