// Upstream leg of the subagents parity harness (TE04 G3; dual-track TE13;
// re-rotated by TE37 for the v0.1.5 window, ADR-0029).
//
// Track `regression` (default): the live submodule worktree — pi-subagents
// v0.66.0 @ 0fc0eebb until TE39 switches the pin (the zero-regression
// baseline of the window).
//
// Track `target`: the v0.70.0 snapshot extracted by `setup-target-source.sh`
// (never a checkout of `external/`) into /tmp/rpi-subagents-parity-target-v070.
//
// Mode roots (TE37 skeleton facts, verified against both pins):
//   - frontmatter / final-output / discovery / notify: the track root
//     (all four module faces exist unchanged at v0.70).
//   - argv/env (`args`): the frozen v0.48 golden on BOTH tracks
//     ([RPI-OWN], ADR-0025 §4 — upstream deleted pi-args.ts in v0.65 and it
//     stayed deleted at v0.70; there is no live upstream face to drive).
//   - fallback / model: re-anchored at v0.70 on both tracks (TE39 followed
//     the #2270 removal — src/runs/shared/model-fallback.ts deleted; the
//     retained surface lives in model-resolution.ts: isContextOverflow +
//     resolveSubagentModelOverride + resolveModelSelection for the surviving
//     single-candidate vectors; the retryable/attempt fixtures retired).
//
// Prints normalized JSON lines that the orchestrator diffs against the Rust
// parity_runner example.
//
// Run via run-parity.mjs — never run inside external/ (the submodule stays
// read-only; nothing here writes to it).
import { mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, isAbsolute, join, relative, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const TRACK = process.env.RPI_SUBAGENTS_PARITY_TRACK ?? "regression";
// regression = the CURRENT-pin snapshot (v0.66.0 @ 0fc0eebb until TE39; the
// live worktree cannot serve directly — its discovery chain imports `yaml`,
// unresolvable from a pristine external/); target = the v0.70.0 snapshot
// (TE37, ADR-0029). Both are extracted by setup-target-source.sh.
const REGRESSION_ROOT =
	process.env.RPI_SUBAGENTS_REGRESSION_SRC ?? "/tmp/rpi-subagents-parity-regression-v066";
const TARGET_ROOT =
	process.env.RPI_SUBAGENTS_TARGET_SRC ?? "/tmp/rpi-subagents-parity-target-v070";
const ARGS_GOLDEN = resolve(HERE, "args-golden-v048.json");

// Normalize an argv array the same way the Rust runner does.
function normalizeArgv(args) {
	const out = [];
	let skipValue = null;
	for (const arg of args) {
		if (skipValue) {
			out.push(skipValue);
			skipValue = null;
			continue;
		}
		if (arg === "--system-prompt" || arg === "--append-system-prompt") {
			out.push(arg);
			skipValue = "<PROMPT_FILE>";
			continue;
		}
		if (arg === "--extension") {
			out.push(arg);
			skipValue = "<EXT>";
			continue;
		}
		if (arg.startsWith("@")) out.push("@<TASK_FILE>");
		else out.push(arg);
	}
	return out;
}

// Env keys upstream sets for features rpi defers to P1/P2; dropped from the
// comparison (documented in README.md).
const DROPPED_ENV_KEYS = new Set([
	"PI_SUBAGENT_RUNTIME_ACKNOWLEDGED_EXTENSIONS",
	// Package-root propagation has two historical names (utils.ts:19 uses the
	// prefixed one); either side may carry it when the package is resolvable.
	"PI_CODING_AGENT_PACKAGE_ROOT",
	"PI_SUBAGENTS_PI_CODING_AGENT_PACKAGE_ROOT",
]);

function normalizeEnv(env) {
	const out = {};
	for (const [key, value] of Object.entries(env)) {
		if (value === undefined) continue;
		if (DROPPED_ENV_KEYS.has(key)) continue;
		out[key.replace(/^PI_SUBAGENT_/, "RPI_SUBAGENT_")] = value;
	}
	return out;
}

function moduleUrl(root, relative) {
	return pathToFileURL(resolve(root, relative)).href;
}

async function loadUpstream() {
	const root = trackRoot();
	const frontmatter = await import(moduleUrl(root, "src/agents/frontmatter.ts"));
	const utils = await import(moduleUrl(root, "src/shared/utils.ts"));
	// fallback/model face: re-anchored at v0.70 by TE39 (#2270 deleted
	// src/runs/shared/model-fallback.ts; model-resolution.ts keeps
	// resolveSubagentModelOverride / isContextOverflow and the single-model
	// launch path — buildModelCandidates is gone, so the candidates
	// vectors run against resolveModelSelection on this side).
	const modelResolution = await import(
		moduleUrl(TARGET_ROOT, "src/runs/shared/model-resolution.ts"),
	);
	return { frontmatter, utils, modelResolution };
}

// The track's expectation root: the current-pin regression snapshot or the
// v0.70 target snapshot (both extracted by setup-target-source.sh).
function trackRoot() {
	return TRACK === "target" ? TARGET_ROOT : REGRESSION_ROOT;
}

// The v0.48 pi-args face (buildPiArgs) is gone from every reachable pin
// (deleted upstream in v0.65). The args mode reads the frozen golden on
// both tracks; buildArgsCase survives only for --record-args-golden runs
// against a v0.48-era worktree, gated behind
// RPI_SUBAGENTS_PARITY_ARGS_LEGACY=1 (it throws otherwise).
let legacyPiArgsModule;
function legacyPiArgs() {
	if (!process.env.RPI_SUBAGENTS_PARITY_ARGS_LEGACY) {
		throw new Error(
			"pi-args.ts is deleted at every reachable pin (v0.65+); the args face "
				+ "is the frozen golden — set RPI_SUBAGENTS_PARITY_ARGS_LEGACY=1 with a "
				+ "v0.48-era worktree to re-record",
		);
	}
	legacyPiArgsModule ??= import(moduleUrl(REGRESSION_ROOT, "src/runs/shared/pi-args.ts"));
	return legacyPiArgsModule;
}

function buildArgsCase(input) {
	try {
		const result = legacyPiArgs().buildPiArgs({
			baseArgs: ["--mode", "json", "-p"],
			task: input.task ?? "",
			taskDelivery: input.taskDelivery === "file" ? "file" : undefined,
			sessionEnabled: input.sessionEnabled !== false,
			sessionDir: input.sessionDir ?? undefined,
			sessionFile: input.sessionFile ?? undefined,
			// The runner cwd only feeds MCP direct-tool resolution (P2).
			model: input.model ?? undefined,
			thinking: input.thinking ?? undefined,
			systemPrompt: input.systemPrompt ?? undefined,
			systemPromptMode: input.systemPromptMode === "append" ? "append" : "replace",
			inheritProjectContext: input.inheritProjectContext === true,
			inheritSkills: input.inheritSkills === true,
			requireReadTool: input.requireReadTool === true,
			tools: input.tools ?? undefined,
			extensions: input.extensions ?? undefined,
			subagentOnlyExtensions: input.subagentOnlyExtensions ?? undefined,
			mcpDirectTools: undefined,
			cwd: process.cwd(),
			promptFileStem: input.promptFileStem ?? undefined,
			runId: input.runId ?? undefined,
			childAgentName: input.childAgentName ?? undefined,
			childIndex: input.childIndex ?? undefined,
			parentSessionId: input.parentSessionId ?? undefined,
		});
		return {
			ok: true,
			argv: normalizeArgv(result.args),
			env: normalizeEnv(result.env),
		};
	} catch (error) {
		return { ok: false, error: String(error?.message ?? error) };
	}
}

function frontmatterCase(frontmatter, content) {
	const parsed = frontmatter.parseFrontmatter(content);
	const sorted = {};
	for (const key of Object.keys(parsed.frontmatter).sort()) {
		sorted[key] = parsed.frontmatter[key];
	}
	return {
		frontmatter: sorted,
		body: parsed.body,
		tools: parsed.frontmatter.tools === undefined
			? undefined
			: frontmatter.parseFrontmatterList(parsed.frontmatter.tools),
	};
}

// Post-#2270 (TE39): only the context-overflow classifier survives; the
// retryable/attempt kinds went with the v0.70 fallback removal and their
// fixtures were retired.
function fallbackCase(modelResolution, fixture) {
	switch (fixture.kind) {
		case "context-overflow":
			return { contextOverflow: modelResolution.isContextOverflow(fixture.error) };
		default:
			throw new Error(`unknown fallback fixture kind: ${fixture.kind}`);
	}
}

// TE17 notify leg (R7.1.7.2): drive the real track-root
// formatSingleCompletion / parseSubagentNotifyContent (both faces exist
// unchanged at v0.70). The parse projection mirrors the Rust leg's
// notify_projection (undefined-dropping serializer == null-stripping).
// TE18 (R7.1.4.4/.5, #1093), re-anchored at v0.70 by TE39: override vectors
// drive resolveSubagentModelOverride (unchanged for these inputs — the
// v0.66 exclusion check never applied on a fresh process); the surviving
// candidates vectors (no fallbacks) drive resolveModelSelection, the v0.70
// single-model launch resolution matching the Rust single-candidate shape
// post-#2270. Throws surface as { error } so a fail-closed throw on this
// side diffs against a value (or error) from the Rust leg.
function modelCase(modelResolution, fixture) {
	const registry = fixture.registry === undefined || fixture.registry === null
		? undefined
		: fixture.registry.map((entry) => ({
				fullId: entry.fullId,
				provider: entry.provider,
				id: entry.id,
		}));
	const parentModel = typeof fixture.parentModel === "string" && fixture.parentModel.includes("/")
		? (() => {
			const [provider, ...rest] = fixture.parentModel.split("/");
			return { provider, id: rest.join("/") };
		})()
		: undefined;
	if (fixture.kind === "override") {
		try {
			const resolved = modelResolution.resolveSubagentModelOverride(
				fixture.model ?? undefined,
				parentModel,
				registry,
				fixture.preferredProvider ?? undefined,
				{ source: fixture.source === "explicit" ? "explicit" : "inherited" },
			);
			return { resolved };
		} catch (error) {
			return { error: String(error?.message ?? error) };
		}
	}
	if (fixture.kind === "candidates") {
		try {
			const origin = ["explicit", "inherited", "configured"].includes(fixture.origin)
				? fixture.origin
				: "configured";
			const selection = modelResolution.resolveModelSelection(fixture.primary ?? undefined, registry, fixture.preferredProvider ?? undefined, { origin });
			const candidates = selection.model === undefined ? [] : [selection.model];
			return { candidates };
		} catch (error) {
			return { error: String(error?.message ?? error) };
		}
	}
	return { error: `unknown model fixture kind: ${fixture.kind}` };
}

function notifyCase(notify, fixture) {
	const kind = fixture.kind ?? "format";
	if (kind === "format") {
		return { text: notify.formatSingleCompletion(fixture.details ?? {}) };
	}
	if (kind === "parse") {
		const parsed = notify.parseSubagentNotifyContent(fixture.content ?? "");
		if (!parsed) return null;
		return {
			agent: parsed.agent,
			status: parsed.status,
			source: parsed.source,
			taskInfo: parsed.taskInfo,
			resultPreview: parsed.resultPreview,
			runId: parsed.childRuns?.[0]?.runId ?? parsed.workflowRunId,
			handoffPath: parsed.handoffPath,
			sessionLabel: parsed.sessionLabel,
			sessionValue: parsed.sessionValue,
		};
	}
	throw new Error(`unknown notify fixture kind: ${kind}`);
}

function loadArgsGolden() {
	const golden = JSON.parse(readFileSync(ARGS_GOLDEN, "utf-8"));
	const byName = new Map();
	for (const entry of golden.cases ?? []) byName.set(entry.name, entry.output);
	return byName;
}

// TE15 discovery-tree leg (R7.1.3): materialize the case tree under a sandbox
// user-agent dir with the upstream config-dir name (`.pi`; the Rust leg uses
// `.rpi`, both map back to `<CFGDIR>`), then drive the real v0.66
// `discoverAgents` in `user` scope. Scope `user` keeps discovery uncached
// (`discoverAgentsUncached`), so repeated cases in one process stay isolated.
// Returns normalized agents + diagnostics; the Rust runner emits the same
// shape.
async function discoveryCase(fixture) {
	const root = mkdtempSync(join(tmpdir(), "rpi-sub-discovery-parity-up-"));
	const home = join(root, "home");
	const agentDir = join(root, "agentdir");
	const userDir = join(agentDir, "agents");
	const projectDir = join(root, "proj");
	mkdirSync(home, { recursive: true });
	mkdirSync(userDir, { recursive: true });
	mkdirSync(projectDir, { recursive: true });
	const cfgDir = ".pi";
	for (const [rawPath, content] of Object.entries(fixture.tree?.files ?? {})) {
		const target = join(userDir, rawPath.replaceAll("<CFGDIR>", cfgDir));
		mkdirSync(dirname(target), { recursive: true });
		writeFileSync(target, content);
	}
	// Symlinks are not portable to Windows checkouts; both legs skip them there
	// (materialize.json records the same limitation).
	if (process.platform !== "win32") {
		for (const link of fixture.tree?.symlinks ?? []) {
			symlinkSync(link.target, join(userDir, link.path), "dir");
		}
	}
	const saved = {
		HOME: process.env.HOME,
		USERPROFILE: process.env.USERPROFILE,
		PI_CODING_AGENT_DIR: process.env.PI_CODING_AGENT_DIR,
		PI_OFFLINE: process.env.PI_OFFLINE,
		PI_SUBAGENT_EXTRA_AGENT_DIRS: process.env.PI_SUBAGENT_EXTRA_AGENT_DIRS,
	};
	process.env.HOME = home;
	process.env.USERPROFILE = home;
	process.env.PI_CODING_AGENT_DIR = agentDir;
	process.env.PI_OFFLINE = "1";
	delete process.env.PI_SUBAGENT_EXTRA_AGENT_DIRS;
	try {
		const { discoverAgents } = await import(moduleUrl(trackRoot(), "src/agents/agents.ts"));
		const result = discoverAgents(projectDir, "user");
		const under = (filePath) => {
			const rel = relative(userDir, filePath);
			return rel !== "" && !rel.startsWith("..") && !isAbsolute(rel);
		};
		const relativize = (filePath) => {
			const rel = relative(userDir, filePath).split("\\").join("/");
			return rel.startsWith(`${cfgDir}/`) ? `<CFGDIR>/${rel.slice(cfgDir.length + 1)}` : rel;
		};
		const agents = result.agents
			.filter((agent) => under(agent.filePath))
			.map((agent) => ({ name: agent.name, source: agent.source, path: relativize(agent.filePath) }))
			.sort((a, b) => a.name.localeCompare(b.name));
		const diagnostics = (result.agentDiagnostics ?? [])
			.filter((diagnostic) => under(diagnostic.filePath))
			.map((diagnostic) => ({ path: relativize(diagnostic.filePath), source: diagnostic.source, error: diagnostic.error }))
			.sort((a, b) => a.path.localeCompare(b.path));
		return { agents, diagnostics };
	} finally {
		for (const [key, value] of Object.entries(saved)) {
			if (value === undefined) delete process.env[key];
			else process.env[key] = value;
		}
		rmSync(root, { recursive: true, force: true });
	}
}

async function main() {
	const mode = process.argv[2];
	const fixturePath = process.argv[3];
	if (!mode || !fixturePath) {
		console.error(
			"usage: upstream-runner.mjs <args|frontmatter|final-output|fallback|model|discovery|notify> <fixture.json>",
		);
		process.exit(2);
	}
	const { frontmatter, utils, modelResolution } = await loadUpstream();
	// TE17: the notify module exists on both pins (face unchanged at v0.70),
	// loaded from the track root.
	const notify = mode === "notify"
		? await import(moduleUrl(trackRoot(), "src/runs/background/notify.ts"))
		: undefined;
	const fixtures = JSON.parse(readFileSync(fixturePath, "utf-8"));
	const golden = mode === "args" ? loadArgsGolden() : null;
	for (const fixture of fixtures.cases ?? []) {
		let output;
		if (mode === "args") {
			output = golden
				? golden.get(fixture.name)
				: buildArgsCase(fixture.input ?? {});
			if (output === undefined) {
				throw new Error(`args golden missing case ${fixture.name}`);
			}
		} else if (mode === "frontmatter") {
			output = frontmatterCase(frontmatter, fixture.content ?? "");
		} else if (mode === "final-output") {
			output = utils.getFinalOutput(fixture.messages ?? []);
		} else if (mode === "fallback") {
			output = fallbackCase(modelResolution, fixture);
		} else if (mode === "model") {
			output = modelCase(modelResolution, fixture);
		} else if (mode === "discovery") {
			output = await discoveryCase(fixture);
		} else if (mode === "notify") {
			output = notifyCase(notify, fixture);
		} else {
			console.error(`upstream-runner: unknown mode ${mode}`);
			process.exit(2);
		}
		process.stdout.write(JSON.stringify({ name: fixture.name, output }) + "\n");
	}
}

await main();
