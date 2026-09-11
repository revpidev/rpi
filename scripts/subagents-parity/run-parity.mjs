// Orchestrator of the subagents parity harness (TE04 G3; dual-track TE13;
// target track made the default by TE27 when the submodule pin switched to
// v0.66.0 — the old-pin regression track's lifecycle ended there, its
// historical reports are kept under fixtures/generated/subagents-parity/).
//
//   node scripts/subagents-parity/run-parity.mjs [--track=target|regression]
//   node scripts/subagents-parity/run-parity.mjs --record-args-golden
//
// The Rust leg is built by this script and copied to a private path before
// execution: both plugin crates ship an example named `parity_runner`, and
// the unsuffixed target/debug/examples/parity_runner belongs to whichever
// crate built last (the mcp harness would shadow it otherwise).
//
// Track `target` (default since TE27; pi-subagents v0.66.0, ADR-0025):
//   1. argv/env: Rust vs the frozen v0.48 golden ([RPI-OWN], ADR-0025 §4;
//      upstream deleted `pi-args.ts` in v0.65).
//   2. frontmatter/final-output/fallback: Rust vs the v0.66 snapshot modules
//      extracted by setup-target-source.sh.
//   3. discovery (TE15): the case tree is materialized per side (`.pi`
//      upstream / `.rpi` rpi, both normalized to `<CFGDIR>`) and both legs
//      run their real discovery entry point; agents + diagnostics are diffed.
//   4. Diffs are attributed through expected-target-diffs.json
//      (`upstream-semantics` vs `rpi-deviation`, each with R + owner task);
//      writes fixtures/generated/subagents-parity-v066/parity-report.md.
//   Non-zero exit = any UNATTRIBUTED diff.
//
// Track `regression` (retired by TE27; kept for archaeology — requires the
// old-pin worktree, i.e. `git -C external/pi-subagents checkout 56f97234`
// followed by restoring the v0.66.0 pin afterwards):
//   1. Runs the pinned upstream v0.48 modules (tsx) on the shared fixtures.
//   2. Runs the Rust parity_runner example on the same fixtures.
//   3. Normalizes both sides and diffs; writes
//      fixtures/generated/subagents-parity/parity-report.md.
//   Non-zero exit = any case mismatched (expects the rpi crate at v0.48
//   semantics — will not hold after the rebase batches).
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
const TRACK = TRACK_FLAG ? TRACK_FLAG.slice("--track=".length) : "target";
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
		? ["args", "frontmatter", "final-output", "fallback", "model", "discovery", "notify"]
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

function materialize(mode, caseSubset) {
	const raw = JSON.stringify(caseSubset ?? loadCases(mode));
	const rewritten = raw.replaceAll("/sess/root", `${SESSION_BASE}/sess/root`);
	const cases = JSON.parse(rewritten);
	const modeFile = `${CASES_DIR}/cases-${TRACK}-${mode}${caseSubset ? "-subset" : ""}.json`;
	writeFileSync(modeFile, JSON.stringify({ cases }));
	return modeFile;
}

// Both legs run with the ambient subagent/parent-session env cleared. pi
// forwards parent env into subagent children under the `PI_SUBAGENTS_` prefix
// (e.g. `PI_SUBAGENTS_PI_CODING_AGENT_PACKAGE_ROOT`, the package-root key
// `pi-args.ts:641` copies into the child env) and exports `PI_SUBAGENT_*`
// keys; the Rust leg has no matching ambient keys, so a value exported in the
// surrounding shell reaches exactly one leg and the args cases mismatch
// spuriously (2026-09-09: running inside a pi subagent session produced eight
// false MISMATCHes). The harness's own keys are added by the caller *after*
// cleaning, so they survive.
function cleanSessionEnv(env) {
	const cleaned = {};
	for (const [key, value] of Object.entries(env)) {
		if (key.startsWith("PI_SUBAGENT") || key.startsWith("RPI_SUBAGENT")) continue;
		cleaned[key] = value;
	}
	return cleaned;
}

function runUpstream(mode, modeFile) {
	const result = spawnSync(TSX, [`${HERE}/upstream-runner.mjs`, mode, modeFile], {
		encoding: "utf-8",
		env: {
			...cleanSessionEnv(process.env),
			PI_CODING_AGENT_PACKAGE_ROOT: "/tmp",
			RPI_SUBAGENTS_PARITY_TRACK: TRACK,
		},
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
		env: cleanSessionEnv({
			...process.env,
			// TE19 (#1318): isolate the model-exclusion store per harness run
			// — recorded exclusions from fixtures (or an earlier e2e run on
			// the default path) must not silently drop fixture candidates
			// from the model legs.
			RPI_MODEL_EXCLUSIONS_PATH: `${tmpdir()}/rpi-subagents-parity-exclusions-${process.pid}.json`,
		}),
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
			// TE18 (ADR-0026): the rpi-side switch for upstream #1560's
			// in-process `inheritGlobalContext: false` default — no argv/env
			// counterpart on either pin; its presence is pinned by crate unit
			// tests + the e2e env dump instead of this diff.
			"RPI_NO_GLOBAL_CONTEXT",
			// TE19 (#1397/#1615): v0.66 launch-contract additions with no
			// v0.48 argv/env counterpart — the intersected thinking ceiling
			// (thinking-ceiling.ts, upstream threads it in-process) and the
			// child session display name (child-session-name.ts, upstream
			// rides the child runtime config). Pinned by crate unit tests +
			// the e2e env dump.
			"RPI_SUBAGENT_THINKING_CEILING",
			"RPI_SUBAGENT_SESSION_NAME",
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

// TE18: target-track args face = frozen v0.48 golden cases (upstream leg
// reads the golden) + [RPI-OWN] inline-expected cases for semantics no
// upstream recorder has (upstream deleted pi-args.ts before gaining
// --exclude-tools; ADR-0025 §4). Inline cases compare the Rust leg against
// the `expected` object shipped in the fixture.
function compareArgsTarget(report) {
	const all = loadCases("args");
	const goldenCases = all.filter((entry) => !entry.expected);
	const inlineCases = all.filter((entry) => entry.expected);
	if (goldenCases.length > 0) {
		compareMode("args", report, goldenCases);
	}
	const lines = [];
	if (inlineCases.length > 0) {
		const rust = runRust("args", materialize("args", inlineCases));
		for (const fixture of inlineCases) {
			const produced = rust.find((entry) => entry.name === fixture.name);
			if (!produced) {
				lines.push(`- ${fixture.name}: MISSING FROM RUST LEG`);
				ok = false;
				continue;
			}
			const diff = diffOutput(fixture.expected, produced.output);
			if (!diff) {
				lines.push(`- ${fixture.name}: MATCH (inline [RPI-OWN] golden)`);
			} else {
				lines.push(
					`- ${fixture.name}: MISMATCH (inline [RPI-OWN] golden)\n` +
						`  expected: ${JSON.stringify(diff.upstream)}\n` +
						`  rust:     ${JSON.stringify(diff.rust)}`,
			);
				ok = false;
			}
		}
		report.push(`## args (inline [RPI-OWN] golden)\n\n${lines.join("\n")}\n`);
	}
}

function compareMode(mode, report, caseSubset) {
	const modeFile = materialize(mode, caseSubset);
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
	// Only the fixtures.json (v0.48-recorded) cases go through the recorder;
	// TE18 inline-expected cases (excludeTools/global-context semantics) have
	// no upstream recorder and live in fixtures-target.json with their own
	// expected objects.
	const recordable = loadCases("args").filter((entry) => !entry.expected);
	const modeFile = materialize("args", recordable);
	const entries = runUpstream("args", modeFile);
	const golden = {
		"//":
			"Frozen argv/env baseline for the subagents target track ([RPI-OWN], ADR-0025 §4): " +
			"recorded from the pinned v0.48.0 upstream leg (pi-args.ts) before the v0.65 deletion. " +
			"Regenerate with `node scripts/subagents-parity/run-parity.mjs --record-args-golden` " +
			"(records only the fixtures.json args cases; TE18+ semantics cases carry inline " +
			"expected objects in fixtures-target.json — see compareArgsTarget).",
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
	if (mode === "args" && TRACK === "target") {
		compareArgsTarget(report);
	} else {
		compareMode(mode, report);
	}
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
