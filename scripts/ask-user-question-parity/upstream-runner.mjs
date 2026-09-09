// Upstream leg of the ask-user-question parity harness (TE28 G3/G12).
//
// Executes the pinned rpiv-mono pure-function modules (tsx, no build step)
// against the shared fixtures and prints one normalized JSON line per case.
// The modules are imported from a fresh snapshot of the pinned submodule
// (PARITY_SNAPSHOT, materialized by run-parity.mjs) — external/ stays
// read-only; the snapshot carries sha256 provenance in the report.
import { readFileSync } from "node:fs";

const SNAPSHOT =
	process.env.PARITY_SNAPSHOT ??
	"/tmp/rpi-ask-user-question-parity-deps/snapshot";

const types = await import(`${SNAPSHOT}/tool/types.ts`);
const normalize = await import(`${SNAPSHOT}/tool/normalize-params.ts`);
const validate = await import(`${SNAPSHOT}/tool/validate-questionnaire.ts`);
const envelope = await import(`${SNAPSHOT}/tool/response-envelope.ts`);
const rowIntent = await import(`${SNAPSHOT}/state/row-intent.ts`);
const rpcFallback = await import(`${SNAPSHOT}/rpc-fallback.ts`);

function jsonClone(value) {
	return JSON.parse(JSON.stringify(value));
}

function schemaCase() {
	return {
		schema: jsonClone(types.QuestionParamsSchema),
		constants: {
			MAX_QUESTIONS: types.MAX_QUESTIONS,
			MIN_OPTIONS: types.MIN_OPTIONS,
			MAX_OPTIONS: types.MAX_OPTIONS,
			MAX_HEADER_LENGTH: types.MAX_HEADER_LENGTH,
			MAX_LABEL_LENGTH: types.MAX_LABEL_LENGTH,
		},
		reservedLabels: [...types.RESERVED_LABELS],
		sentinelLabels: jsonClone(types.SENTINEL_LABELS),
	};
}

function rowIntentQuestion(input) {
	const question = {
		question: "Q?",
		header: "H",
		options: [
			{ label: "A", description: "a" },
			{ label: "B", description: "b" },
		],
	};
	if (input && input.multiSelect !== undefined) {
		question.multiSelect = input.multiSelect;
	}
	return question;
}

function rowIntentCase(name, input) {
	if (name === "constants") {
		return {
			reservedLabels: [...rowIntent.RESERVED_LABEL_SET],
			labelsByKind: jsonClone(rowIntent.LABELS_BY_KIND),
			meta: jsonClone(rowIntent.ROW_INTENT_META),
			sentinelKinds: [...rowIntent.SENTINEL_KINDS],
		};
	}
	return { sentinelsToAppend: [...rowIntent.sentinelsToAppend(rowIntentQuestion(input))] };
}

/** TE29: mock `DialogUI` — records every call, pops one scripted reply per
 * call (`{cancel: true}` or exhausted script = dismissed → undefined). The
 * upstream i18n bridge is the identity fallback (the rpiv-i18n SDK is not a
 * harness dep), so titles/labels resolve to the canonical English literals —
 * the Rust leg injects the same `en` table. */
function scriptedUi(script) {
	const calls = [];
	const queue = [...(script ?? [])];
	const next = () => (queue.length > 0 ? queue.shift() : { cancel: true });
	return {
		ui: {
			select: async (title, options) => {
				calls.push({ method: "select", title, options: [...options] });
				const entry = next();
				return entry.cancel === true ? undefined : entry.reply;
			},
			input: async (title, placeholder) => {
				calls.push({ method: "input", title, placeholder: placeholder ?? null });
				const entry = next();
				return entry.cancel === true ? undefined : entry.reply;
			},
		},
		calls,
	};
}

async function rpcCase(input) {
	if (input.probe !== undefined) {
		// hasDialogUI judgment table: {select, input} flags, null = undefined ui.
		if (input.probe === null) {
			return { hasDialogUI: rpcFallback.hasDialogUI(undefined) };
		}
		const ui = {};
		if (input.probe.select) ui.select = async () => undefined;
		if (input.probe.input) ui.input = async () => "";
		return { hasDialogUI: rpcFallback.hasDialogUI(ui) };
	}
	const { ui, calls } = scriptedUi(input.script);
	const result = jsonClone(await rpcFallback.runRpcQuestionnaire(ui, input.params));
	return { calls, result };
}

async function main() {
	const group = process.argv[2];
	const fixturePath = process.argv[3];
	if (!group || !fixturePath) {
		console.error("usage: upstream-runner.mjs <schema|normalize|validate|envelope|row-intent|rpc> <fixture.json>");
		process.exit(2);
	}
	const fixtures = JSON.parse(readFileSync(fixturePath, "utf-8"));
	const cases = fixtures[group]?.cases ?? [];
	for (const fixture of cases) {
		let output;
		if (group === "schema") {
			output = schemaCase();
		} else if (group === "normalize") {
			output = normalize.normalizeQuestionParams(fixture.input);
		} else if (group === "validate") {
			output = validate.validateQuestionnaire(fixture.input);
		} else if (group === "envelope") {
			output = envelope.buildQuestionnaireResponse(fixture.input.result, fixture.input.params);
		} else if (group === "row-intent") {
			output = rowIntentCase(fixture.name, fixture.input);
		} else if (group === "rpc") {
			output = await rpcCase(fixture.input);
		} else {
			throw new Error(`unknown group: ${group}`);
		}
		process.stdout.write(`${JSON.stringify({ name: fixture.name, output })}\n`);
	}
}

await main();
