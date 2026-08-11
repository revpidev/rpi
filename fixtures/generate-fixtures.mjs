#!/usr/bin/env node
/**
 * Rpi fixtures generator (runbook step: see fixtures/README.md §2).
 *
 * Runs the pinned upstream Pi (recording pin: pi 0.84.1+, external/pi @
 * 4181f66 — see the pin note in fixtures/README.md; the current repo pin
 * lives in UPSTREAM.md) with the
 * faux provider and fixed prompt scripts over the SDK (`createAgentSession`),
 * then exports, per scenario:
 *   - `session.jsonl`  — the real on-disk session file (file-backed SessionManager)
 *   - `events.jsonl`   — the INTERNAL AgentSession event transcript (with
 *     cumulative `message`/`partial`; the delta-only `toJsonEvent` wire shape
 *     applies only at the print/json + RPC stdout boundary, T18)
 *
 * Prerequisites (one-time, see fixtures/README.md):
 *   cd external/pi && npm ci --ignore-scripts \
 *     && npm run build --workspace @earendil-works/pi-telemetry \
 *     && npm run build --workspace @earendil-works/pi-tui \
 *     && npm run build:offline --workspace @earendil-works/pi-ai \
 *     && npm run build --workspace @earendil-works/pi-protocol \
 *     && npm run build --workspace @earendil-works/pi-client \
 *     && npm run build --workspace @earendil-works/pi-agent-core \
 *     && npm run build --workspace @earendil-works/pi-coding-agent
 *
 * Usage:  node fixtures/generate-fixtures.mjs [scenario ...]
 * Default: regenerate all scenarios. Deterministic: fixed scripts, fixed faux
 * ids, temp dirs; volatile fields (timestamps/ids/paths) are stripped by the
 * rpi-test-support normalizer at diff time, not here.
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
// fixtures in the same commit)
// ---------------------------------------------------------------------------

const SCENARIOS = {
	/** Single turn: one user prompt, one text response. */
	"single-turn": {
		responses: () => [fauxAssistantMessage("Hello from the faux provider!")],
		async drive(session) {
			await session.prompt("Say hello.");
		},
	},

	/** read/bash tool calls: tool calls execute real tools in the temp cwd. */
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

	/** steering / follow-up: steer / followUp queue mid-stream and are delivered as subsequent turns in order. */
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

	/** Length truncation, whole-batch failure: response truncated by max_tokens (stopReason length). */
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

	/**
	 * Compaction threshold trigger: contextWindow 8192 / reserve 4096 → threshold 4096 tokens.
	 * Each of the two large replies triggers one auto-compaction: the first round is a
	 * split-turn with only a turn-prefix summary (empty history → "No prior history."),
	 * the second round runs the UPDATE prompt + turn-prefix summary with a previousSummary;
	 * the third round's small reply skips compaction silently because the cut point has
	 * no content to summarize (the stale guard also covers the pre-prompt check).
	 */
	"compaction-threshold": {
		options: { models: [{ id: "faux-1", contextWindow: 8192, maxTokens: 65536 }] },
		settings: { compaction: { reserveTokens: 4096, keepRecentTokens: 512 } },
		// Factory responses so each assistant timestamp is its own turn's
		// Date.now() — scripted-upfront timestamps would predate the first
		// compaction and trip the stale-usage guard (agent-session.ts:1974).
		responses: () => [
			() => fauxAssistantMessage(`ALPHA ${"alpha evidence block. ".repeat(560)}`),
			() => fauxAssistantMessage("Turn prefix summary: the user asked about the alpha topic."),
			() => fauxAssistantMessage(`BETA ${"beta evidence block. ".repeat(560)}`),
			() => fauxAssistantMessage("Updated history summary: alpha and beta evidence discussed."),
			() => fauxAssistantMessage("Turn prefix summary: the user then asked about beta."),
			() => fauxAssistantMessage("A short final answer."),
		],
		async drive(session, { waitForEvent }) {
			await session.prompt("First question about the alpha topic.");
			await waitForEvent("compaction_end", 1);
			await session.prompt("Second question about the beta topic.");
			await waitForEvent("compaction_end", 2);
			await session.prompt("A small follow-up question.");
		},
	},

	/**
	 * Compaction overflow recovery: window 16384 / reserve 8192 (threshold far from
	 * normal usage). The third round receives a "prompt is too long" error → overflow
	 * recovery compaction (split-turn: initial history summary + turn-prefix summary,
	 * two LLM calls) → willRetry → agent.continue automatically retries the round
	 * and succeeds.
	 */
	"compaction-overflow": {
		options: { models: [{ id: "faux-1", contextWindow: 16384, maxTokens: 65536 }] },
		settings: { compaction: { reserveTokens: 8192, keepRecentTokens: 256 } },
		responses: () => [
			() => fauxAssistantMessage(`FIRST ${"first answer block. ".repeat(80)}`),
			() => fauxAssistantMessage(`SECOND ${"second answer block. ".repeat(80)}`),
			() =>
				fauxAssistantMessage("", {
					stopReason: "error",
					errorMessage: "prompt is too long: 200000 tokens > 16384 maximum",
				}),
			() => fauxAssistantMessage("History summary: two answers were given before the overflow."),
			() => fauxAssistantMessage("Turn prefix summary: the user asked the overflowing question."),
			() => fauxAssistantMessage("Recovered answer after compaction and retry."),
		],
		async drive(session, { waitForEvent }) {
			await session.prompt("Question one.");
			await session.prompt("Question two.");
			await session.prompt("The question that overflows the context window.");
			await waitForEvent("compaction_end", 1);
		},
	},
};

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

async function waitForEventFactory(events) {
	// `occurrence` waits for the Nth event of a type (1-based) so scenarios
	// with repeated events (two compactions) don't resolve on the first one.
	return (type, occurrence = 1, timeoutMs = 15000) =>
		new Promise((resolve, reject) => {
			const started = Date.now();
			const timer = setInterval(() => {
				if (events.filter((e) => e.type === type).length >= occurrence) {
					clearInterval(timer);
					resolve();
				} else if (Date.now() - started > timeoutMs) {
					clearInterval(timer);
					reject(new Error(`timed out waiting for event "${type}" #${occurrence}`));
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
	const root = mkdtempSync(join(tmpdir(), `rpi-fixture-${name}-`));
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

		const settingsManager = SettingsManager.inMemory(scenario.settings ?? {});
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
