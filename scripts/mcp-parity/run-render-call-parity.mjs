// Orchestrator of the renderCall parity leg (TE09 FR-E).
//
//   node scripts/mcp-parity/run-render-call-parity.mjs
//
// 1. Runs the pinned upstream exported functions (tsx) on the shared
//    fixtures (render-call-upstream.mjs).
// 2. Runs the Rust render_call_parity example on the same fixtures.
// 3. Diffs the per-case outputs byte-for-byte.
// 4. Writes fixtures/generated/mcp-parity/render-call-parity-{upstream,rpi}.json
//    and render-call-parity.md (git evidence).
//
// Non-zero exit = any case mismatched.
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "../..");
const OUT_DIR = resolve(REPO, "fixtures/generated/mcp-parity");
const FIXTURES = resolve(HERE, "render-call-fixtures.json");
const DEPS = "/tmp/rpi-mcp-parity-deps";
const tsxLoader = join(DEPS, "node_modules", "tsx", "dist", "loader.mjs");
if (!existsSync(tsxLoader)) {
	console.error(`tsx not installed under ${DEPS}; run scripts/mcp-parity/setup-deps.sh first`);
	process.exit(2);
}

function runUpstream() {
	const result = spawnSync(
		process.execPath,
		["--import", tsxLoader, resolve(HERE, "render-call-upstream.mjs"), FIXTURES],
		{
			encoding: "utf-8",
			env: { ...process.env, RPI_MCP_PARITY_DEPS: DEPS },
		},
	);
	if (result.status !== 0) {
		throw new Error(`upstream leg failed:\n${result.stderr}\n${result.stdout}`);
	}
	return result.stdout.trim().split("\n").filter(Boolean).map((line) => JSON.parse(line));
}

function runRust() {
	const binary = resolve(REPO, "target/debug/examples/render_call_parity");
	const result = spawnSync(binary, [FIXTURES], { encoding: "utf-8" });
	if (result.status !== 0) {
		throw new Error(`rust leg failed (build first: cargo build --example render_call_parity):\n${result.stderr}\n${result.stdout}`);
	}
	return result.stdout.trim().split("\n").filter(Boolean).map((line) => JSON.parse(line));
}

const upstream = runUpstream();
const rpi = runRust();

const byName = (rows) => new Map(rows.map((row) => [row.name, row]));
const upMap = byName(upstream);
const rpiMap = byName(rpi);
const names = [...new Set([...upMap.keys(), ...rpiMap.keys()])];

const mismatches = [];
for (const name of names) {
	if (!upMap.has(name) || !rpiMap.has(name)) {
		mismatches.push({ name, reason: "missing on one side" });
		continue;
	}
	const left = JSON.stringify(upMap.get(name));
	const right = JSON.stringify(rpiMap.get(name));
	if (left !== right) {
		mismatches.push({ name, upstream: upMap.get(name), rpi: rpiMap.get(name) });
	}
}

mkdirSync(OUT_DIR, { recursive: true });
writeFileSync(
	resolve(OUT_DIR, "render-call-parity-upstream.json"),
	JSON.stringify(upstream, null, 2) + "\n",
);
writeFileSync(
	resolve(OUT_DIR, "render-call-parity-rpi.json"),
	JSON.stringify(rpi, null, 2) + "\n",
);

const report = [
	"# renderCall parity (TE09 FR-E)",
	"",
	`Upstream: pi-mcp-adapter tool-result-renderer.ts @ 3d953f90 (exported pure functions, tsx leg)`,
	`rpi: crates/rpi-ext-mcp-adapter/src/render.rs (render_call_parity example leg)`,
	`Fixtures: scripts/mcp-parity/render-call-fixtures.json (${names.length} cases)`,
	"",
	mismatches.length === 0
		? "All cases byte-identical."
		: `${mismatches.length} mismatched cases:\n\n\`\`\`json\n${JSON.stringify(mismatches, null, 2)}\n\`\`\``,
	"",
].join("\n");
writeFileSync(resolve(OUT_DIR, "render-call-parity.md"), report);

console.log(report);
process.exit(mismatches.length === 0 ? 0 : 1);
