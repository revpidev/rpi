#!/usr/bin/env node
/**
 * Pir fixtures generator (runbook step: design doc §10.2).
 *
 * Runs the pinned upstream Pi (@ pi 0.82.1, external/pi @ 2efa728) with the
 * faux provider and fixed prompt scripts over the SDK (`createAgentSession`),
 * then exports, per scenario:
 *   - `session.jsonl`  — the real on-disk session file (file-backed SessionManager)
 *   - `events.jsonl`   — the AgentSession event transcript (json-mode shapes)
 *
 * Prerequisites (one-time, see fixtures/README.md):
 *   cd external/pi && npm ci && npm run build --workspace @earendil-works/pi-tui \
 *     && npm run build --workspace @earendil-works/pi-ai \
 *     && npm run build --workspace @earendil-works/pi-agent-core \
 *     && npm run build --workspace @earendil-works/pi-coding-agent
 *
 * Usage:  node fixtures/generate-fixtures.mjs [scenario ...]
 * Default: regenerate all scenarios. Deterministic: fixed scripts, fixed faux
 * ids, temp dirs; volatile fields (timestamps/ids/paths) are stripped by the
 * pir-test-support normalizer at diff time, not here.
 */

import { copyFileSync, existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import {
	fauxAssistantMessage,
	fauxProvider,
	fauxText,
	fauxThinking,
	fauxToolCall,
} from "../external/pi/packages/ai/dist/index.js";
import {
	createAgentSession,
	DefaultResourceLoader,
	ModelRuntime,
	SessionManager,
	SettingsManager,
} from "../external/pi/packages/coding-agent/dist/index.js";

const here = dirname(fileURLToPath(import.meta.url));
const outDir = join(here, "generated");

const FIXED_TOOL_ID = { id: "fixture-tool-call-1" };
const FIXED_TOOL_ID_2 = { id: "fixture-tool-call-2" };

// ---------------------------------------------------------------------------
// Scenario scripts (fixed — changes require regenerating + re-reviewing all
// fixtures in the same commit, coding-standards §12.3)
// ---------------------------------------------------------------------------

const SCENARIOS = {
	/** 单轮问答：one user prompt, one text response. */
	"single-turn": {
		responses: () => [fauxAssistantMessage("Hello from the faux provider!")],
		async drive(session) {
			await session.prompt("Say hello.");
		},
	},

	/** read/bash 工具调用：tool calls execute real tools in the temp cwd. */
	"tool-calls": {
		setup(workspace) {
			writeFileSync(join(workspace, "note.txt"), "fixture note content\n");
		},
		responses: () => [
			fauxAssistantMessage(
				[
					fauxText("Let me read the note and run a command."),
					fauxToolCall("read", { path: "note.txt" }, FIXED_TOOL_ID),
					fauxToolCall("bash", { command: "echo fixture-bash-output" }, FIXED_TOOL_ID_2),
				],
				{ stopReason: "toolUse" },
			),
			fauxAssistantMessage("I read the note and ran the command."),
		],
		async drive(session) {
			await session.prompt("Read note.txt and run the echo command.");
		},
	},

	/** steering / follow-up：流式进行中 steer / followUp 排队，作为后续 turn 依次投递。 */
	"steering-followup": {
		// Slow pacing + long texts so steer/followUp are queued while streaming.
		// Captured upstream semantics: with no tool calls pending, both are
		// delivered as subsequent turns (no mid-stream abort); the queue holds
		// 4 responses (initial, post-steer, second answer, follow-up answer).
		options: { tokensPerSecond: 50 },
		responses: () => [
			fauxAssistantMessage(
				"This is a long first answer that keeps streaming so a steering message can interrupt it mid-turn. ".repeat(
					8,
				),
			),
			fauxAssistantMessage("Answer after the steering interruption."),
			fauxAssistantMessage(
				"Second answer, also long enough that a follow-up message can be queued while it streams. ".repeat(
					8,
				),
			),
			fauxAssistantMessage("Final answer after steering and follow-up."),
		],
		async drive(session, { waitForEvent }) {
			const first = session.prompt("Start the first answer.");
			await waitForEvent("message_update");
			await session.steer("Change of plans: answer briefly.");
			await first;
			const second = session.prompt("Now the second answer.");
			await waitForEvent("message_update");
			await session.followUp("And one more thing.");
			await second;
		},
	},

	/** abort：user aborts mid-stream. */
	abort: {
		options: { tokensPerSecond: 50 },
		responses: () => [
			fauxAssistantMessage(
				"A long answer that the user will abort before it finishes streaming. ".repeat(8),
			),
		],
		async drive(session, { waitForEvent }) {
			const pending = session.prompt("Give me a long answer.");
			await waitForEvent("message_update");
			await session.abort();
			await pending;
		},
	},

	/** length 截断整批失败：response truncated by max_tokens (stopReason length). */
	"length-truncation": {
		responses: () => [
			fauxAssistantMessage("Truncated answer that hit the max token limit", {
				stopReason: "length",
			}),
		],
		async drive(session) {
			await session.prompt("Answer until you run out of tokens.");
		},
	},
};

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

async function waitForEventFactory(events) {
	return (type, timeoutMs = 15000) =>
		new Promise((resolve, reject) => {
			const started = Date.now();
			const timer = setInterval(() => {
				if (events.some((e) => e.type === type)) {
					clearInterval(timer);
					resolve();
				} else if (Date.now() - started > timeoutMs) {
					clearInterval(timer);
					reject(new Error(`timed out waiting for event "${type}"`));
				}
			}, 5);
		});
}

/** JSON.stringify replacer that keeps Error objects inspectable. */
function eventReplacer(_key, value) {
	if (value instanceof Error) {
		return { name: value.name, message: value.message };
	}
	return value;
}

async function runScenario(name, scenario) {
	const root = mkdtempSync(join(tmpdir(), `pir-fixture-${name}-`));
	const workspace = join(root, "workspace");
	const agentDir = join(root, "agent");
	mkdirSync(workspace, { recursive: true });
	mkdirSync(agentDir, { recursive: true });
	try {
		scenario.setup?.(workspace);

		const faux = fauxProvider({ api: "faux", ...(scenario.options ?? {}) });
		const modelRuntime = await ModelRuntime.create({
			authPath: join(agentDir, "auth.json"),
			modelsPath: null,
			allowModelNetwork: false,
		});
		modelRuntime.registerNativeProvider(faux.provider);
		const model = modelRuntime.getModel(faux.provider.id ?? "faux", "faux-1");
		if (!model) throw new Error(`faux model not registered for scenario ${name}`);

		const settingsManager = SettingsManager.inMemory();
		const resourceLoader = new DefaultResourceLoader({
			cwd: workspace,
			agentDir,
			settingsManager,
			noExtensions: true,
			noSkills: true,
			noPromptTemplates: true,
			noThemes: true,
			noContextFiles: true,
		});
		await resourceLoader.reload();

		const sessionManager = SessionManager.create(workspace, join(agentDir, "sessions"));
		const { session } = await createAgentSession({
			cwd: workspace,
			agentDir,
			model,
			modelRuntime,
			thinkingLevel: "off",
			resourceLoader,
			settingsManager,
			sessionManager,
		});

		const events = [];
		session.subscribe((event) => events.push(event));
		const waitForEvent = await waitForEventFactory(events);

		faux.setResponses(scenario.responses());
		await scenario.drive(session, { waitForEvent });
		// Give trailing events (agent_end etc.) a microtask turn.
		await new Promise((resolve) => setTimeout(resolve, 50));

		const target = join(outDir, name);
		mkdirSync(target, { recursive: true });
		const sessionFile = sessionManager.getSessionFile();
		if (!sessionFile || !existsSync(sessionFile)) {
			throw new Error(`scenario ${name}: no session file produced`);
		}
		copyFileSync(sessionFile, join(target, "session.jsonl"));
		const lines = events.map((e) => JSON.stringify(e, eventReplacer));
		writeFileSync(join(target, "events.jsonl"), lines.join("\n") + "\n");

		session.dispose();
		console.log(
			`[${name}] ok — session ${lines.length ? "" : "(no events!)"}events=${events.length}, sessionFile=${sessionFile}`,
		);
	} finally {
		rmSync(root, { recursive: true, force: true });
	}
}

const selected = process.argv.slice(2);
const names = selected.length > 0 ? selected : Object.keys(SCENARIOS);
for (const name of names) {
	const scenario = SCENARIOS[name];
	if (!scenario) {
		console.error(`unknown scenario: ${name} (have: ${Object.keys(SCENARIOS).join(", ")})`);
		process.exit(1);
	}
	await runScenario(name, scenario);
}
console.log(`fixtures written to ${outDir}`);
