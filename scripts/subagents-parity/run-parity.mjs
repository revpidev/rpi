// Orchestrator of the subagents parity harness (TE04 G3; dual-track TE13).
//
//   node scripts/subagents-parity/run-parity.mjs [--track=regression|target]
//   node scripts/subagents-parity/run-parity.mjs --record-args-golden
//
// The Rust leg is built by this script and copied to a private path before
// execution: both plugin crates ship an example named `parity_runner`, and
// the unsuffixed target/debug/examples/parity_runner belongs to whichever
// crate built last (the mcp harness would shadow it otherwise).
//
// Track `regression` (default; byte-compatible with the pre-TE13 harness):
//   1. Runs the pinned upstream v0.48 modules (tsx) on the shared fixtures.
//   2. Runs the Rust parity_runner example on the same fixtures.
//   3. Normalizes both sides and diffs; writes
//      fixtures/generated/subagents-parity/parity-report.md.
//   Non-zero exit = any case mismatched.
//
// Track `target` (pi-subagents v0.66.0, ADR-0025):
//   1. argv/env: Rust vs the frozen v0.48 golden ([RPI-OWN], ADR-0025 §4;
//      upstream deleted `pi-args.ts` in v0.65).
//   2. frontmatter/final-output/fallback: Rust vs the v0.66 snapshot modules
//      extracted by setup-target-source.sh.
//   3. Diffs are attributed through expected-target-diffs.json
//      (`upstream-semantics` vs `rpi-deviation`, each with R + owner task);
//      writes fixtures/generated/subagents-parity-v066/parity-report.md.
//   Non-zero exit = any UNATTRIBUTED diff.
import { spawnSync } from "node:child_process";
import { copyFileSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "../..");
const TSX = "/tmp/rpi-subagents-parity-deps/node_modules/.bin/tsx";
const GOLDEN_PATH = `${HERE}/args-golden-v048.json`;
const TARGET_MANIFEST = `${HERE}/expected-target-diffs.json`;

const TRACK_FLAG = process.argv.find((arg) => arg.startsWith("--track="));
const TRACK = TRACK_FLAG ? TRACK_FLAG.slice("--track=".length) : "regression";
if (!["regression", "target"].includes(TRACK)) {
	console.error(`unknown track: ${TRACK}`);
	process.exit(2);
}
const RECORD_ARGS_GOLDEN = process.argv.includes("--record-args-golden");

const GENERATED = resolve(
	REPO,
	TRACK === "target"
		? "fixtures/generated/subagents-parity-v066"
		: "fixtures/generated/subagents-parity",
);
const MODES =
	TRACK === "target"
		? ["args", "frontmatter", "final-output", "fallback"]
		: ["args", "frontmatter", "final-output"];

// Session paths in fixtures.json use the /sess/root placeholder; both legs
// run against the same rewritten copy in a fresh temp dir so buildPiArgs /
// build_rpi_args can create them and the argv values match verbatim.
// The materialized copies live in an out-of-repo temp dir (their session
// paths are run-volatile; they are inputs, not evidence).
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";

const SESSION_BASE = mkdtempSync(`${tmpdir()}/rpi-sub-parity-`);
const CASES_DIR = mkdtempSync(`${tmpdir()}/rpi-sub-parity-cases-`);
// Materialized fixtures rewrite `/sess/root` to `${SESSION_BASE}/sess/root`;
// normalize that whole prefix first so it collapses to one placeholder.
const SESSION_PREFIX = `${SESSION_BASE}/sess/root`;
const placeholderizePaths = (value) =>
	String(value)
		.replaceAll(SESSION_PREFIX, "<SESSION_BASE>")
		.replaceAll(SESSION_BASE, "<SESSION_BASE>")
		.replaceAll("/sess/root", "<SESSION_BASE>");

function loadCases(mode) {
	const base = JSON.parse(readFileSync(`${HERE}/fixtures.json`, "utf-8"));
	let cases = base[mode]?.cases ?? [];
	if (TRACK === "target") {
		const extra = JSON.parse(readFileSync(`${HERE}/fixtures-target.json`, "utf-8"));
		cases = cases.concat(extra[mode]?.cases ?? []);
	}
	return cases;
}

function materialize(mode) {
	const raw = JSON.stringify(loadCases(mode));
	const rewritten = raw.replaceAll("/sess/root", `${SESSION_BASE}/sess/root`);
	const cases = JSON.parse(rewritten);
	const modeFile = `${CASES_DIR}/cases-${TRACK}-${mode}.json`;
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

function runUpstream(mode, modeFile) {
	const result = spawnSync(TSX, [`${HERE}/upstream-runner.mjs`, mode, modeFile], {
		encoding: "utf-8",
		env: cleanSessionEnv({
			...process.env,
			PI_CODING_AGENT_PACKAGE_ROOT: "/tmp",
			RPI_SUBAGENTS_PARITY_TRACK: TRACK,
		}),
	});
	if (result.status !== 0) {
		throw new Error(`upstream runner (${mode}) failed:\n${result.stderr}\n${result.stdout}`);
	}
	return parseLines(result.stdout, `upstream ${mode}`);
}

function runRust(mode, modeFile) {
	const binary = ensureRustRunner();
	const result = spawnSync(binary, [mode, modeFile], {
		encoding: "utf-8",
		env: cleanSessionEnv({ ...process.env }),
	});
	if (result.status !== 0) {
		throw new Error(`rust parity_runner (${mode}) failed:\n${result.stderr}\n${result.stdout}`);
	}
	return parseLines(result.stdout, `rust ${mode}`);
}

let rustRunnerPath;
// Both rpi-ext-subagents and rpi-ext-mcp-adapter ship an example named
// `parity_runner`; cargo writes the unsuffixed `target/debug/examples/
// parity_runner` for whichever crate built last, so the mcp harness can
// shadow this one. Build ours and copy it to a private path in the same
// step (the copy is what we execute), keeping the documented example name.
function ensureRustRunner() {
	if (rustRunnerPath) return rustRunnerPath;
	const build = spawnSync(
		"cargo",
		["build", "-p", "rpi-ext-subagents", "--example", "parity_runner"],
		{ cwd: REPO, encoding: "utf-8" },
	);
	if (build.status !== 0) {
		throw new Error(`cargo build of the parity_runner example failed:\n${build.stderr}`);
	}
	rustRunnerPath = `${CASES_DIR}/subagents-parity_runner`;
	copyFileSync(resolve(REPO, "target/debug/examples/parity_runner"), rustRunnerPath);
	return rustRunnerPath;
}

function parseLines(stdout, label) {
	return stdout
		.trim()
		.split("\n")
		.filter(Boolean)
		.map((line) => {
			try {
				return JSON.parse(line);
			} catch (error) {
				throw new Error(`${label}: non-JSON output line: ${line}\n${error}`);
			}
		});
}

// Env comparison is key-order-insensitive (upstream JS insertion order vs
// the Rust BTreeMap) and replaces per-run mkdtemp prefixes (rpi-subagent-* /
// pi-subagent-*) with a placeholder — the temp dir names differ by design.
// Session bases are placeholderized too so the frozen v0.48 golden (recorded
// in another run) stays comparable.
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
			sorted[key] = placeholderizePaths(clone.env[key]).replace(
				/\/tmp\/(pi|rpi)-subagent-[A-Za-z0-9_-]+/g,
				"<TMPDIR>",
			);
		}
		clone.env = sorted;
	}
	if (clone?.argv) {
		clone.argv = clone.argv.map(placeholderizePaths);
	}
	return clone;
}

// Field-level diff: `null` on the Rust side means "function not implemented
// yet" (R7.1.2.2/.3 at M0), which is a diff just like a value mismatch.
function diffOutput(upstream, rust) {
	const a = normalizeOutput(upstream);
	const b = normalizeOutput(rust);
	if (JSON.stringify(a) === JSON.stringify(b)) return null;
	const fields = [];
	if (
		a && b && typeof a === "object" && typeof b === "object"
		&& !Array.isArray(a) && !Array.isArray(b)
	) {
		for (const key of new Set([...Object.keys(a), ...Object.keys(b)])) {
			if (JSON.stringify(a[key]) !== JSON.stringify(b[key])) fields.push(key);
		}
	}
	return { fields, upstream: a, rust: b };
}

function loadManifest() {
	const raw = JSON.parse(readFileSync(TARGET_MANIFEST, "utf-8"));
	const byKey = new Map();
	for (const entry of raw.cases ?? []) {
		byKey.set(`${entry.mode}/${entry.case}`, entry);
	}
	return byKey;
}

const manifest = TRACK === "target" ? loadManifest() : new Map();
const attribution = { "upstream-semantics": [], "rpi-deviation": [] };
const unattributed = [];
let ok = true;

function compareMode(mode, report) {
	const modeFile = materialize(mode);
	const upstream = runUpstream(mode, modeFile);
	const rust = runRust(mode, modeFile);
	if (upstream.length !== rust.length) {
		report.push(
			`## ${mode}: CASE COUNT MISMATCH (upstream ${upstream.length}, rust ${rust.length})`,
		);
		ok = false;
		return;
	}
	const lines = [];
	for (let i = 0; i < upstream.length; i += 1) {
		const up = upstream[i];
		const rs = rust[i];
		if (up.name !== rs.name) {
			lines.push(`- ${up.name}: NAME MISMATCH vs ${rs.name}`);
			ok = false;
			continue;
		}
		const diff = diffOutput(up.output, rs.output);
		if (!diff) {
			lines.push(`- ${up.name}: MATCH`);
			continue;
		}
		const entry = manifest.get(`${mode}/${up.name}`);
		if (TRACK === "target" && entry) {
			const fieldNote = diff.fields.length > 0 ? ` fields: ${diff.fields.join(", ")}` : "";
			lines.push(
				`- ${up.name}: ATTRIBUTED [${entry.section}] ${entry.r} → ${entry.owner}${fieldNote}`,
			);
			attribution[entry.section]?.push(`${mode}/${up.name} (${entry.r} → ${entry.owner})`);
			continue;
		}
		lines.push(
			`- ${up.name}: ${TRACK === "target" ? "UNATTRIBUTED" : "MISMATCH"}\n` +
				`  upstream: ${JSON.stringify(diff.upstream)}\n` +
				`  rust:     ${JSON.stringify(diff.rust)}`,
		);
		if (TRACK === "target") {
			unattributed.push(`${mode}/${up.name}`);
		} else {
			ok = false;
		}
	}
	report.push(`## ${mode}\n\n${lines.join("\n")}\n`);
}

if (RECORD_ARGS_GOLDEN) {
	const modeFile = materialize("args");
	const entries = runUpstream("args", modeFile);
	const golden = {
		"//":
			"Frozen argv/env baseline for the subagents target track ([RPI-OWN], ADR-0025 §4): " +
			"recorded from the pinned v0.48.0 upstream leg (pi-args.ts) before the v0.65 deletion. " +
			"Regenerate with `node scripts/subagents-parity/run-parity.mjs --record-args-golden`.",
		cases: entries.map((entry) => ({ name: entry.name, output: normalizeOutput(entry.output) })),
	};
	writeFileSync(GOLDEN_PATH, JSON.stringify(golden, null, 2) + "\n");
	console.log(`recorded ${golden.cases.length} args cases -> ${GOLDEN_PATH}`);
	process.exit(0);
}

mkdirSync(GENERATED, { recursive: true });
const report = [
	TRACK === "target"
		? "# subagents parity report (target track: pi-subagents v0.66.0 @ 0fc0eebb)"
		: "# subagents parity report (TE04 G3)",
	"",
	`generated: ${new Date().toISOString()}`,
	"",
];
for (const mode of MODES) {
	compareMode(mode, report);
}
if (TRACK === "target") {
	report.push("## Attribution summary", "");
	for (const section of ["upstream-semantics", "rpi-deviation"]) {
		report.push(`### ${section}`, "");
		report.push(
			attribution[section].length === 0
				? "- (none)"
				: attribution[section].map((item) => `- ${item}`).join("\n"),
		);
		report.push("");
	}
	report.push("### unattributed", "");
	report.push(unattributed.length === 0 ? "- (none)" : unattributed.map((item) => `- ${item}`).join("\n"));
	report.push("");
	if (unattributed.length > 0) ok = false;
	const attributedCount = attribution["upstream-semantics"].length + attribution["rpi-deviation"].length;
	const result = !ok ? "UNATTRIBUTED DIFF" : attributedCount === 0 ? "MATCH" : "ATTRIBUTED-OK";
	report.push("", `## RESULT: ${result}`);
} else {
	report.push("", ok ? "## RESULT: MATCH" : "## RESULT: MISMATCH");
}
const reportPath = `${GENERATED}/parity-report.md`;
writeFileSync(reportPath, report.join("\n") + "\n");
console.log(report.join("\n"));
process.exit(ok ? 0 : 1);
