// Orchestrator of the subagents parity harness (TE04 G3).
//
//   node scripts/subagents-parity/run-parity.mjs
//
// 1. Runs the pinned upstream modules (tsx) on the shared fixtures.
// 2. Runs the Rust parity_runner example on the same fixtures.
// 3. Normalizes both sides (see README.md for the whitelist) and diffs.
// 4. Writes fixtures/generated/subagents-parity/{parity-report.md, parity-*.json}.
//
// Non-zero exit = any case mismatched.
import { spawnSync } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "../..");
const GENERATED = resolve(REPO, "fixtures/generated/subagents-parity");
const TSX = "/tmp/rpi-subagents-parity-deps/node_modules/.bin/tsx";

// Session paths in fixtures.json use the /sess/root placeholder; both legs
// run against the same rewritten copy in a fresh temp dir so buildPiArgs /
// build_rpi_args can create them and the argv values match verbatim.
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";

const SESSION_BASE = mkdtempSync(`${tmpdir()}/rpi-sub-parity-`);

function materialize(mode) {
	const fixtures = JSON.parse(readFileSync(`${HERE}/fixtures.json`, "utf-8"));
	const raw = JSON.stringify(fixtures[mode].cases);
	const rewritten = raw.replaceAll("/sess/root", `${SESSION_BASE}/sess/root`);
	const cases = JSON.parse(rewritten);
	const modeFile = `${HERE}/.cases-${mode}.json`;
	writeFileSync(modeFile, JSON.stringify({ cases }));
	return modeFile;
}

// Both legs run with the ambient parent-session env keys cleared: the
// upstream runner falls back to PI_SUBAGENT_PARENT_SESSION from the shell
// while the rust runner reads RPI_SUBAGENT_PARENT_SESSION (the bridge renames
// PI_SUBAGENT_* → RPI_*), so a value exported in the surrounding shell
// reaches exactly one leg and eight args cases mismatch spuriously.
function cleanSessionEnv(env) {
	const cleaned = { ...env };
	delete cleaned.PI_SUBAGENT_PARENT_SESSION;
	delete cleaned.RPI_SUBAGENT_PARENT_SESSION;
	return cleaned;
}

function runUpstream(mode) {
	const modeFile = materialize(mode);
	const result = spawnSync(
		TSX,
		[`${HERE}/upstream-runner.mjs`, mode, modeFile],
		{
			encoding: "utf-8",
			env: cleanSessionEnv({
				...process.env,
				PI_CODING_AGENT_PACKAGE_ROOT: "/tmp",
			}),
		},
	);
	if (result.status !== 0) {
		throw new Error(`upstream runner (${mode}) failed:\n${result.stderr}\n${result.stdout}`);
	}
	return result.stdout
		.trim()
		.split("\n")
		.filter(Boolean)
		.map((line) => JSON.parse(line));
}

function runRust(mode) {
	const modeFile = materialize(mode);
	const binary = resolve(
		REPO,
		"target/debug/examples/parity_runner",
	);
	const result = spawnSync(binary, [mode, modeFile], {
		encoding: "utf-8",
		env: cleanSessionEnv({ ...process.env }),
	});
	if (result.status !== 0) {
		throw new Error(`rust parity_runner (${mode}) failed:\n${result.stderr}\n${result.stdout}`);
	}
	return result.stdout
		.trim()
		.split("\n")
		.filter(Boolean)
		.map((line) => JSON.parse(line));
}

// Env comparison is key-order-insensitive (upstream JS insertion order vs
// the Rust BTreeMap) and replaces per-run mkdtemp prefixes (rpi-subagent-* /
// pi-subagent-*) with a placeholder — the temp dir names differ by design.
function normalizeOutput(output) {
	const clone = structuredClone(output);
	if (clone?.argv) {
		// Upstream injects its runtime extensions as separate source files
		// (prompt-runtime.ts + fanout-child.ts when authorized); rpi injects a
		// single cdylib filling both slots, so consecutive runtime-extension
		// placeholders collapse (README.md whitelist).
		const argv = [];
		for (let i = 0; i < clone.argv.length; i += 1) {
			const arg = clone.argv[i];
			// "<EXT> --extension <EXT>" runs collapse to one entry (the
			// runtime-extension slots upstream splits across two source files).
			if (
				arg === "--extension"
				&& clone.argv[i + 1] === "<EXT>"
				&& argv[argv.length - 1] === "<EXT>"
			) {
				i += 1;
				continue;
			}
			argv.push(arg);
		}
		clone.argv = argv;
	}
	if (clone?.env) {
		const sorted = {};
		// rpi-only env keys with no upstream counterpart (TE05): the steer
		// inbox and supervisor channel are rpi-native channel mechanisms
		// (upstream rides the prompt-runtime extension + PI_-prefixed vars
		// the fixtures never set), so cleared/absent values are excluded
		// from the diff instead of whitelisted per case.
		const RPI_ONLY_ENV_KEYS = new Set([
			"RPI_SUBAGENT_STEER_INBOX",
			"RPI_SUBAGENT_SUPERVISOR_CHANNEL_DIR",
		]);
		for (const key of Object.keys(clone.env).sort()) {
			if (RPI_ONLY_ENV_KEYS.has(key)) continue;
			sorted[key] = String(clone.env[key]).replace(
				/\/tmp\/(pi|rpi)-subagent-[A-Za-z0-9_-]+/g,
				"<TMPDIR>",
			);
		}
		clone.env = sorted;
	}
	return clone;
}

function deepEqual(a, b) {
	return JSON.stringify(normalizeOutput(a)) === JSON.stringify(normalizeOutput(b));
}

function compareMode(mode, report) {
	const upstream = runUpstream(mode);
	const rust = runRust(mode);
	if (upstream.length !== rust.length) {
		report.push(`## ${mode}: CASE COUNT MISMATCH (upstream ${upstream.length}, rust ${rust.length})`);
		return false;
	}
	let allMatch = true;
	const lines = [];
	for (let i = 0; i < upstream.length; i += 1) {
		const up = upstream[i];
		const rs = rust[i];
		if (up.name !== rs.name) {
			lines.push(`- ${up.name}: NAME MISMATCH vs ${rs.name}`);
			allMatch = false;
			continue;
		}
		if (!deepEqual(up.output, rs.output)) {
			lines.push(`- ${up.name}: MISMATCH\n  upstream: ${JSON.stringify(normalizeOutput(up.output))}\n  rust:     ${JSON.stringify(normalizeOutput(rs.output))}`);
			allMatch = false;
		} else {
			lines.push(`- ${up.name}: MATCH`);
		}
	}
	report.push(`## ${mode}\n\n${lines.join("\n")}\n`);
	return allMatch;
}

mkdirSync(GENERATED, { recursive: true });
const report = ["# subagents parity report (TE04 G3)", "", `generated: ${new Date().toISOString()}`, ""];
let ok = true;
for (const mode of ["args", "frontmatter", "final-output"]) {
	ok = compareMode(mode, report) && ok;
}
report.push("", ok ? "## RESULT: MATCH" : "## RESULT: MISMATCH");
const reportPath = `${GENERATED}/parity-report.md`;
writeFileSync(reportPath, report.join("\n") + "\n");
console.log(report.join("\n"));
process.exit(ok ? 0 : 1);
