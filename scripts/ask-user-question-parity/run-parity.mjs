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
const GROUPS = ["schema", "normalize", "validate", "envelope", "row-intent"];
const UPSTREAM_MODULES = [
	"tool/types.ts",
	"tool/normalize-params.ts",
	"tool/validate-questionnaire.ts",
	"tool/response-envelope.ts",
	"tool/format-answer.ts",
	"state/row-intent.ts",
];

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

function ensureDeps() {
	if (existsSync(TSX)) return;
	console.log(`[parity] installing tsx + typebox into ${DEPS} (one-time)`);
	mkdirSync(DEPS, { recursive: true });
	writeFileSync(
		resolve(DEPS, "package.json"),
		`${JSON.stringify({ name: "rpi-ask-user-question-parity-deps", private: true, type: "module" }, null, 2)}\n`,
	);
	const result = run(
		"npm",
		["install", "--no-save", "tsx@4", `typebox@${typeboxPin()}`],
		{ cwd: DEPS },
	);
	if (result.status !== 0 || !existsSync(TSX)) {
		throw new Error(`npm install failed (need network?):\n${result.stderr}\n${result.stdout}`);
	}
}

function materializeSnapshot() {
	mkdirSync(resolve(SNAPSHOT, "tool"), { recursive: true });
	mkdirSync(resolve(SNAPSHOT, "state"), { recursive: true });
	const hashes = [];
	for (const module of UPSTREAM_MODULES) {
		const from = resolve(UPSTREAM, module);
		const to = resolve(SNAPSHOT, module);
		copyFileSync(from, to);
		hashes.push({ module, sha256: sha256(from) });
	}
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
		"# ask-user-question parity report (TE28 G3/G12)",
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
	ok = compareLocales(report) && ok;
	report.push("", ok ? "## RESULT: MATCH" : "## RESULT: MISMATCH");

	writeFileSync(resolve(GENERATED, "parity-report.md"), `${report.join("\n")}\n`);
	console.log(report.join("\n"));
	process.exit(ok ? 0 : 1);
}

main();
