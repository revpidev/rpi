#!/usr/bin/env node
/**
 * Compaction golden-values generator (T08).
 *
 * Runs the pinned upstream compaction implementation
 * (external/pi @ 2efa728, built dist) over a fixed battery of inputs and
 * dumps:
 *   - `generated/compaction/golden.json`  — numeric/structural golden cases
 *     (estimateTokens / calculateContextTokens / estimateContextTokens /
 *     findCutPoint / prepareCompaction / serializeConversation / file ops /
 *     prepareBranchEntries / isContextOverflow)
 *   - `generated/compaction/prompts/*.txt` — byte-exact prompt renders
 *     captured from the real upstream call sites (history initial / update,
 *     turn prefix, branch summary, system prompt, split-turn merged summary)
 *
 * The Rust side (crates/rpi-agent/tests/compaction_golden_test.rs) replays
 * every case and asserts byte/value equality — do not edit the outputs by
 * hand; regenerate with:
 *
 *   node fixtures/generate-compaction-golden.mjs
 *
 * Prerequisites: upstream dist built (see fixtures/README.md §2).
 */

import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import {
	calculateContextTokens,
	compact,
	estimateContextTokens,
	estimateTokens,
	findCutPoint,
	generateSummaryWithUsage,
	prepareCompaction,
	DEFAULT_COMPACTION_SETTINGS,
} from "../external/pi/packages/coding-agent/dist/core/compaction/compaction.js";
import {
	computeFileLists,
	createFileOps,
	formatFileOperations,
	serializeConversation,
} from "../external/pi/packages/coding-agent/dist/core/compaction/utils.js";
import {
	generateBranchSummary,
	prepareBranchEntries,
} from "../external/pi/packages/coding-agent/dist/core/compaction/branch-summarization.js";
import { isContextOverflow } from "../external/pi/packages/ai/dist/utils/overflow.js";

const here = dirname(fileURLToPath(import.meta.url));
const outDir = join(here, "generated", "compaction");
const promptsDir = join(outDir, "prompts");
mkdirSync(promptsDir, { recursive: true });

// ---------------------------------------------------------------------------
// Builders (fixed shapes; ids/timestamps are inert for the pure functions)
// ---------------------------------------------------------------------------

let entrySeq = 0;
function entryId() {
	entrySeq += 1;
	return `e${String(entrySeq).padStart(4, "0")}`;
}

function userEntry(text, extra = {}) {
	return {
		type: "message",
		id: entryId(),
		parentId: null,
		timestamp: "2026-08-01T00:00:00.000Z",
		message: { role: "user", content: text, timestamp: 1, ...extra },
	};
}

function assistantEntry(blocks, usage, extra = {}) {
	return {
		type: "message",
		id: entryId(),
		parentId: null,
		timestamp: "2026-08-01T00:00:01.000Z",
		message: {
			role: "assistant",
			content: blocks,
			api: "faux",
			provider: "faux",
			model: "faux-1",
			usage: usage ?? {
				input: 0,
				output: 0,
				cacheRead: 0,
				cacheWrite: 0,
				totalTokens: 0,
				cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
			},
			stopReason: "stop",
			timestamp: 2,
			...extra,
		},
	};
}

function toolResultEntry(text, extra = {}) {
	return {
		type: "message",
		id: entryId(),
		parentId: null,
		timestamp: "2026-08-01T00:00:02.000Z",
		message: {
			role: "toolResult",
			toolCallId: "call-1",
			toolName: "read",
			content: [{ type: "text", text }],
			isError: false,
			timestamp: 3,
			...extra,
		},
	};
}

function labelEntry(targetId) {
	return {
		type: "label",
		id: entryId(),
		parentId: null,
		timestamp: "2026-08-01T00:00:03.000Z",
		targetId,
		label: "mark",
	};
}

function compactionEntry(summary, firstKeptEntryId, tokensBefore, details) {
	return {
		type: "compaction",
		id: entryId(),
		parentId: null,
		timestamp: "2026-08-01T00:00:04.000Z",
		summary,
		firstKeptEntryId,
		tokensBefore,
		...(details ? { details } : {}),
	};
}

/** Chain parentId along the array (root-first), as a real session path is. */
function chain(entries) {
	let prev = null;
	for (const e of entries) {
		e.parentId = prev;
		prev = e.id;
	}
	return entries;
}

const text = (t) => ({ type: "text", text: t });
const thinking = (t) => ({ type: "thinking", thinking: t });
const toolCall = (name, args, id = "call-1") => ({ type: "toolCall", id, name, arguments: args });
const image = () => ({ type: "image", data: "QUJD", mimeType: "image/png" });

// ---------------------------------------------------------------------------
// 1. estimateTokens cases
// ---------------------------------------------------------------------------

const estimateTokensCases = [
	{
		name: "user_text",
		message: { role: "user", content: "hello world", timestamp: 1 },
	},
	{
		name: "user_blocks_text_and_image",
		message: { role: "user", content: [text("abcd"), image(), text("ef")], timestamp: 1 },
	},
	{
		name: "user_empty_string",
		message: { role: "user", content: "", timestamp: 1 },
	},
	{
		name: "assistant_text_thinking_toolcalls",
		message: {
			role: "assistant",
			content: [
				thinking("hmm hmm"),
				text("answer text"),
				toolCall("read", { path: "src/main.rs" }),
				toolCall("bash", { command: "ls -la", timeout: 1000 }, "call-2"),
			],
			api: "faux",
			provider: "faux",
			model: "faux-1",
			usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
			stopReason: "stop",
			timestamp: 2,
		},
	},
	{
		name: "assistant_toolcall_nested_args",
		message: {
			role: "assistant",
			content: [toolCall("edit", { path: "a.ts", edits: [{ old: "x", new: "y" }], flags: ["g", "i"] })],
			api: "faux",
			provider: "faux",
			model: "faux-1",
			usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
			stopReason: "stop",
			timestamp: 2,
		},
	},
	{
		name: "toolResult_text",
		message: {
			role: "toolResult",
			toolCallId: "call-1",
			toolName: "read",
			content: [text("file body here")],
			isError: false,
			timestamp: 3,
		},
	},
	{
		name: "toolResult_image",
		message: {
			role: "toolResult",
			toolCallId: "call-1",
			toolName: "read",
			content: [text("shot"), image()],
			isError: false,
			timestamp: 3,
		},
	},
	{
		name: "bashExecution",
		message: {
			role: "bashExecution",
			command: "git status",
			output: "On branch main\nnothing to commit",
			cancelled: false,
			truncated: false,
			timestamp: 4,
		},
	},
	{
		name: "custom",
		message: {
			role: "custom",
			customType: "note",
			content: "custom content text",
			display: false,
			timestamp: 5,
		},
	},
	{
		name: "branchSummary",
		message: { role: "branchSummary", summary: "branch summary body", fromId: "e0001", timestamp: 6 },
	},
	{
		name: "compactionSummary",
		message: { role: "compactionSummary", summary: "compacted history body", tokensBefore: 12345, timestamp: 7 },
	},
	{
		name: "user_unicode_bmp",
		message: { role: "user", content: "中文测试字符串", timestamp: 1 },
	},
];

// ---------------------------------------------------------------------------
// 2. calculateContextTokens cases
// ---------------------------------------------------------------------------

const usageOf = (input, output, cacheRead, cacheWrite, totalTokens) => ({
	input,
	output,
	cacheRead,
	cacheWrite,
	totalTokens,
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
});

const calculateContextTokensCases = [
	{ name: "total_tokens_wins", usage: usageOf(10, 5, 3, 2, 999) },
	{ name: "component_sum_when_total_zero", usage: usageOf(10, 5, 3, 2, 0) },
	{ name: "all_zero", usage: usageOf(0, 0, 0, 0, 0) },
];

// ---------------------------------------------------------------------------
// 3. estimateContextTokens cases
// ---------------------------------------------------------------------------

const estimateContextTokensCases = [
	{
		name: "no_usage_anchor",
		messages: [
			{ role: "user", content: "aaaaaaaa", timestamp: 1 },
			{ role: "user", content: "bbbb", timestamp: 2 },
		],
	},
	{
		name: "usage_anchor_with_trailing",
		messages: [
			{ role: "user", content: "start", timestamp: 1 },
			{
				role: "assistant",
				content: [text("reply")],
				api: "faux",
				provider: "faux",
				model: "faux-1",
				usage: usageOf(100, 20, 5, 10, 135),
				stopReason: "stop",
				timestamp: 2,
			},
			{ role: "user", content: "aaaaaaaa", timestamp: 3 },
		],
	},
	{
		name: "skips_aborted_error_and_zero_usage",
		messages: [
			{
				role: "assistant",
				content: [text("old")],
				api: "faux",
				provider: "faux",
				model: "faux-1",
				usage: usageOf(50, 10, 0, 0, 60),
				stopReason: "stop",
				timestamp: 1,
			},
			{
				role: "assistant",
				content: [text("aborted")],
				api: "faux",
				provider: "faux",
				model: "faux-1",
				usage: usageOf(999, 999, 0, 0, 1998),
				stopReason: "aborted",
				timestamp: 2,
			},
			{
				role: "assistant",
				content: [text("err")],
				api: "faux",
				provider: "faux",
				model: "faux-1",
				usage: usageOf(888, 1, 0, 0, 889),
				stopReason: "error",
				errorMessage: "boom",
				timestamp: 3,
			},
			{
				role: "assistant",
				content: [text("zero")],
				api: "faux",
				provider: "faux",
				model: "faux-1",
				usage: usageOf(0, 0, 0, 0, 0),
				stopReason: "stop",
				timestamp: 4,
			},
		],
	},
];

// ---------------------------------------------------------------------------
// 4. findCutPoint cases
// ---------------------------------------------------------------------------

const BIG = "x".repeat(400); // 100 tokens per big message
const SMALL = "y".repeat(40); // 10 tokens

function cutPointCases() {
	entrySeq = 0;
	const cases = [];

	// 4a. Crossing the threshold exactly: newest walk accumulates 60+50>=100
	// at the second user message; cut lands on it.
	{
		const entries = chain([
			userEntry(BIG),
			userEntry("z".repeat(200)), // 50 tokens
			userEntry("z".repeat(240)), // 60 tokens
		]);
		cases.push({
			name: "crosses_threshold_at_user_boundary",
			entries,
			startIndex: 0,
			endIndex: entries.length,
			keepRecentTokens: 100,
		});
	}

	// 4b. Never cuts at toolResult: budget reached at the toolResult entry, the
	// cut snaps forward to the assistant that follows it.
	{
		const entries = chain([
			userEntry(BIG),
			assistantEntry([text(BIG)]),
			toolResultEntry(BIG),
			assistantEntry([text(SMALL)]),
			toolResultEntry(SMALL),
		]);
		cases.push({
			name: "never_cuts_at_tool_result",
			entries,
			startIndex: 0,
			endIndex: entries.length,
			keepRecentTokens: 110,
		});
	}

	// 4c. Split turn: one huge turn; the cut lands mid-turn on an assistant
	// message and turnStartIndex points at the turn's user message.
	{
		const entries = chain([
			userEntry(SMALL), // turn 1 (fully summarized)
			assistantEntry([text(SMALL)]),
			userEntry(BIG), // turn 2 start (turnStartIndex)
			assistantEntry([text(BIG)]),
			toolResultEntry(BIG),
			assistantEntry([text(BIG)]),
			toolResultEntry(SMALL),
		]);
		cases.push({
			name: "split_turn_mid_turn_cut",
			entries,
			startIndex: 0,
			endIndex: entries.length,
			keepRecentTokens: 150,
		});
	}

	// 4d. Metadata absorption: label entries before the cut point are absorbed
	// into the kept span (cut moves backwards over them).
	{
		const mid = userEntry(BIG);
		const entries = chain([userEntry(BIG), mid, labelEntry(mid.id), userEntry(SMALL)]);
		cases.push({
			name: "absorbs_metadata_entries_before_cut",
			entries,
			startIndex: 0,
			endIndex: entries.length,
			keepRecentTokens: 20,
		});
	}

	// 4e. Under budget: accumulated never reaches keepRecentTokens, so the cut
	// defaults to the first valid cut point in range.
	{
		const entries = chain([userEntry(SMALL), assistantEntry([text(SMALL)])]);
		cases.push({
			name: "under_budget_keeps_from_first_cut_point",
			entries,
			startIndex: 0,
			endIndex: entries.length,
			keepRecentTokens: 100000,
		});
	}

	// 4f. No valid cut points in range.
	{
		const entries = chain([toolResultEntry(BIG), toolResultEntry(SMALL)]);
		cases.push({
			name: "no_valid_cut_points",
			entries,
			startIndex: 0,
			endIndex: entries.length,
			keepRecentTokens: 10,
		});
	}

	// 4g. Compaction entries are skipped as cut points and stop the metadata
	// absorption scan.
	{
		const entries = chain([
			userEntry(BIG),
			compactionEntry("summary", "e0001", 500),
			userEntry(BIG),
			userEntry(SMALL),
		]);
		cases.push({
			name: "compaction_boundary_not_crossed",
			entries,
			startIndex: 0,
			endIndex: entries.length,
			keepRecentTokens: 20,
		});
	}

	return cases;
}

// ---------------------------------------------------------------------------
// 5. prepareCompaction cases
// ---------------------------------------------------------------------------

function prepareCompactionCases() {
	entrySeq = 0;
	const cases = [];

	// 5a. First compaction: whole first turn summarized, second turn kept.
	{
		const entries = chain([
			userEntry(BIG),
			assistantEntry([text(BIG), toolCall("read", { path: "src/a.ts" })]),
			toolResultEntry("file body"),
			userEntry(SMALL),
			assistantEntry([text(SMALL)]),
		]);
		cases.push({
			name: "first_compaction",
			entries,
			settings: { enabled: true, reserveTokens: 16384, keepRecentTokens: 120 },
		});
	}

	// 5b. Split turn with file ops in both history and turn prefix.
	{
		const entries = chain([
			userEntry(BIG),
			assistantEntry([text(BIG), toolCall("write", { path: "out.txt", content: "c" })]),
			userEntry(BIG),
			assistantEntry([text(BIG), toolCall("edit", { path: "out.txt", old: "a", new: "b" })]),
			toolResultEntry(BIG),
			assistantEntry([text(BIG)]),
			toolResultEntry(SMALL),
		]);
		cases.push({
			name: "split_turn_preparation",
			entries,
			settings: { enabled: true, reserveTokens: 16384, keepRecentTokens: 150 },
		});
	}

	// 5c. Trailing compaction entry -> undefined.
	{
		const entries = chain([userEntry(SMALL), compactionEntry("s", "e0001", 10)]);
		cases.push({
			name: "trailing_compaction_returns_undefined",
			entries,
			settings: DEFAULT_COMPACTION_SETTINGS,
		});
	}

	// 5d. Iterative compaction: previous compaction boundary honored, previous
	// summary carried forward, prior details file lists merged.
	{
		const first = chain([
			userEntry(BIG),
			assistantEntry([text(BIG), toolCall("read", { path: "old.ts" })]),
			userEntry(SMALL),
		]);
		const prevCompaction = compactionEntry("previous summary body", first[1].id, 1234, {
			readFiles: ["prev-read.ts"],
			modifiedFiles: ["prev-mod.ts"],
		});
		const entries = chain([
			...first,
			prevCompaction,
			userEntry(BIG),
			assistantEntry([text(BIG), toolCall("edit", { path: "prev-mod.ts", old: "a", new: "b" })]),
			toolResultEntry(BIG),
			userEntry(SMALL),
		]);
		cases.push({
			name: "iterative_compaction",
			entries,
			settings: { enabled: true, reserveTokens: 16384, keepRecentTokens: 120 },
		});
	}

	return cases;
}

// ---------------------------------------------------------------------------
// 6. serializeConversation cases
// ---------------------------------------------------------------------------

const serializeCases = [
	{
		name: "all_roles",
		messages: [
			{ role: "user", content: "what is in a.ts?", timestamp: 1 },
			{
				role: "assistant",
				content: [thinking("let me look"), text("I will read it"), toolCall("read", { path: "a.ts" })],
				api: "faux",
				provider: "faux",
				model: "faux-1",
				usage: usageOf(1, 1, 0, 0, 2),
				stopReason: "stop",
				timestamp: 2,
			},
			{
				role: "toolResult",
				toolCallId: "call-1",
				toolName: "read",
				content: [text("const a = 1;")],
				isError: false,
				timestamp: 3,
			},
		],
	},
	{
		name: "tool_result_truncated_at_2000_chars",
		messages: [
			{
				role: "toolResult",
				toolCallId: "call-1",
				toolName: "bash",
				content: [text("q".repeat(2000 + 137))],
				isError: false,
				timestamp: 1,
			},
		],
	},
	{
		name: "assistant_tool_calls_only",
		messages: [
			{
				role: "assistant",
				content: [toolCall("bash", { command: "ls", timeout: 5 }), toolCall("read", { path: "b.ts" }, "call-2")],
				api: "faux",
				provider: "faux",
				model: "faux-1",
				usage: usageOf(1, 1, 0, 0, 2),
				stopReason: "stop",
				timestamp: 1,
			},
		],
	},
	{
		name: "empty_and_blank_parts_dropped",
		messages: [
			{ role: "user", content: "", timestamp: 1 },
			{ role: "user", content: "real question", timestamp: 2 },
			{
				role: "toolResult",
				toolCallId: "call-1",
				toolName: "read",
				content: [],
				isError: false,
				timestamp: 3,
			},
		],
	},
];

// ---------------------------------------------------------------------------
// 7. File operations cases
// ---------------------------------------------------------------------------

const fileOpsCases = [
	{
		name: "read_write_edit_dedup",
		ops: { read: ["b.ts", "a.ts", "c.ts"], written: ["c.ts"], edited: ["b.ts", "d.ts"] },
	},
	{ name: "empty", ops: { read: [], written: [], edited: [] } },
	{ name: "read_only", ops: { read: ["x.ts", "y.ts"], written: [], edited: [] } },
];

// ---------------------------------------------------------------------------
// 8. prepareBranchEntries cases
// ---------------------------------------------------------------------------

function branchEntryCases() {
	entrySeq = 0;
	const cases = [];

	// 8a. Budget loading: newest-first until budget; a summary-type entry that
	// would overflow is force-kept while under 90% of the budget.
	{
		const entries = chain([
			userEntry(BIG), // 100
			{
				type: "branch_summary",
				id: entryId(),
				parentId: null,
				timestamp: "2026-08-01T00:00:05.000Z",
				fromId: "e0001",
				summary: "s".repeat(400), // 100 tokens
			},
			userEntry(BIG), // 100
			assistantEntry([text(BIG)]), // 100
		]);
		cases.push({ name: "budget_loading_with_summary_force_keep", entries, tokenBudget: 260 });
	}

	// 8b. No budget (0 = unlimited).
	{
		const entries = chain([userEntry(BIG), assistantEntry([text(SMALL)])]);
		cases.push({ name: "no_budget", entries, tokenBudget: 0 });
	}

	// 8c. Tool results are skipped; file ops accumulate from nested branch
	// summary details + tool calls.
	{
		const entries = chain([
			userEntry(SMALL),
			assistantEntry([toolCall("read", { path: "r1.ts" })]),
			toolResultEntry("body"),
			{
				type: "branch_summary",
				id: entryId(),
				parentId: null,
				timestamp: "2026-08-01T00:00:06.000Z",
				fromId: "e0001",
				summary: "nested",
				details: { readFiles: ["nested-read.ts"], modifiedFiles: ["nested-mod.ts"] },
			},
		]);
		cases.push({ name: "tool_results_skipped_file_ops_merged", entries, tokenBudget: 0 });
	}

	return cases;
}

// ---------------------------------------------------------------------------
// 9. isContextOverflow cases (three branches + guards)
// ---------------------------------------------------------------------------

function overflowAssistant(stopReason, errorMessage, usage) {
	return {
		role: "assistant",
		content: [],
		api: "faux",
		provider: "faux",
		model: "faux-1",
		usage: usage ?? usageOf(0, 0, 0, 0, 0),
		stopReason,
		...(errorMessage ? { errorMessage } : {}),
		timestamp: 1,
	};
}

const overflowCases = [
	{
		name: "error_pattern_anthropic",
		message: overflowAssistant("error", "prompt is too long: 213462 tokens > 200000 maximum"),
		contextWindow: 200000,
	},
	{
		name: "error_pattern_excluded_throttling",
		message: overflowAssistant("error", "Throttling error: Too many tokens, please wait before trying again."),
		contextWindow: 200000,
	},
	{
		name: "silent_overflow_zai",
		message: overflowAssistant("stop", null, usageOf(210000, 10, 0, 0, 210010)),
		contextWindow: 200000,
	},
	{
		name: "silent_overflow_within_window",
		message: overflowAssistant("stop", null, usageOf(199999, 10, 0, 0, 200009)),
		contextWindow: 200000,
	},
	{
		name: "xiaomi_truncation_length_zero_output",
		message: overflowAssistant("length", null, usageOf(199000, 0, 0, 0, 199000)),
		contextWindow: 200000,
	},
	{
		name: "xiaomi_truncation_with_output_not_overflow",
		message: overflowAssistant("length", null, usageOf(199000, 5, 0, 0, 199005)),
		contextWindow: 200000,
	},
];

// ---------------------------------------------------------------------------
// Run the pure-function batteries
// ---------------------------------------------------------------------------

const golden = {
	upstream: "external/pi @ 2efa728 (v0.82.1), dist build",
	estimateTokens: estimateTokensCases.map((c) => ({ ...c, expected: estimateTokens(c.message) })),
	calculateContextTokens: calculateContextTokensCases.map((c) => ({
		...c,
		expected: calculateContextTokens(c.usage),
	})),
	estimateContextTokens: estimateContextTokensCases.map((c) => ({ ...c, expected: estimateContextTokens(c.messages) })),
	findCutPoint: cutPointCases().map((c) => ({ ...c, expected: findCutPoint(c.entries, c.startIndex, c.endIndex, c.keepRecentTokens) })),
	prepareCompaction: prepareCompactionCases().map((c) => {
		const prep = prepareCompaction(c.entries, c.settings);
		return {
			...c,
			expected: prep
				? {
						firstKeptEntryId: prep.firstKeptEntryId,
						isSplitTurn: prep.isSplitTurn,
						tokensBefore: prep.tokensBefore,
						previousSummary: prep.previousSummary ?? null,
						messagesToSummarizeCount: prep.messagesToSummarize.length,
						turnPrefixMessagesCount: prep.turnPrefixMessages.length,
						fileOps: {
							read: [...prep.fileOps.read].sort(),
							written: [...prep.fileOps.written].sort(),
							edited: [...prep.fileOps.edited].sort(),
						},
					}
				: null,
		};
	}),
	serializeConversation: serializeCases.map((c) => ({ ...c, expected: serializeConversation(c.messages) })),
	fileOps: fileOpsCases.map((c) => {
		const ops = createFileOps();
		for (const f of c.ops.read) ops.read.add(f);
		for (const f of c.ops.written) ops.written.add(f);
		for (const f of c.ops.edited) ops.edited.add(f);
		const lists = computeFileLists(ops);
		return { ...c, expected: { ...lists, formatted: formatFileOperations(lists.readFiles, lists.modifiedFiles) } };
	}),
	prepareBranchEntries: branchEntryCases().map((c) => {
		const prep = prepareBranchEntries(c.entries, c.tokenBudget);
		return {
			...c,
			expected: {
				messageCount: prep.messages.length,
				totalTokens: prep.totalTokens,
				roles: prep.messages.map((m) => m.role),
				fileOps: {
					read: [...prep.fileOps.read].sort(),
					written: [...prep.fileOps.written].sort(),
					edited: [...prep.fileOps.edited].sort(),
				},
			},
		};
	}),
	isContextOverflow: overflowCases.map((c) => ({ ...c, expected: isContextOverflow(c.message, c.contextWindow) })),
};

writeFileSync(join(outDir, "golden.json"), JSON.stringify(golden, null, 2) + "\n");
console.log(`golden.json written (${JSON.stringify(golden).length} bytes)`);

// ---------------------------------------------------------------------------
// Prompt captures (byte-exact renders from the real call sites)
// ---------------------------------------------------------------------------

const MODEL = {
	id: "faux-1",
	name: "Faux",
	api: "faux",
	provider: "faux",
	baseUrl: "http://localhost:0",
	reasoning: false,
	input: ["text"],
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
	contextWindow: 128000,
	maxTokens: 16384,
};

function captureStreamFn(texts) {
	const captured = [];
	let i = 0;
	const streamFn = (_model, context, options) => {
		const text = texts[Math.min(i, texts.length - 1)];
		i += 1;
		captured.push({ context, options });
		const message = {
			role: "assistant",
			content: [{ type: "text", text }],
			api: "faux",
			provider: "faux",
			model: "faux-1",
			usage: usageOf(11, 7, 0, 0, 18),
			stopReason: "stop",
			timestamp: 99,
		};
		return { result: async () => message };
	};
	return { captured, streamFn };
}

const SAMPLE_MESSAGES = [
	{ role: "user", content: "Please refactor src/auth.ts to split token handling.", timestamp: 1 },
	{
		role: "assistant",
		content: [text("I will read the file first."), toolCall("read", { path: "src/auth.ts" })],
		api: "faux",
		provider: "faux",
		model: "faux-1",
		usage: usageOf(10, 5, 0, 0, 15),
		stopReason: "stop",
		timestamp: 2,
	},
	{
		role: "toolResult",
		toolCallId: "call-1",
		toolName: "read",
		content: [text("export function refresh() { /* ... */ }")],
		isError: false,
		timestamp: 3,
	},
];

function writePrompt(name, content) {
	writeFileSync(join(promptsDir, name), content);
	console.log(`prompts/${name} (${content.length} chars)`);
}

// 1. Initial history summary render + system prompt.
{
	const { captured, streamFn } = captureStreamFn(["SUMMARY ONE"]);
	await generateSummaryWithUsage(SAMPLE_MESSAGES, MODEL, 16384, undefined, undefined, undefined, undefined, undefined, undefined, streamFn);
	writePrompt("system_prompt.txt", captured[0].context.systemPrompt);
	writePrompt("history_initial.txt", captured[0].context.messages[0].content[0].text);
	writePrompt("history_initial_options.json", JSON.stringify({ maxTokens: captured[0].options.maxTokens, cacheRetention: captured[0].options.cacheRetention, hasSessionId: typeof captured[0].options.sessionId === "string" && captured[0].options.sessionId.length > 0 }, null, 2) + "\n");
}

// 2. Iterative update render with previous summary + custom instructions.
{
	const { captured, streamFn } = captureStreamFn(["SUMMARY TWO"]);
	await generateSummaryWithUsage(
		SAMPLE_MESSAGES,
		MODEL,
		16384,
		undefined,
		undefined,
		undefined,
		"focus on the token refresh path",
		"## Goal\nPrevious goal text.",
		undefined,
		streamFn,
	);
	writePrompt("history_update.txt", captured[0].context.messages[0].content[0].text);
}

// 3. Turn prefix render (via compact() on a split-turn preparation) and the
//    split-turn merged summary assembly.
{
	entrySeq = 0;
	const entries = chain([
		userEntry(BIG),
		assistantEntry([text(BIG)]),
		userEntry(BIG), // split turn starts here
		assistantEntry([text(BIG)]),
		toolResultEntry(BIG),
		assistantEntry([text(BIG)]),
		toolResultEntry(SMALL),
	]);
	const preparation = prepareCompaction(entries, { enabled: true, reserveTokens: 16384, keepRecentTokens: 150 });
	if (!preparation?.isSplitTurn) throw new Error("expected a split-turn preparation");
	const { captured, streamFn } = captureStreamFn(["HISTORY SUMMARY TEXT", "TURN PREFIX SUMMARY TEXT"]);
	const result = await compact(preparation, MODEL, undefined, undefined, "keep the auth details", undefined, undefined, streamFn);
	writePrompt("history_in_split_turn.txt", captured[0].context.messages[0].content[0].text);
	writePrompt("turn_prefix.txt", captured[1].context.messages[0].content[0].text);
	writePrompt("split_turn_merged_summary.txt", result.summary);
	writeFileSync(
		join(promptsDir, "split_turn_result.json"),
		JSON.stringify(
			{
				tokensBefore: result.tokensBefore,
				firstKeptEntryId: result.firstKeptEntryId,
				usage: result.usage,
				details: result.details,
			},
			null,
			2,
		) + "\n",
	);
}

// 4. Non-split compact() with file ops -> summary + <read-files>/<modified-files>.
{
	entrySeq = 0;
	const entries = chain([
		// turn 1 (summarized; carries the file ops)
		userEntry(BIG),
		assistantEntry([
			text(BIG),
			toolCall("read", { path: "src/a.ts" }),
			toolCall("write", { path: "src/b.ts", content: "x" }, "call-2"),
			toolCall("edit", { path: "src/a.ts", old: "a", new: "b" }, "call-3"),
		]),
		toolResultEntry("body a"),
		// turn 2 (kept; crossing lands on its user message)
		userEntry(BIG),
		assistantEntry([text(SMALL), toolCall("bash", { command: "ls" }, "call-4")]),
		toolResultEntry("ok"),
		// turn 3 (kept)
		userEntry(SMALL),
		assistantEntry([text(SMALL)]),
	]);
	const preparation = prepareCompaction(entries, { enabled: true, reserveTokens: 16384, keepRecentTokens: 120 });
	if (!preparation || preparation.isSplitTurn) throw new Error("expected a non-split preparation");
	const { captured, streamFn } = captureStreamFn(["HISTORY ONLY SUMMARY"]);
	const result = await compact(preparation, MODEL, undefined, undefined, undefined, undefined, undefined, streamFn);
	writePrompt("compact_summary_with_file_lists.txt", result.summary);
	writeFileSync(
		join(promptsDir, "compact_result.json"),
		JSON.stringify(
			{
				tokensBefore: result.tokensBefore,
				firstKeptEntryId: result.firstKeptEntryId,
				usage: result.usage,
				details: result.details,
				promptText: captured[0].context.messages[0].content[0].text,
			},
			null,
			2,
		) + "\n",
	);
}

// 5. Branch summary render (prompt + preamble-spliced result).
{
	entrySeq = 0;
	const branchEntries = chain([
		userEntry("Explore the OAuth device flow."),
		assistantEntry([text("Reading the OAuth module."), toolCall("read", { path: "src/oauth.ts" })]),
		toolResultEntry("export async function deviceFlow() {}"),
		assistantEntry([text("The device flow polls every 5 seconds.")]),
	]);
	const { captured, streamFn } = captureStreamFn(["BRANCH SUMMARY TEXT"]);
	const result = await generateBranchSummary(branchEntries, { model: MODEL, streamFn, signal: undefined });
	writePrompt("branch.txt", captured[0].context.messages[0].content[0].text);
	writePrompt("branch_result_summary.txt", result.summary);
	writeFileSync(
		join(promptsDir, "branch_result.json"),
		JSON.stringify(
			{
				readFiles: result.readFiles,
				modifiedFiles: result.modifiedFiles,
				maxTokens: captured[0].options.maxTokens,
				cacheRetention: captured[0].options.cacheRetention,
				usage: result.usage,
			},
			null,
			2,
		) + "\n",
	);
}

// 6. Branch summary with custom instructions ("Additional focus:" splice).
{
	entrySeq = 0;
	const branchEntries = chain([userEntry("short branch"), assistantEntry([text("work")])]);
	const { captured, streamFn } = captureStreamFn(["BRANCH CUSTOM"]);
	await generateBranchSummary(branchEntries, {
		model: MODEL,
		streamFn,
		signal: undefined,
		customInstructions: "focus on OAuth",
	});
	writePrompt("branch_custom_instructions.txt", captured[0].context.messages[0].content[0].text);
}

console.log("prompt captures written");
