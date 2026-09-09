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

async function main() {
	const group = process.argv[2];
	const fixturePath = process.argv[3];
	if (!group || !fixturePath) {
		console.error("usage: upstream-runner.mjs <schema|normalize|validate|envelope|row-intent> <fixture.json>");
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
		} else {
			throw new Error(`unknown group: ${group}`);
		}
		process.stdout.write(`${JSON.stringify({ name: fixture.name, output })}\n`);
	}
}

await main();
