// Orchestrator of the ask-user-question parity harness (TE28 G3/G12).
//
//   node scripts/ask-user-question-parity/run-parity.mjs
//
// 1. Verifies the pinned submodule HEAD (external/rpiv-mono @ 338b264c) and
//    materializes a read-only snapshot of the six upstream pure-function
//    modules into the deps dir (external/ is never written).
// 2. Builds this crate's `parity_runner` example. Three workspace crates
//    (ask-user-question / mcp-adapter / subagents) ship an example with that
//    name, so cargo's shared `target/debug/examples/parity_runner` is
//    whichever built last — building it here makes the harness independent of
//    build order (mcp-parity precedent; cargo emits an output-filename
//    collision warning for the shared path).
// 3. Runs the upstream modules (tsx) on the shared fixtures.
// 4. Runs the Rust parity_runner example on the same fixtures.
// 5. Normalizes both sides (key-order-insensitive deep compare) and diffs.
// 6. Checks the vendored locale tables byte-for-byte against the submodule.
// 7. Writes fixtures/generated/ask-user-question-parity/{parity-report.md,
//    upstream-*.jsonl, rust-*.jsonl}.
//
// Non-zero exit = any case mismatched. First run installs tsx + typebox into
// /tmp/rpi-ask-user-question-parity-deps (override with RPI_ASKQ_PARITY_DEPS).
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
	copyFileSync,
	existsSync,
	mkdirSync,
	readFileSync,
	readdirSync,
	writeFileSync,
} from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "../..");
const UPSTREAM = resolve(REPO, "external/rpiv-mono/packages/rpiv-ask-user-question");
const DEPS = process.env.RPI_ASKQ_PARITY_DEPS ?? "/tmp/rpi-ask-user-question-parity-deps";
const SNAPSHOT = resolve(DEPS, "snapshot");
const TSX = resolve(DEPS, "node_modules/.bin/tsx");
const RUST_RUNNER = resolve(REPO, "target/debug/examples/parity_runner");
const VENDORED_LOCALES = resolve(REPO, "crates/rpi-ext-ask-user-question/locales");
const GENERATED = resolve(REPO, "fixtures/generated/ask-user-question-parity");
const PINNED_COMMIT = "338b264c1ca4fd8828cc849b632f4f7ad88d2e78";
const GROUPS = ["schema", "normalize", "validate", "envelope", "row-intent", "rpc", "state", "keys", "preview"];
const UPSTREAM_MODULES = [
	"tool/types.ts",
	"tool/normalize-params.ts",
	"tool/validate-questionnaire.ts",
	"tool/response-envelope.ts",
	"tool/format-answer.ts",
	"state/row-intent.ts",
	"state/i18n-bridge.ts",
	"rpc-fallback.ts",
	// TE30: dialog state machine + key router.
	"state/state-reducer.ts",
	"state/key-router.ts",
	// TE31: preview layout math + bordered-box renderer (pure; the markdown
	// body cache / block renderer need the pi-tui Markdown component and stay
	// Rust-side golden-tested instead).
	"view/components/preview/preview-layout-decider.ts",
	"view/components/preview/preview-box-renderer.ts",
];
// The `@earendil-works/pi-tui` import in key-router.ts is stubbed with the
// pinned upstream keys module (self-contained; no other tui sources needed).
// TE31: the preview modules additionally need `visibleWidth`/
// `truncateToWidth` from the pinned upstream utils module (plus its
// `get-east-asian-width` dependency, installed at the pi-tui pin).
const PI_TUI_KEYS = resolve(REPO, "external/pi/packages/tui/src/keys.ts");
const PI_TUI_UTILS = resolve(REPO, "external/pi/packages/tui/src/utils.ts");
const GOLDEN_DIR = resolve(GENERATED, "golden-frames");

function sha256(path) {
	return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function run(command, args, options = {}) {
	return spawnSync(command, args, { encoding: "utf-8", ...options });
}

function submoduleHead() {
	const result = run("git", ["-C", resolve(REPO, "external/rpiv-mono"), "rev-parse", "HEAD"]);
	return result.status === 0 ? result.stdout.trim() : `unknown (${result.stderr.trim()})`;
}

function typeboxPin() {
	try {
		const lock = JSON.parse(readFileSync(resolve(REPO, "external/rpiv-mono/package-lock.json"), "utf-8"));
		return lock.packages?.["node_modules/typebox"]?.version ?? "1.3.6";
	} catch {
		return "1.3.6";
	}
}

/** pi-tui's `get-east-asian-width` pin (utils.ts dependency, TE31). */
function eastAsianWidthPin() {
	try {
		const pkg = JSON.parse(readFileSync(resolve(REPO, "external/pi/packages/tui/package.json"), "utf-8"));
		return pkg.dependencies?.["get-east-asian-width"] ?? "1.6.0";
	} catch {
		return "1.6.0";
	}
}

function ensureDeps() {
	if (existsSync(TSX) && existsSync(resolve(DEPS, "node_modules/get-east-asian-width"))) return;
	console.log(`[parity] installing tsx + typebox into ${DEPS} (one-time)`);
	mkdirSync(DEPS, { recursive: true });
	writeFileSync(
		resolve(DEPS, "package.json"),
		`${JSON.stringify({ name: "rpi-ask-user-question-parity-deps", private: true, type: "module" }, null, 2)}\n`,
	);
	const result = run(
		"npm",
		["install", "--no-save", "tsx@4", `typebox@${typeboxPin()}`, `get-east-asian-width@${eastAsianWidthPin()}`],
		{ cwd: DEPS },
	);
	if (result.status !== 0 || !existsSync(TSX)) {
		throw new Error(`npm install failed (need network?):\n${result.stderr}\n${result.stdout}`);
	}
}

function materializeSnapshot() {
	mkdirSync(resolve(SNAPSHOT, "tool"), { recursive: true });
	mkdirSync(resolve(SNAPSHOT, "state"), { recursive: true });
	mkdirSync(resolve(SNAPSHOT, "view/components/preview"), { recursive: true });
	const hashes = [];
	for (const module of UPSTREAM_MODULES) {
		const from = resolve(UPSTREAM, module);
		const to = resolve(SNAPSHOT, module);
		mkdirSync(dirname(to), { recursive: true });
		copyFileSync(from, to);
		hashes.push({ module, sha256: sha256(from) });
	}
	// Stub `@earendil-works/pi-tui` for the snapshot imports: verbatim copies
	// of the pinned upstream keys + utils modules, resolved by Node from the
	// deps node_modules (external/ stays read-only; utils.ts's
	// `get-east-asian-width` import resolves from the deps install).
	const stubDir = resolve(DEPS, "node_modules/@earendil-works/pi-tui");
	mkdirSync(stubDir, { recursive: true });
	writeFileSync(
		resolve(stubDir, "package.json"),
		`${JSON.stringify({ name: "@earendil-works/pi-tui", version: "0.0.0-harness", type: "module", main: "index.ts" }, null, 2)}\n`,
	);
	copyFileSync(PI_TUI_KEYS, resolve(stubDir, "keys.ts"));
	copyFileSync(PI_TUI_UTILS, resolve(stubDir, "utils.ts"));
	writeFileSync(
		resolve(stubDir, "index.ts"),
		'export * from "./keys.ts";\nexport * from "./utils.ts";\n',
	);
	hashes.push({
		module: "external/pi/packages/tui/src/keys.ts (pi-tui stub)",
		sha256: sha256(PI_TUI_KEYS),
	});
	hashes.push({
		module: "external/pi/packages/tui/src/utils.ts (pi-tui stub, TE31)",
		sha256: sha256(PI_TUI_UTILS),
	});
	return hashes;
}

function runUpstream(group) {
	const result = run(
		TSX,
		[resolve(HERE, "upstream-runner.mjs"), group, resolve(HERE, "fixtures.json")],
		{ env: { ...process.env, PARITY_SNAPSHOT: SNAPSHOT } },
	);
	if (result.status !== 0) {
		throw new Error(`upstream runner (${group}) failed:\n${result.stderr}\n${result.stdout}`);
	}
	return result.stdout
		.trim()
		.split("\n")
		.filter(Boolean)
		.map((line) => JSON.parse(line));
}

function buildRustRunner() {
	const result = run(
		"cargo",
		["build", "-p", "rpi-ext-ask-user-question", "--example", "parity_runner"],
		{ cwd: REPO },
	);
	if (result.status !== 0) {
		throw new Error(`cargo build parity_runner failed:\n${result.stderr}\n${result.stdout}`);
	}
}

function runRust(group) {
	if (!existsSync(RUST_RUNNER)) {
		throw new Error(
			`missing ${RUST_RUNNER}; build it first: cargo build -p rpi-ext-ask-user-question --example parity_runner`,
		);
	}
	const result = run(RUST_RUNNER, [group, resolve(HERE, "fixtures.json")]);
	if (result.status !== 0) {
		throw new Error(`rust parity_runner (${group}) failed:\n${result.stderr}\n${result.stdout}`);
	}
	return result.stdout
		.trim()
		.split("\n")
		.filter(Boolean)
		.map((line) => JSON.parse(line));
}

/** Deep key-order normalization (JS insertion order vs serde_json order). */
function normalize(value) {
	if (Array.isArray(value)) return value.map(normalize);
	if (value && typeof value === "object") {
		const out = {};
		for (const key of Object.keys(value).sort()) out[key] = normalize(value[key]);
		return out;
	}
	return value;
}

function canonical(value) {
	return JSON.stringify(normalize(value));
}

function compareGroup(group, report) {
	const upstream = runUpstream(group);
	const rust = runRust(group);
	writeFileSync(
		resolve(GENERATED, `upstream-${group}.jsonl`),
		`${upstream.map((entry) => JSON.stringify(entry)).join("\n")}\n`,
	);
	writeFileSync(
		resolve(GENERATED, `rust-${group}.jsonl`),
		`${rust.map((entry) => JSON.stringify(entry)).join("\n")}\n`,
	);
	if (upstream.length !== rust.length) {
		report.push(`## ${group}: CASE COUNT MISMATCH (upstream ${upstream.length}, rust ${rust.length})`);
		return false;
	}
	let allMatch = true;
	const lines = [];
	for (let index = 0; index < upstream.length; index += 1) {
		const up = upstream[index];
		const rs = rust[index];
		if (up.name !== rs.name) {
			lines.push(`- ${up.name}: NAME MISMATCH vs ${rs.name}`);
			allMatch = false;
			continue;
		}
		if (canonical(up.output) !== canonical(rs.output)) {
			lines.push(
				`- ${up.name}: MISMATCH\n  upstream: ${canonical(up.output)}\n  rust:     ${canonical(rs.output)}`,
			);
			allMatch = false;
		} else {
			lines.push(`- ${up.name}: MATCH`);
		}
	}
	report.push(`## ${group}\n\n${lines.join("\n")}\n`);
	return allMatch;
}

/** TE30 golden frames: render fresh frames and compare byte-for-byte against
 * the committed baseline (`gen-golden-frames.mjs` re-records). */
function compareGoldenFrames(report) {
	const result = run(RUST_RUNNER, ["golden"]);
	if (result.status !== 0) {
		report.push(`## golden-frames: RUN FAILED\n\n${result.stderr}\n`);
		return false;
	}
	const byFile = new Map();
	for (const line of result.stdout.trim().split("\n").filter(Boolean)) {
		const entry = JSON.parse(line);
		if (!byFile.has(entry.file)) byFile.set(entry.file, []);
		byFile.get(entry.file).push(JSON.stringify(entry.frame));
	}
	if (!existsSync(GOLDEN_DIR)) {
		report.push(
			`## golden-frames\n\nMISSING ${GOLDEN_DIR.replace(`${REPO}/`, "")} — run: node scripts/ask-user-question-parity/gen-golden-frames.mjs\n`,
		);
		return false;
	}
	const committed = readdirSync(GOLDEN_DIR).filter((file) => file.endsWith(".jsonl")).sort();
	const lines = [
		"- baseline: self-baseline (fresh Rust frames vs the committed `golden-frames/*.jsonl`; no upstream renderer exists)",
	];
	let allMatch = true;
	for (const file of committed) {
		const expected = readFileSync(resolve(GOLDEN_DIR, file), "utf-8").trimEnd();
		const actual = (byFile.get(file) ?? []).join("\n");
		if (expected !== actual) {
			lines.push(`- ${file}: MISMATCH (re-record with gen-golden-frames.mjs if intended)`);
			allMatch = false;
		} else {
			lines.push(`- ${file}: MATCH (${expected.split("\n").length} frames)`);
		}
	}
	for (const file of byFile.keys()) {
		if (!committed.includes(file)) {
			lines.push(`- ${file}: EXTRA rendered file not in the golden baseline`);
			allMatch = false;
		}
	}
	report.push(`## golden-frames\n\n${lines.join("\n")}\n`);
	return allMatch;
}

function compareLocales(report) {
	const upstreamLocales = resolve(UPSTREAM, "locales");
	const files = readdirSync(upstreamLocales).filter((file) => file.endsWith(".json"));
	files.sort();
	const lines = [];
	let allMatch = true;
	for (const file of files) {
		const upstreamFile = resolve(upstreamLocales, file);
		const vendoredFile = resolve(VENDORED_LOCALES, file);
		if (!existsSync(vendoredFile)) {
			lines.push(`- ${file}: MISSING in crates/rpi-ext-ask-user-question/locales/`);
			allMatch = false;
			continue;
		}
		const upstreamHash = sha256(upstreamFile);
		const vendoredHash = sha256(vendoredFile);
		if (upstreamHash !== vendoredHash) {
			lines.push(`- ${file}: MISMATCH (upstream ${upstreamHash}, vendored ${vendoredHash})`);
			allMatch = false;
		} else {
			lines.push(`- ${file}: MATCH (${upstreamHash.slice(0, 12)})`);
		}
	}
	const extra = readdirSync(VENDORED_LOCALES).filter(
		(file) => file.endsWith(".json") && !files.includes(file),
	);
	for (const file of extra) {
		lines.push(`- ${file}: EXTRA vendored locale not in upstream`);
		allMatch = false;
	}
	report.push(`## locales\n\n${lines.join("\n")}\n`);
	return allMatch;
}

function main() {
	ensureDeps();
	buildRustRunner();
	const head = submoduleHead();
	const snapshotHashes = materializeSnapshot();
	mkdirSync(GENERATED, { recursive: true });

	const report = [
		"# ask-user-question parity report (TE28/TE29/TE30/TE31 G3/G12 — preview group added by TE31)",
		"",
		`generated: ${new Date().toISOString()}`,
		`upstream submodule: ${UPSTREAM.replace(`${REPO}/`, "")}`,
		`submodule HEAD: ${head} (pinned ${PINNED_COMMIT})`,
		`typebox: ${typeboxPin()}`,
		"",
		"## snapshot (sha256 of the driven upstream modules)",
		"",
		...snapshotHashes.map((entry) => `- ${entry.module}: ${entry.sha256}`),
		"",
	];

	let ok = head === PINNED_COMMIT;
	if (!ok) {
		report.push(
			`## PIN MISMATCH\n\nsubmodule HEAD ${head} != pinned ${PINNED_COMMIT}\n`,
		);
	}
	for (const group of GROUPS) {
		ok = compareGroup(group, report) && ok;
	}
	ok = compareGoldenFrames(report) && ok;
	ok = compareLocales(report) && ok;
	report.push("", ok ? "## RESULT: MATCH" : "## RESULT: MISMATCH");

	writeFileSync(resolve(GENERATED, "parity-report.md"), `${report.join("\n")}\n`);
	console.log(report.join("\n"));
	process.exit(ok ? 0 : 1);
}

main();
