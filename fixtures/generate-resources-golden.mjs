#!/usr/bin/env node
/**
 * Resources golden-values generator (T09 parity layer).
 *
 * Runs the pinned upstream resource-loading implementation
 * (recording pin: external/pi @ 2efa728 — see the pin note in
 * fixtures/README.md; the current repo pin lives in UPSTREAM.md — built
 * dist) over fixed inputs and dumps golden
 * JSON per group under `fixtures/generated/resources/`:
 *
 *   - `skills-battery/golden.json`   — upstream `loadSkills()` over the 13
 *     upstream skill fixture dirs (copied to `skills-battery/input/skills/`,
 *     red line: read-only copy, external/ untouched) plus the
 *     `skills-collision` pair. Records name/description/location/sourceInfo/
 *     disableModelInvocation + warning/collision diagnostics.
 *   - `prompt-dsl/golden.json`       — `parseCommandArgs` + `substituteArgs`
 *     over the full DSL battery ($1..$N/$@/$ARGUMENTS/${N:-d}/${@:-d}/
 *     ${ARGUMENTS:-d}/${@:N}/${@:N:L}, quote-aware parsing, missing → "",
 *     no recursion into argument/default values).
 *   - `themes/golden.json`           — `loadThemeFromPath()` over custom
 *     theme JSON (vars refs, 256-color ints, "" default, thinkingMax
 *     fallback, invalid-value diagnostics) in both color modes, plus the
 *     resolved-color snapshots of builtin dark/light.
 *   - `keybindings/golden.json`      — `migrateKeybindingsConfig()` over
 *     legacy-name configs incl. old+new conflicts and ordering.
 *   - `settings/golden.json`         — `deepMergeSettings` battery observed
 *     through the `SettingsManager.fromStorage()` getter surface (nested
 *     single-level shallow merge / depth≥2 replace / array+scalar replace)
 *     and the 4 legacy-format migrations (`migrateSettings`).
 *   - `resource-loader-e2e/golden.json` — `DefaultResourceLoader` over a
 *     multi-level tree (global agentDir + project config + settings paths +
 *     CLI paths + ancestor `.agents/skills` + git-repo boundary). The tree
 *     is copied to a temp dir; `repo/.git` (untrackable) and the `.pi` twin
 *     of `.rpi` (upstream reads `.pi`, rpi reads `.rpi` — intentional rename)
 *     are created by the script; the Rust test repeats the same prep.
 *
 * Absolute paths in goldens are rewritten to `<path>` at generation time
 * (the rpi-test-support Normalizer path placeholder); the Rust side applies
 * `Normalizer::with_path(root)` for the same rewrite before diffing.
 *
 * The Rust side (crates/rpi/tests/parity_resources_test.rs) replays every
 * case — do not edit the outputs by hand; regenerate with:
 *
 *   node fixtures/generate-resources-golden.mjs
 *
 * Prerequisites: upstream dist built (see fixtures/README.md §2).
 */

import { copyFileSync, cpSync, existsSync, mkdirSync, mkdtempSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { loadSkills } from "../external/pi/packages/coding-agent/dist/core/skills.js";
import { parseCommandArgs, substituteArgs } from "../external/pi/packages/coding-agent/dist/core/prompt-templates.js";
import { getResolvedThemeColors, loadThemeFromPath } from "../external/pi/packages/coding-agent/dist/modes/interactive/theme/theme.js";
import { migrateKeybindingsConfig } from "../external/pi/packages/coding-agent/dist/core/keybindings.js";
import { InMemorySettingsStorage, SettingsManager } from "../external/pi/packages/coding-agent/dist/core/settings-manager.js";
import { DefaultResourceLoader } from "../external/pi/packages/coding-agent/dist/core/resource-loader.js";

const here = dirname(fileURLToPath(import.meta.url));
const resourcesDir = join(here, "generated", "resources");

const UPSTREAM = "external/pi @ 2efa728 (v0.82.1), dist build";

/** Replace every occurrence of `root` in any string with the `<path>` placeholder. */
function stripRoot(value, root) {
	if (typeof value === "string") {
		return value.split(root).join("<path>");
	}
	if (Array.isArray(value)) {
		return value.map((v) => stripRoot(v, root));
	}
	if (value !== null && typeof value === "object") {
		return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, stripRoot(v, root)]));
	}
	return value;
}

/**
 * Canonicalize the intentional `.pi` → `.rpi` rename (ADR-0001) in path strings: upstream discovers `cwd/.pi`, rpi discovers
 * `cwd/.rpi`; goldens are recorded in the rpi spelling so the Rust side
 * needs no per-test compensation. Only path-segment occurrences are
 * rewritten (`/.pi/` or a trailing `/.pi`).
 */
function renamePiConfigDir(value) {
	if (typeof value === "string") {
		return value.replace(/\/\.pi(?=\/|$)/g, "/.rpi");
	}
	if (Array.isArray(value)) {
		return value.map((v) => renamePiConfigDir(v));
	}
	if (value !== null && typeof value === "object") {
		return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, renamePiConfigDir(v)]));
	}
	return value;
}

function writeGolden(group, golden) {
	const file = join(resourcesDir, group, "golden.json");
	mkdirSync(dirname(file), { recursive: true });
	writeFileSync(file, JSON.stringify(golden, null, 2) + "\n");
	console.log(`${group}/golden.json written (${JSON.stringify(golden).length} bytes)`);
}

// ---------------------------------------------------------------------------
// Summary shapes (mirrored field-by-field by the Rust test)
// ---------------------------------------------------------------------------

function summarizeSkill(skill) {
	return {
		name: skill.name,
		description: skill.description,
		filePath: skill.filePath,
		baseDir: skill.baseDir,
		disableModelInvocation: skill.disableModelInvocation,
		source: skill.sourceInfo.source,
		scope: skill.sourceInfo.scope,
		origin: skill.sourceInfo.origin,
		sourceBaseDir: skill.sourceInfo.baseDir ?? null,
	};
}

function summarizeDiagnostic(d, dropMessage = false) {
	return {
		type: d.type,
		// Parser-engine error texts (JS yaml vs serde_yaml) are not part of
		// the contract; `dropMessage` records only type/path/collision.
		message: dropMessage ? null : d.message,
		path: d.path ?? null,
		collision: d.collision
			? {
					resourceType: d.collision.resourceType,
					name: d.collision.name,
					winnerPath: d.collision.winnerPath,
					loserPath: d.collision.loserPath,
					winnerSource: d.collision.winnerSource ?? null,
					loserSource: d.collision.loserSource ?? null,
				}
			: null,
	};
}

// ---------------------------------------------------------------------------
// 1. skills-battery
// ---------------------------------------------------------------------------

function generateSkillsBattery() {
	const inputRoot = join(resourcesDir, "skills-battery", "input");
	const agentDir = join(inputRoot, "nonexistent-agent-dir");
	const caseDirs = readdirSync(join(inputRoot, "skills"), { withFileTypes: true })
		.filter((e) => e.isDirectory())
		.map((e) => e.name)
		.sort();

	const cases = [];
	for (const dir of caseDirs) {
		const result = loadSkills({
			cwd: inputRoot,
			agentDir,
			skillPaths: [join(inputRoot, "skills", dir)],
			includeDefaults: false,
		});
		// The invalid-yaml diagnostic message is the JS yaml parser's error
		// text; serde_yaml words it differently (skill drop + warning shape
		// stay contractual).
		const engineDependent = dir === "invalid-yaml";
		cases.push({
			name: dir,
			skillPaths: [`skills/${dir}`],
			engineDependentMessages: engineDependent,
			// JS yaml keeps the trailing newline of a `|` block scalar even
			// when the frontmatter slice drops the final "\n"; serde_yaml does
			// not. The Rust test compares descriptions with trailing newlines
			// trimmed on both sides for this case.
			engineDependentTrailingNewline: dir === "multiline-description",
			expected: {
				skills: result.skills.map(summarizeSkill),
				diagnostics: result.diagnostics.map((d) => summarizeDiagnostic(d, engineDependent)),
			},
		});
	}

	// Name collision: first path wins, loser reported as a collision diagnostic.
	{
		const result = loadSkills({
			cwd: inputRoot,
			agentDir,
			skillPaths: [join(inputRoot, "skills-collision", "first"), join(inputRoot, "skills-collision", "second")],
			includeDefaults: false,
		});
		cases.push({
			name: "collision-first-wins",
			skillPaths: ["skills-collision/first", "skills-collision/second"],
			expected: {
				skills: result.skills.map(summarizeSkill),
				diagnostics: result.diagnostics.map((d) => summarizeDiagnostic(d)),
			},
		});
	}

	writeGolden("skills-battery", stripRoot({ upstream: UPSTREAM, cases }, inputRoot));
}

// ---------------------------------------------------------------------------
// 2. prompt-dsl
// ---------------------------------------------------------------------------

const PROMPT_DSL_CASES = [
	{ name: "positional", content: "Review $1 for $2.", argsString: "auth.ts security" },
	{ name: "positional_missing_is_empty", content: "[$1][$2][$3]", argsString: "a b" },
	{ name: "all_args_at", content: "files: $@", argsString: 'one "two words" three' },
	{ name: "all_args_arguments", content: "files: $ARGUMENTS", argsString: "one two" },
	{ name: "all_args_empty", content: "[$@]", argsString: "" },
	{ name: "default_used_when_missing", content: "open ${1:-index.ts}", argsString: "" },
	{ name: "default_not_used_when_present", content: "open ${1:-index.ts}", argsString: "main.ts" },
	{ name: "default_at_used_when_empty", content: "grep ${@:-*}", argsString: "" },
	{ name: "default_arguments_used_when_empty", content: "run ${ARGUMENTS:-everything}", argsString: "" },
	{ name: "default_arguments_present", content: "run ${ARGUMENTS:-everything}", argsString: "x y" },
	{ name: "default_not_recursive", content: "say ${1:-$2}", argsString: "" },
	{ name: "arg_value_not_recursive", content: "echo $1", argsString: "xx$2yy" },
	{ name: "slice_from_second", content: "rest: ${@:2}", argsString: "a b c d" },
	{ name: "slice_with_length", content: "mid: ${@:2:2}", argsString: "a b c d" },
	{ name: "slice_zero_is_one", content: "all: ${@:0}", argsString: "a b" },
	{ name: "slice_beyond_end", content: "none: [${@:5}]", argsString: "a b" },
	{ name: "quotes_single_and_double", content: "$1|$2|$3", argsString: "'hello world' \"it's\" x" },
	{ name: "unterminated_quote_swallows_rest", content: "[$1]", argsString: '"abc def' },
	{ name: "empty_quotes_are_dropped", content: "[$1]", argsString: '""' },
	{ name: "two_digit_index", content: "ten: $10", argsString: "1 2 3 4 5 6 7 8 9 ten" },
	{ name: "mixed_dsl_forms", content: "$1 then ${2:-none} then ${@:3}", argsString: "a b c d" },
	{ name: "dollar_without_pattern", content: "cost is $ 5 and $x stays", argsString: "a" },
];

function generatePromptDsl() {
	const cases = PROMPT_DSL_CASES.map((c) => {
		const args = parseCommandArgs(c.argsString);
		return { ...c, args, expected: substituteArgs(c.content, args) };
	});
	writeGolden("prompt-dsl", { upstream: UPSTREAM, cases });
}

// ---------------------------------------------------------------------------
// 3. themes
// ---------------------------------------------------------------------------

// The 51 required color tokens (theme-schema.json; thinkingMax is optional).
const REQUIRED_COLOR_KEYS = [
	"accent", "border", "borderAccent", "borderMuted", "success", "error", "warning",
	"muted", "dim", "text", "thinkingText",
	"selectedBg", "userMessageBg", "userMessageText", "customMessageBg", "customMessageText",
	"customMessageLabel", "toolPendingBg", "toolSuccessBg", "toolErrorBg", "toolTitle", "toolOutput",
	"mdHeading", "mdLink", "mdLinkUrl", "mdCode", "mdCodeBlock", "mdCodeBlockBorder",
	"mdQuote", "mdQuoteBorder", "mdHr", "mdListBullet",
	"toolDiffAdded", "toolDiffRemoved", "toolDiffContext",
	"syntaxComment", "syntaxKeyword", "syntaxFunction", "syntaxVariable", "syntaxString",
	"syntaxNumber", "syntaxType", "syntaxOperator", "syntaxPunctuation",
	"thinkingOff", "thinkingMinimal", "thinkingLow", "thinkingMedium", "thinkingHigh", "thinkingXhigh",
	"bashMode",
];

function baseColors(fill) {
	return Object.fromEntries(REQUIRED_COLOR_KEYS.map((k) => [k, fill]));
}

/** Sorted plain object from a Map (Rust side sorts the same keys). */
function sortedEntries(map) {
	return Object.fromEntries([...map.entries()].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)));
}

const THEME_MODES = ["truecolor", "256color"];

const THEME_CASES = [
	{
		name: "vars-and-hex",
		json: {
			name: "vars-and-hex",
			vars: { primary: "#123456", secondary: "primary", accent256: 196 },
			colors: { ...baseColors("primary"), accent: "secondary", border: "#abcdef", success: "accent256" },
		},
	},
	{
		name: "palette-256",
		json: { name: "palette-256", colors: { ...baseColors(0), accent: 255, border: 128, success: 232 } },
	},
	{
		name: "empty-string-default",
		json: { name: "empty-string-default", colors: { ...baseColors(""), accent: "#ff8800" } },
	},
	{
		name: "thinkingmax-fallback",
		json: { name: "thinkingmax-fallback", colors: { ...baseColors("#101010"), thinkingXhigh: "#abcdef" } },
	},
	{
		name: "thinkingmax-explicit",
		json: {
			name: "thinkingmax-explicit",
			colors: { ...baseColors("#101010"), thinkingXhigh: "#abcdef", thinkingMax: "#fedcba" },
		},
	},
	{
		name: "invalid-missing-colors",
		json: { name: "invalid-missing-colors", colors: { accent: "#123456", border: 7 } },
		error: true,
	},
	{
		name: "invalid-color-value-type",
		json: { name: "invalid-color-value-type", colors: { ...baseColors("#123456"), accent: 256, border: true } },
		error: true,
		// The error body is typebox-validator wording upstream vs the port's
		// hand-rolled validator wording — only the stable parts are pinned.
		engineDependent: true,
		errorContains: ["Other errors:"],
	},
	{
		name: "invalid-circular-var",
		json: {
			name: "invalid-circular-var",
			vars: { a: "b", b: "a" },
			colors: { ...baseColors("#123456"), accent: "a" },
		},
		error: true,
	},
	{
		name: "invalid-unresolved-var",
		json: { name: "invalid-unresolved-var", colors: { ...baseColors("#123456"), accent: "nonexistent" } },
		error: true,
	},
	{
		name: "invalid-name-slash",
		json: { name: "foo/bar", colors: baseColors("#123456") },
		error: true,
	},
	{
		name: "invalid-json-document",
		raw: "{ not json ",
		error: true,
		// The trailing text is the JS engine's SyntaxError string; the port
		// emits the serde_json error text instead. Only the prefix is pinned.
		engineDependent: true,
		errorContains: [],
	},
];

function generateThemes() {
	const root = mkdtempSync(join(tmpdir(), "rpi-golden-themes-"));
	try {
		const cases = [];
		for (const c of THEME_CASES) {
			const content = c.raw ?? JSON.stringify(c.json, null, 2);
			const themePath = join(root, `${c.name}.json`);
			writeFileSync(themePath, content);
			const expected = { content };
			for (const mode of THEME_MODES) {
				try {
					const theme = loadThemeFromPath(themePath, mode);
					expected[mode] = {
						name: theme.name,
						fgColors: sortedEntries(theme.fgColors),
						bgColors: sortedEntries(theme.bgColors),
					};
				} catch (error) {
					const message = error instanceof Error ? error.message : String(error);
					if (c.engineDependent) {
						// Pin the first line up to the engine/validator-specific
						// text (the label-carrying prefix is the stable contract).
						const firstLine = message.split("\n")[0];
						const prefix = firstLine.includes("SyntaxError")
							? firstLine.slice(0, firstLine.indexOf("SyntaxError"))
							: firstLine;
						expected[mode] = { errorPrefix: prefix, errorContains: c.errorContains ?? [] };
					} else {
						expected[mode] = { error: message };
					}
				}
			}
			cases.push({ name: c.name, expected });
		}

		const builtins = {
			dark: Object.fromEntries(Object.entries(getResolvedThemeColors("dark")).sort()),
			light: Object.fromEntries(Object.entries(getResolvedThemeColors("light")).sort()),
		};

		writeGolden("themes", stripRoot({ upstream: UPSTREAM, cases, builtins }, root));
	} finally {
		rmSync(root, { recursive: true, force: true });
	}
}

// ---------------------------------------------------------------------------
// 4. keybindings
// ---------------------------------------------------------------------------

const KEYBINDING_CASES = [
	{
		name: "legacy-names-migrate",
		input: { cursorUp: "ctrl+k", submit: "enter", followUp: "alt+enter" },
	},
	{
		name: "conflict-new-name-wins",
		input: { cursorUp: "up", "tui.editor.cursorUp": "down" },
	},
	{
		name: "new-names-passthrough",
		input: { "app.interrupt": "escape", "tui.select.confirm": "enter" },
	},
	{
		name: "extras-sorted-after-known",
		input: { "zzz.custom": "x", "app.clear": "ctrl+c", "aaa.custom": "y", "tui.input.newLine": "shift+enter" },
	},
	{
		name: "raw-values-preserved",
		input: { selectDown: ["j", "down"], selectUp: 5, "tui.select.cancel": null, "app.exit": "ctrl+d" },
	},
];

function generateKeybindings() {
	const cases = KEYBINDING_CASES.map((c) => {
		const { config, migrated } = migrateKeybindingsConfig(c.input);
		return { ...c, expected: { config, migrated } };
	});
	writeGolden("keybindings", { upstream: UPSTREAM, cases });
}

// ---------------------------------------------------------------------------
// 5. settings
// ---------------------------------------------------------------------------

const DEEP_MERGE_CASES = [
	{
		name: "nested-single-level-shallow-merge",
		global: { compaction: { enabled: false, reserveTokens: 4096 } },
		project: { compaction: { keepRecentTokens: 512, reserveTokens: 8192 } },
	},
	{
		name: "depth-two-object-replaced",
		global: { retry: { maxRetries: 5, provider: { timeoutMs: 1000, maxRetries: 2 } } },
		project: { retry: { provider: { maxRetryDelayMs: 999 } } },
	},
	{
		name: "arrays-replaced-wholesale",
		global: { packages: ["a", "b"], extensions: ["x"] },
		project: { packages: ["c"] },
	},
	{
		name: "scalars-and-null-replaced",
		global: { theme: "dark", quietStartup: true, steeringMode: "one-at-a-time" },
		project: { theme: null, steeringMode: "all" },
	},
	{
		name: "branch-summary-merge",
		global: { branchSummary: { reserveTokens: 1024, skipPrompt: true } },
		project: { branchSummary: { skipPrompt: false } },
	},
];

const MIGRATION_CASES = [
	{ name: "queue-mode-to-steering-mode", input: { queueMode: "all" } },
	{ name: "queue-mode-kept-when-steering-mode-present", input: { queueMode: "all", steeringMode: "one-at-a-time" } },
	{ name: "websockets-true-to-transport", input: { websockets: true } },
	{ name: "websockets-false-to-transport", input: { websockets: false } },
	{
		name: "skills-object-with-custom-directories",
		input: { skills: { enableSkillCommands: false, customDirectories: ["./a", "./b"] } },
	},
	{
		name: "skills-object-without-custom-directories",
		input: { skills: { enableSkillCommands: true } },
	},
	{ name: "retry-max-delay-ms", input: { retry: { maxDelayMs: 5000, enabled: false } } },
	{
		name: "retry-max-delay-ms-provider-wins",
		input: { retry: { maxDelayMs: 5000, provider: { maxRetryDelayMs: 1000 } } },
	},
];

function managerSurface(manager) {
	return {
		compaction: manager.getCompactionSettings(),
		branchSummary: manager.getBranchSummarySettings(),
		retry: manager.getRetrySettings(),
		providerRetry: {
			timeoutMs: manager.getProviderRetrySettings().timeoutMs ?? null,
			maxRetries: manager.getProviderRetrySettings().maxRetries ?? null,
			maxRetryDelayMs: manager.getProviderRetrySettings().maxRetryDelayMs,
		},
		packages: manager.getPackages(),
		extensionPaths: manager.getExtensionPaths(),
		themeSetting: manager.getThemeSetting() ?? null,
		quietStartup: manager.getQuietStartup(),
		steeringMode: manager.getSteeringMode(),
	};
}

function generateSettings() {
	const deepMerge = DEEP_MERGE_CASES.map((c) => {
		const storage = new InMemorySettingsStorage();
		storage.global = JSON.stringify(c.global);
		storage.project = JSON.stringify(c.project);
		const manager = SettingsManager.fromStorage(storage);
		return { ...c, expected: managerSurface(manager) };
	});
	const migrations = MIGRATION_CASES.map((c) => ({
		...c,
		expected: SettingsManager.migrateSettings(structuredClone(c.input)),
	}));
	writeGolden("settings", { upstream: UPSTREAM, deepMerge, migrations });
}

// ---------------------------------------------------------------------------
// 6. resource-loader-e2e
// ---------------------------------------------------------------------------

/**
 * Prepare a runnable copy of the committed input tree (mirrored by the Rust
 * test): copy to `dest`, duplicate every `.rpi` dir as `.pi` (upstream reads
 * `.pi`, rpi reads `.rpi`), create the untrackable `repo/.git` marker.
 */
function prepareE2eTree(inputRoot, dest) {
	cpSync(inputRoot, dest, { recursive: true });
	for (const dir of ["repo", join("repo", "sub")]) {
		const rpiDir = join(dest, dir, ".rpi");
		if (existsSync(rpiDir)) {
			cpSync(rpiDir, join(dest, dir, ".pi"), { recursive: true });
		}
	}
	mkdirSync(join(dest, "repo", ".git"), { recursive: true });
}

async function generateResourceLoaderE2e() {
	const inputRoot = join(resourcesDir, "resource-loader-e2e", "input");
	const root = mkdtempSync(join(tmpdir(), "rpi-golden-e2e-"));
	try {
		prepareE2eTree(inputRoot, root);
		const cwd = join(root, "repo", "sub");
		const agentDir = join(root, "agent");
		process.env.HOME = join(root, "home");

		const settingsManager = SettingsManager.create(cwd, agentDir);
		const loader = new DefaultResourceLoader({
			cwd,
			agentDir,
			settingsManager,
			noExtensions: true,
			additionalSkillPaths: [join(root, "cli", "cli-skill"), join(root, "cli", "cli-single.md")],
		});
		await loader.reload();

		const skills = loader.getSkills();
		const prompts = loader.getPrompts();
		const themes = loader.getThemes();
		const agentsFiles = loader.getAgentsFiles();

		const golden = {
			upstream: UPSTREAM,
			cwd: "repo/sub",
			cliSkillPaths: ["cli/cli-skill", "cli/cli-single.md"],
			expected: {
				skills: skills.skills.map(summarizeSkill),
				skillDiagnostics: skills.diagnostics.map((d) => summarizeDiagnostic(d)),
				prompts: prompts.prompts.map((p) => ({
					name: p.name,
					description: p.description,
					argumentHint: p.argumentHint ?? null,
					filePath: p.filePath,
				})),
				promptDiagnostics: prompts.diagnostics.map((d) => summarizeDiagnostic(d)),
				themes: themes.themes.map((t) => ({ name: t.name ?? null, sourcePath: t.sourcePath ?? null })),
				themeDiagnostics: themes.diagnostics.map((d) => summarizeDiagnostic(d)),
				contextFiles: agentsFiles.agentsFiles.map((f) => ({ path: f.path, content: f.content })),
				systemPrompt: loader.getSystemPrompt() ?? null,
				appendSystemPrompt: loader.getAppendSystemPrompt(),
			},
		};
		writeGolden("resource-loader-e2e", renamePiConfigDir(stripRoot(golden, root)));
	} finally {
		rmSync(root, { recursive: true, force: true });
	}
}

// ---------------------------------------------------------------------------

const groups = {
	"skills-battery": generateSkillsBattery,
	"prompt-dsl": generatePromptDsl,
	themes: generateThemes,
	keybindings: generateKeybindings,
	settings: generateSettings,
	"resource-loader-e2e": generateResourceLoaderE2e,
};

const selected = process.argv.slice(2);
const names = selected.length > 0 ? selected : Object.keys(groups);
for (const name of names) {
	const generate = groups[name];
	if (!generate) {
		console.error(`unknown group: ${name} (have: ${Object.keys(groups).join(", ")})`);
		process.exit(1);
	}
	await generate();
}
console.log(`resources goldens written to ${resourcesDir}`);
