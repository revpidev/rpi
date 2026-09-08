// Upstream leg of the renderCall parity (TE09 FR-E): runs the pinned
// pi-mcp-adapter exported pure functions (tool-result-renderer.ts @
// 3d953f90) over the shared fixtures and prints one JSON document per line
// (case name → { lines } for the format functions, { rendered } for the
// render wrappers with the plain-theme Text output).
//
//   tsx scripts/mcp-parity/render-call-upstream.mjs <fixtures.json>
//
// Never writes into rpi/external/. The tsx + dependency closure lives in
// /tmp/rpi-mcp-parity-deps (see setup-deps.sh). The render-call hooks map
// pi-tui to a minimal value stub (the render functions construct Text).
import { readFileSync } from "node:fs";
import { register } from "node:module";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

register(new URL("./render-call-hooks.mjs", import.meta.url));

// Default = regression track (old pin); the target track points
// RPI_MCP_PARITY_UPSTREAM at the v2.32.1 snapshot (ADR-0025 §9).
const UPSTREAM_ROOT =
	process.env.RPI_MCP_PARITY_UPSTREAM ??
	new URL("../../external/pi-mcp-adapter/", import.meta.url).pathname;

// Dynamic import: the hooks must be registered before the pinned upstream
// module resolves its bare imports (same ordering as upstream-runner.mjs).
const {
	formatMcpDirectToolCallLines,
	formatMcpProxyToolCallLines,
	renderMcpProxyToolCall,
	createMcpDirectToolCallRenderer,
} = await import(pathToFileURL(resolve(UPSTREAM_ROOT, "tool-result-renderer.ts")).href);

const fixturesPath = process.argv[2] ?? new URL("./render-call-fixtures.json", import.meta.url).pathname;
const cases = JSON.parse(readFileSync(fixturesPath, "utf-8")).cases;

for (const item of cases) {
	if (item.kind === "proxy") {
		const lines = formatMcpProxyToolCallLines(item.args);
		console.log(JSON.stringify({ name: item.name, lines }));
	} else if (item.kind === "direct") {
		const lines = formatMcpDirectToolCallLines(item.displayName, item.args);
		console.log(JSON.stringify({ name: item.name, lines }));
	} else if (item.kind === "render-proxy") {
		// Plain theme (none passed): Text.render(80) is the ANSI-free line
		// output the rpi ComponentTree extraction mirrors.
		const rendered = renderMcpProxyToolCall(item.args).render(80).join("\n");
		console.log(JSON.stringify({ name: item.name, rendered }));
	} else if (item.kind === "render-direct") {
		const renderer = createMcpDirectToolCallRenderer(item.displayName);
		const rendered = renderer(item.args).render(80).join("\n");
		console.log(JSON.stringify({ name: item.name, rendered }));
	}
}
