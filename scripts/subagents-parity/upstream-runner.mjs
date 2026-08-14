// Upstream leg of the subagents parity harness (TE04 G3).
//
// Executes the pinned pi-subagents modules directly (tsx, no build step)
// against the shared fixture files and prints normalized JSON lines that the
// orchestrator diffs against the Rust parity_runner example.
//
// Run from rpi/scripts/subagents-parity via run-parity.mjs — never run inside
// external/ (the submodule stays read-only; nothing here writes to it).
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const UPSTREAM_ROOT = new URL("../../external/pi-subagents/", import.meta.url).pathname;

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

async function loadUpstream() {
	const { pathToFileURL } = await import("node:url");
	const piArgsUrl = pathToFileURL(UPSTREAM_ROOT + "src/runs/shared/pi-args.ts").href;
	const frontmatterUrl = pathToFileURL(UPSTREAM_ROOT + "src/agents/frontmatter.ts").href;
	const utilsUrl = pathToFileURL(UPSTREAM_ROOT + "src/shared/utils.ts").href;
	const piArgs = await import(piArgsUrl);
	const frontmatter = await import(frontmatterUrl);
	const utils = await import(utilsUrl);
	return { piArgs, frontmatter, utils };
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

async function main() {
	const mode = process.argv[2];
	const fixturePath = process.argv[3];
	if (!mode || !fixturePath) {
		console.error("usage: upstream-runner.mjs <args|frontmatter|final-output> <fixture.json>");
		process.exit(2);
	}
	const { piArgs, frontmatter, utils } = await loadUpstream();
	const fixtures = JSON.parse(readFileSync(fixturePath, "utf-8"));
	for (const fixture of fixtures.cases ?? []) {
		let output;
		if (mode === "args") {
			output = buildArgsCase(piArgs, fixture.input ?? {});
		} else if (mode === "frontmatter") {
			output = frontmatterCase(frontmatter, fixture.content ?? "");
		} else {
			output = utils.getFinalOutput(fixture.messages ?? []);
		}
		process.stdout.write(JSON.stringify({ name: fixture.name, output }) + "\n");
	}
}

await main();
