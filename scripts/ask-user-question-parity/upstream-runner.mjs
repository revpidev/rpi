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
// TE30: the dialog state machine + key router (the `@earendil-works/pi-tui`
// import in key-router.ts resolves to the harness stub package, a verbatim
// copy of `external/pi/packages/tui/src/keys.ts` @ 9841914c).
const stateReducer = await import(`${SNAPSHOT}/state/state-reducer.ts`);
const keyRouter = await import(`${SNAPSHOT}/state/key-router.ts`);
const i18nBridge = await import(`${SNAPSHOT}/state/i18n-bridge.ts`);
// TE31: the preview layout decider + bordered-box renderer (pure functions;
// resolved from the snapshot like every other driven module).
const previewDecider = await import(`${SNAPSHOT}/view/components/preview/preview-layout-decider.ts`);
const previewBox = await import(`${SNAPSHOT}/view/components/preview/preview-box-renderer.ts`);
// Stubbed `@earendil-works/pi-tui`: the harness materializes a package whose
// entry is a verbatim copy of `external/pi/packages/tui/src/keys.ts` @
// 9841914c. The snapshot's own bare import resolves it from the deps
// node_modules; this leg loads the same file by path (the runner lives
// outside the deps tree, so bare resolution would not find it).
const DEPS = SNAPSHOT.replace(/\/snapshot\/?$/, "");
const piTui = await import(`${DEPS}/node_modules/@earendil-works/pi-tui/index.ts`);

/** Upstream `buildItemsForQuestion` (ask-user-question.ts:289-297). */
function buildItemsForQuestion(question) {
	const items = question.options.map((o) => ({ kind: "option", label: o.label, description: o.description }));
	for (const kind of rowIntent.sentinelsToAppend(question)) {
		items.push({ kind, label: i18nBridge.displayLabel(kind) });
	}
	return items;
}

function fixtureItems(input) {
	if (input.itemsByTab) return input.itemsByTab;
	return input.questions.map((question) => buildItemsForQuestion(question));
}

/** Canonical questionnaire state from a partial fixture snapshot. */
function stateFromJson(setup = {}) {
	const mapFrom = (object) =>
		new Map(Object.entries(object ?? {}).map(([key, value]) => [Number(key), value]));
	return {
		currentTab: setup.currentTab ?? 0,
		optionIndex: setup.optionIndex ?? 0,
		inputMode: setup.inputMode ?? false,
		notesVisible: setup.notesVisible ?? false,
		answers: mapFrom(setup.answers),
		multiSelectChecked: new Set(setup.multiSelectChecked ?? []),
		customDraftsByTab: mapFrom(setup.customDraftsByTab),
		notesByTab: mapFrom(setup.notesByTab),
		submitChoiceIndex: setup.submitChoiceIndex ?? 0,
		notesDraft: setup.notesDraft ?? "",
		collapsed: setup.collapsed ?? false,
	};
}

/** Canonical snapshot (identical shape to the Rust `snapshot()`). */
function snapshotState(state) {
	const objectFrom = (map) =>
		Object.fromEntries([...map.entries()].sort((a, b) => a[0] - b[0]));
	return {
		currentTab: state.currentTab,
		optionIndex: state.optionIndex,
		inputMode: state.inputMode,
		notesVisible: state.notesVisible,
		answers: objectFrom(state.answers),
		multiSelectChecked: [...state.multiSelectChecked].sort((a, b) => a - b),
		customDraftsByTab: objectFrom(state.customDraftsByTab),
		notesByTab: objectFrom(state.notesByTab),
		submitChoiceIndex: state.submitChoiceIndex,
		notesDraft: state.notesDraft,
		collapsed: state.collapsed,
	};
}

/** TE30 `state` group: action sequence -> per-step snapshot + effects. */
function stateCase(input) {
	const questions = input.questions;
	const itemsByTab = fixtureItems(input);
	let state = stateFromJson(input.setup);
	const ctx = { questions, itemsByTab };
	const steps = [];
	for (const action of input.actions ?? []) {
		const result = stateReducer.reduce(state, action, ctx);
		steps.push({ state: snapshotState(result.state), effects: jsonClone(result.effects) });
		state = result.state;
	}
	return { steps };
}

/** Upstream keybinding defaults for the names the questionnaire reads
 * (`packages/tui/src/keybindings.ts` + `packages/coding-agent/src/core/keybindings.ts`). */
const DEFAULT_BINDINGS = {
	"tui.select.up": ["up"],
	"tui.select.down": ["down"],
	"tui.select.confirm": ["enter"],
	"tui.input.submit": ["enter"],
	"tui.select.cancel": ["escape", "ctrl+c"],
	"tui.input.newLine": ["shift+enter", "ctrl+j"],
	"tui.editor.cursorUp": ["up"],
	"tui.editor.cursorDown": ["down"],
	"tui.editor.deleteToLineStart": ["ctrl+u"],
	"app.editor.external": ["ctrl+g"],
};

/** TE30 `keys` group: raw key bytes -> routed action. */
function keysCase(input, keyMatrix) {
	const questions = input.questions;
	const itemsByTab = fixtureItems(input);
	const state = stateFromJson(input.setup);
	const runtimeInput = input.runtime ?? {};
	const bindings = { ...DEFAULT_BINDINGS, ...(runtimeInput.bindings ?? {}) };
	const keybindings = {
		matches: (data, name) => (bindings[name] ?? []).some((key) => piTui.matchesKey(data, key)),
	};
	const items = itemsByTab[state.currentTab] ?? [];
	const runtime = {
		keybindings,
		inputBuffer: runtimeInput.inputBuffer ?? "",
		canMoveInputUp: runtimeInput.canMoveInputUp ?? false,
		canMoveInputDown: runtimeInput.canMoveInputDown ?? false,
		questions,
		isMulti: questions.length > 1,
		currentItem: items[state.optionIndex],
		items,
		collapseKey: runtimeInput.collapseKey ?? "ctrl+]",
	};
	const keys = input.keys ?? keyMatrix ?? [];
	return { actions: keys.map((data) => jsonClone(keyRouter.routeKey(data, state, runtime))) };
}


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

/** TE31 `preview` group: the pure preview layout/box functions. */
function previewCase(input) {
	switch (input.fn) {
		case "decideLayout":
			return { mode: previewDecider.decideLayout(input.terminalWidth, input.paneWidth) };
		case "adaptiveLeftWidth":
			return {
				left: previewDecider.adaptiveLeftWidth(
					input.items,
					input.totalForNumbering,
					input.paneWidth,
				),
			};
		case "crossTabMaxLeftWidth":
			return {
				left: previewDecider.crossTabMaxLeftWidth(
					input.tabs,
					input.itemsByTab,
					input.paneWidth,
				),
			};
		case "previewSourceWidth":
			return { width: previewDecider.previewSourceWidth(input.question) };
		case "crossTabPreviewBudget":
			return { budget: previewDecider.crossTabPreviewBudget(input.questions, input.paneWidth) };
		case "crossTabLeftWidthWithDonation":
			return {
				left: previewDecider.crossTabLeftWidthWithDonation(
					input.tabs,
					input.itemsByTab,
					input.questions,
					input.paneWidth,
				),
			};
		case "columnWidths":
			return jsonClone(previewDecider.columnWidths(input.paneWidth, input.adaptiveLeft));
		case "bodyWidths":
			return jsonClone(
				previewDecider.bodyWidths(input.paneWidth, input.mode, input.adaptiveLeft),
			);
		case "constants":
			return {
				PREVIEW_MIN_WIDTH: previewDecider.PREVIEW_MIN_WIDTH,
				PREVIEW_COLUMN_GAP: previewDecider.PREVIEW_COLUMN_GAP,
				PREVIEW_PADDING_LEFT: previewDecider.PREVIEW_PADDING_LEFT,
				STACKED_GAP_ROWS: previewDecider.STACKED_GAP_ROWS,
				MIN_LEFT: previewDecider.MIN_LEFT,
				MAX_LEFT_RATIO: previewDecider.MAX_LEFT_RATIO,
				MIN_PREVIEW_WIDTH: previewDecider.MIN_PREVIEW_WIDTH,
				CONFIRMED_OVERHEAD: previewDecider.CONFIRMED_OVERHEAD,
				BORDER_VERTICAL_OVERHEAD: previewBox.BORDER_VERTICAL_OVERHEAD,
				BORDER_HORIZONTAL_OVERHEAD: previewBox.BORDER_HORIZONTAL_OVERHEAD,
				BORDER_INNER_PADDING_HORIZONTAL: previewBox.BORDER_INNER_PADDING_HORIZONTAL,
				BOX_MIN_CONTENT_WIDTH: previewBox.BOX_MIN_CONTENT_WIDTH,
			};
		case "stripFenceMarkers":
			return { lines: previewBox.stripFenceMarkers(input.lines) };
		case "renderBorderedBox":
			return {
				lines: previewBox.renderBorderedBox(
					input.lines,
					input.width,
					(s) => s,
					input.hidden ?? 0,
				),
			};
		case "computeBoxDimensions":
			return jsonClone(previewBox.computeBoxDimensions(input.lines, input.maxInnerWidth));
		default:
			throw new Error(`unknown preview fn: ${input.fn}`);
	}
}

async function main() {
	const group = process.argv[2];
	const fixturePath = process.argv[3];
	if (!group || !fixturePath) {
		console.error(
		"usage: upstream-runner.mjs <schema|normalize|validate|envelope|row-intent|rpc|state|keys> <fixture.json>",
	);
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
		} else if (group === "state") {
			output = stateCase(fixture.input);
		} else if (group === "keys") {
			output = keysCase(fixture.input, fixtures.keys?.keyMatrix ?? []);
		} else if (group === "preview") {
			output = previewCase(fixture.input);
		} else {
			throw new Error(`unknown group: ${group}`);
		}
		process.stdout.write(`${JSON.stringify({ name: fixture.name, output })}\n`);
	}
}

await main();
