// Upstream leg of the subagents parity harness (TE04 G3; dual-track TE13).
//
// Track `regression` (default): executes the pinned v0.48 modules directly
// (tsx, no build step) from `external/pi-subagents` — the pre-TE13 behavior.
//
// Track `target` (pi-subagents v0.66.0, ADR-0025):
//   - argv/env: the frozen v0.48 golden (upstream deleted `pi-args.ts` /
//     `buildPiArgs` in v0.65, so this surface is [RPI-OWN], ADR-0025 §4);
//   - frontmatter / final-output / fallback: the v0.66 snapshot extracted by
//     `setup-target-source.sh` (never a checkout of `external/`).
//
// Prints normalized JSON lines that the orchestrator diffs against the Rust
// parity_runner example.
//
// Run via run-parity.mjs — never run inside external/ (the submodule stays
// read-only; nothing here writes to it).
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const TRACK = process.env.RPI_SUBAGENTS_PARITY_TRACK ?? "regression";
const REGRESSION_ROOT = resolve(HERE, "../../external/pi-subagents");
const TARGET_ROOT =
	process.env.RPI_SUBAGENTS_TARGET_SRC ?? "/tmp/rpi-subagents-parity-target-v066";
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
	"PI_CODING_AGENT_PACKAGE_ROOT",
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
	const root = TRACK === "target" ? TARGET_ROOT : REGRESSION_ROOT;
	const frontmatter = await import(moduleUrl(root, "src/agents/frontmatter.ts"));
	const utils = await import(moduleUrl(root, "src/shared/utils.ts"));
	const modelFallback =
		TRACK === "target"
			? await import(moduleUrl(root, "src/runs/shared/model-fallback.ts"))
			: undefined;
	// v0.48 only: argv/env source. Absent at v0.66 (ADR-0025 §4).
	const piArgs =
		TRACK === "target"
			? undefined
			: await import(moduleUrl(root, "src/runs/shared/pi-args.ts"));
	return { piArgs, frontmatter, utils, modelFallback };
}

function buildArgsCase(piArgs, input) {
	try {
		const result = piArgs.buildPiArgs({
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

function fallbackCase(modelFallback, fixture) {
	switch (fixture.kind) {
		case "retryable":
			return { retryable: modelFallback.isRetryableModelFailure(fixture.error) };
		case "context-overflow":
			return { contextOverflow: modelFallback.isContextOverflow(fixture.error) };
		case "attempt":
			return {
				attempt: modelFallback.isRetryableModelFailureAttempt({
					error: fixture.error,
					messages: fixture.messages,
					toolCount: fixture.toolCount,
				}),
			};
		default:
			throw new Error(`unknown fallback fixture kind: ${fixture.kind}`);
	}
}

function loadArgsGolden() {
	const golden = JSON.parse(readFileSync(ARGS_GOLDEN, "utf-8"));
	const byName = new Map();
	for (const entry of golden.cases ?? []) byName.set(entry.name, entry.output);
	return byName;
}

async function main() {
	const mode = process.argv[2];
	const fixturePath = process.argv[3];
	if (!mode || !fixturePath) {
		console.error(
			"usage: upstream-runner.mjs <args|frontmatter|final-output|fallback> <fixture.json>",
		);
		process.exit(2);
	}
	const { piArgs, frontmatter, utils, modelFallback } = await loadUpstream();
	const fixtures = JSON.parse(readFileSync(fixturePath, "utf-8"));
	const golden = mode === "args" && TRACK === "target" ? loadArgsGolden() : null;
	for (const fixture of fixtures.cases ?? []) {
		let output;
		if (mode === "args") {
			output = golden
				? golden.get(fixture.name)
				: buildArgsCase(piArgs, fixture.input ?? {});
			if (output === undefined) {
				throw new Error(`args golden missing case ${fixture.name}`);
			}
		} else if (mode === "frontmatter") {
			output = frontmatterCase(frontmatter, fixture.content ?? "");
		} else if (mode === "final-output") {
			output = utils.getFinalOutput(fixture.messages ?? []);
		} else if (mode === "fallback") {
			output = fallbackCase(modelFallback, fixture);
		} else {
			console.error(`upstream-runner: unknown mode ${mode}`);
			process.exit(2);
		}
		process.stdout.write(JSON.stringify({ name: fixture.name, output }) + "\n");
	}
}

await main();
