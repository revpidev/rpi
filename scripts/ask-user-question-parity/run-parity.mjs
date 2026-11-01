// Orchestrator of the ask-user-question parity harness (TE28 G3/G12).
//
//   node scripts/ask-user-question-parity/run-parity.mjs
//
// 1. Verifies the pinned submodule HEAD (external/rpiv-mono @ afc21bc2) and
//    materializes a read-only snapshot of the six upstream pure-function
//    modules into the deps dir (external/ is never written).
// 2. Builds this crate's `ask_user_question_parity_runner` example. All
//    workspace parity examples are uniquely named (P2-9), so cargo's shared
//    target/debug/examples directory has no same-name collision; building
//    here keeps the harness independent of build order anyway.
// 3. Runs the upstream modules (tsx) on the shared fixtures.
// 4. Runs the Rust ask_user_question_parity_runner example on the same fixtures.
// 5. Normalizes both sides (key-order-insensitive deep compare) and diffs.
// 6. Checks the vendored locale tables byte-for-byte against the submodule.
// 7. Writes fixtures/generated/ask-user-question-parity-<track>/{parity-report.md,
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
	rmSync,
	writeFileSync,
} from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "../..");
const MONO = resolve(REPO, "external/rpiv-mono");
const ASKQ_PKG = "packages/rpiv-ask-user-question";
const DEPS = process.env.RPI_ASKQ_PARITY_DEPS ?? "/tmp/rpi-ask-user-question-parity-deps";
const TSX = resolve(DEPS, "node_modules/.bin/tsx");
const RUST_RUNNER = resolve(REPO, "target/debug/examples/ask_user_question_parity_runner");
const VENDORED_LOCALES = resolve(REPO, "crates/rpi-ext-ask-user-question/locales");

// TE51 dual-track rotation (ADR-0038): `target` = the v0.1.8 target pin
// rpiv-mono v2.15.0 @ ce6d3530; `regression` = the retired v0.1.7 pin
// v2.13.8 @ afc21bc2 (the v0.1.6/v0.1.7 parity baseline). Both snapshots are
// extracted from the git object database (`git show`), so external/ is never
// written and the submodule HEAD may sit on either pin while TE51 lands.
const TRACK_FLAG = process.argv.find((arg) => arg.startsWith("--track="));
const TRACK = TRACK_FLAG ? TRACK_FLAG.slice("--track=".length) : "target";
if (!["regression", "target"].includes(TRACK)) {
	console.error(`unknown track: ${TRACK}`);
	process.exit(2);
}
const TARGET_PIN = "ce6d3530410812ca504ec294995581a52170f9e1";
const REGRESSION_PIN = "afc21bc25f7d07cae9fb3103dc309240a16b9f49";
const PIN = TRACK === "target" ? TARGET_PIN : REGRESSION_PIN;
const TRACK_LABEL = TRACK === "target" ? "v2150" : "v2138";
const SNAPSHOT = resolve(DEPS, `snapshot-${TRACK_LABEL}`);
const GENERATED = resolve(
	REPO,
	TRACK === "target"
		? "fixtures/generated/ask-user-question-parity-v2150"
		: "fixtures/generated/ask-user-question-parity-v2138",
);
// Target-track attribution manifest (TE51 seed): every remaining difference
// carries a section + R + owner task; the track lands green once FR-G/FR-H
// are implemented and the manifest empties out.
const MANIFEST_PATH = resolve(
	HERE,
	TRACK === "target" ? "expected-diffs-v2150.json" : "expected-diffs-v2138.json",
);
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
const UPSTREAM_TUI_KEYS = resolve(REPO, "external/pi/packages/tui/src/keys.ts");
const UPSTREAM_TUI_UTILS = resolve(REPO, "external/pi/packages/tui/src/utils.ts");
const GOLDEN_DIR = resolve(GENERATED, "golden-frames");

function sha256(path) {
	return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function run(command, args, options = {}) {
	return spawnSync(command, args, { encoding: "utf-8", ...options });
}

function submoduleHead() {
	const result = run("git", ["-C", MONO, "rev-parse", "HEAD"]);
	return result.status === 0 ? result.stdout.trim() : `unknown (${result.stderr.trim()})`;
}

/** Read one tracked file at a pin from the git object database (`git show`);
 * external/ stays read-only, and either pin is reachable while the gitlink is
 * still on the regression commit. */
function gitShow(pin, path) {
	const result = run("git", ["-C", MONO, "show", `${pin}:${path}`], { maxBuffer: 64 * 1024 * 1024 });
	if (result.status !== 0) {
		throw new Error(`git show ${pin}:${path} failed:\n${result.stderr}`);
	}
	return result.stdout;
}

/** Locale file names at a pin (`git ls-tree` on the package locales dir). */
function localeFilesAt(pin) {
	const result = run("git", ["-C", MONO, "ls-tree", "--name-only", `${pin}:${ASKQ_PKG}/locales`]);
	if (result.status !== 0) {
		throw new Error(`git ls-tree ${pin}:${ASKQ_PKG}/locales failed:\n${result.stderr}`);
	}
	return result.stdout
		.split("\n")
		.map((line) => line.trim())
		.filter((line) => line.endsWith(".json"))
		.sort();
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
	rmSync(SNAPSHOT, { recursive: true, force: true });
	mkdirSync(resolve(SNAPSHOT, "tool"), { recursive: true });
	mkdirSync(resolve(SNAPSHOT, "state"), { recursive: true });
	mkdirSync(resolve(SNAPSHOT, "view/components/preview"), { recursive: true });
	const hashes = [];
	for (const module of UPSTREAM_MODULES) {
		const content = gitShow(PIN, `${ASKQ_PKG}/${module}`);
		const to = resolve(SNAPSHOT, module);
		mkdirSync(dirname(to), { recursive: true });
		writeFileSync(to, content);
		hashes.push({ module, sha256: createHash("sha256").update(content).digest("hex") });
	}
	// Stub `@earendil-works/pi-tui` for the snapshot imports: verbatim copies
	// of the pinned upstream keys + utils modules, resolved by Node from the
	// deps node_modules (external/ stays read-only; utils.ts's
	// `get-east-asian-width` import resolves from the deps install). The tui
	// pin is external/pi (unchanged by TE51), not the rpiv pin.
	const stubDir = resolve(DEPS, "node_modules/@earendil-works/pi-tui");
	mkdirSync(stubDir, { recursive: true });
	writeFileSync(
		resolve(stubDir, "package.json"),
		`${JSON.stringify({ name: "@earendil-works/pi-tui", version: "0.0.0-harness", type: "module", main: "index.ts" }, null, 2)}\n`,
	);
	copyFileSync(UPSTREAM_TUI_KEYS, resolve(stubDir, "keys.ts"));
	copyFileSync(UPSTREAM_TUI_UTILS, resolve(stubDir, "utils.ts"));
	writeFileSync(
		resolve(stubDir, "index.ts"),
		'export * from "./keys.ts";\nexport * from "./utils.ts";\n',
	);
	hashes.push({
		module: "external/pi/packages/tui/src/keys.ts (pi-tui stub)",
		sha256: sha256(UPSTREAM_TUI_KEYS),
	});
	hashes.push({
		module: "external/pi/packages/tui/src/utils.ts (pi-tui stub, TE31)",
		sha256: sha256(UPSTREAM_TUI_UTILS),
	});
	return hashes;
}

function runUpstream(group) {
	const result = run(
		TSX,
		[resolve(HERE, "upstream-runner.mjs"), group, resolve(HERE, "fixtures.json")],
		{ env: { ...process.env, PARITY_SNAPSHOT: SNAPSHOT, PARITY_DEPS: DEPS } },
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
		["build", "-p", "rpi-ext-ask-user-question", "--example", "ask_user_question_parity_runner"],
		{ cwd: REPO },
	);
	if (result.status !== 0) {
		throw new Error(`cargo build ask_user_question_parity_runner failed:\n${result.stderr}\n${result.stdout}`);
	}
}

function runRust(group) {
	if (!existsSync(RUST_RUNNER)) {
		throw new Error(
			`missing ${RUST_RUNNER}; build it first: cargo build -p rpi-ext-ask-user-question --example ask_user_question_parity_runner`,
		);
	}
	const result = run(RUST_RUNNER, [group, resolve(HERE, "fixtures.json")]);
	if (result.status !== 0) {
		throw new Error(`rust ask_user_question_parity_runner (${group}) failed:\n${result.stderr}\n${result.stdout}`);
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

/** `{ cases: [{ key, section, r, owner }] }` keyed by `group/case`. */
function loadManifest() {
	const byKey = new Map();
	if (!existsSync(MANIFEST_PATH)) return byKey;
	const raw = JSON.parse(readFileSync(MANIFEST_PATH, "utf-8"));
	for (const entry of raw.cases ?? []) {
		if (entry?.key) byKey.set(entry.key, entry);
	}
	return byKey;
}

const attribution = [];

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
	const manifest = loadManifest();
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
			const entry = manifest.get(`${group}/${up.name}`);
			if (entry) {
				lines.push(
					`- ${up.name}: ATTRIBUTED [${entry.section}] ${entry.r} → ${entry.owner}`,
				);
				attribution.push(`${group}/${up.name} (${entry.r} → ${entry.owner})`);
			} else {
				lines.push(
					`- ${up.name}: UNATTRIBUTED\n  upstream: ${canonical(up.output)}\n  rust:     ${canonical(rs.output)}`,
				);
				allMatch = false;
			}
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
	const files = localeFilesAt(TARGET_PIN);
	const lines = [];
	let allMatch = true;
	for (const file of files) {
		const upstreamContent = gitShow(TARGET_PIN, `${ASKQ_PKG}/locales/${file}`);
		const vendoredFile = resolve(VENDORED_LOCALES, file);
		if (!existsSync(vendoredFile)) {
			lines.push(`- ${file}: MISSING in crates/rpi-ext-ask-user-question/locales/`);
			allMatch = false;
			continue;
		}
		const upstreamHash = createHash("sha256").update(upstreamContent).digest("hex");
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
	report.push(`## locales (against the v2150 target pin)\n\n${lines.join("\n")}\n`);
	return allMatch;
}

function main() {
	ensureDeps();
	buildRustRunner();
	const head = submoduleHead();
	const snapshotHashes = materializeSnapshot();
	mkdirSync(GENERATED, { recursive: true });

	const report = [
		`# ask-user-question parity report (${TRACK} track: rpiv-mono ${TRACK_LABEL} @ ${PIN.slice(0, 10)} — TE28/TE29/TE30/TE31 G3/G12, dual-track TE51)`,
		"",
		`generated: ${new Date().toISOString()}`,
		`upstream submodule: external/rpiv-mono (${ASKQ_PKG})`,
		`submodule HEAD: ${head} (track pin ${PIN})`,
		`typebox: ${typeboxPin()}`,
		"",
		"## snapshot (sha256 of the driven upstream modules)",
		"",
		...snapshotHashes.map((entry) => `- ${entry.module}: ${entry.sha256}`),
		"",
	];

	// The gitlink may sit on either pin while TE51 lands: target snapshots are
	// extracted from the object DB, so only an unrelated commit is a real
	// mismatch.
	let ok = head === TARGET_PIN || head === REGRESSION_PIN;
	if (!ok) {
		report.push(
			`## PIN MISMATCH\n\nsubmodule HEAD ${head} is neither the target pin ${TARGET_PIN} nor the regression pin ${REGRESSION_PIN}\n`,
		);
	}
	for (const group of GROUPS) {
		ok = compareGroup(group, report) && ok;
	}
	// The golden frames are rendered by the Rust leg only (no upstream
	// renderer exists); record/compare them on the target track only.
	if (TRACK === "target") {
		ok = compareGoldenFrames(report) && ok;
	}
	ok = compareLocales(report) && ok;
	report.push(
		"",
		"## Attribution summary",
		"",
		attribution.length === 0 ? "- (none)" : attribution.map((item) => `- ${item}`).join("\n"),
		"",
		ok ? "## RESULT: MATCH" : "## RESULT: MISMATCH",
	);

	writeFileSync(resolve(GENERATED, "parity-report.md"), `${report.join("\n")}\n`);
	console.log(report.join("\n"));
	process.exit(ok ? 0 : 1);
}

main();
