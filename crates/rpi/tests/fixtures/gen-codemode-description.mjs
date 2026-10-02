#!/usr/bin/env node
// Regenerates `codemode-description.json` from the pinned upstream checkout
// (`external/pi` @ a13d35a74, v1.0.0). See the sibling README for the
// provenance and the byte-parity test that consumes the fixture.
//
// Usage (Node 22.18+/24 with built-in type stripping):
//   node --experimental-strip-types crates/rpi/tests/fixtures/gen-codemode-description.mjs
//
// No upstream file is modified: the needed sources are copied to a temp
// directory with their `@earendil-works/pi-codemode` imports rewritten to the
// copied package files and `getDocsPath()` stubbed to `PI_PACKAGE_DIR`.

import { cpSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, "../../../..");
const upstream = join(repoRoot, "external/pi");
const codemodeSrc = join(upstream, "packages/codemode/src");
const toolSrc = join(upstream, "packages/coding-agent/src/extensions/codemode/tool.ts");

// Both sides render `<packageDir>/docs/codemode.md`; the parity test sets the
// same RPI_PACKAGE_DIR.
process.env.PI_PACKAGE_DIR = "/tmp/rpi-codemode-docs";

const tmp = mkdtempSync(join(tmpdir(), "rpi-codemode-descgen-"));
mkdirSync(join(tmp, "codemode"), { recursive: true });
for (const file of ["declarations.ts", "identifier.ts", "types.ts", "wasm.ts"]) {
	cpSync(join(codemodeSrc, file), join(tmp, "codemode", file));
}

let source = readFileSync(toolSrc, "utf8");
source = source.replace('import { join } from "node:path";', "");
source = source.replace('import type { AgentTool } from "@earendil-works/pi-agent-core";', "");
source = source.replace(
	'import type { CodemodeJsonSchema, CodemodeTool } from "@earendil-works/pi-codemode";',
	"",
);
source = source.replace(
	'from "@earendil-works/pi-codemode/declarations"',
	'from "./codemode/declarations.ts"',
);
source = source.replace(
	'import { CODEMODE_SOURCE_GRAMMAR } from "@earendil-works/pi-codemode/source";',
	'const CODEMODE_SOURCE_GRAMMAR = "";',
);
source = source.replace(
	'import { type Static, Type } from "typebox";',
	"const Type = { Object: (v) => v, String: (v) => ({ type: 'string', ...v }) };",
);
source = source.replace(
	'import { getDocsPath } from "../../config.ts";',
	'import { getDocsPath } from "./config-stub.ts";',
);
source = source.replace(/import type \{[^}]*\} from "[^"]*";/gs, "");
source = source.replace(
	'import { wrapToolDefinition } from "../../core/tools/tool-definition-wrapper.ts";',
	"const wrapToolDefinition = (definition) => ({ ...definition });",
);
source = source.replace(
	'import { loadCodemodeExecutor } from "./execute.lazy.ts";',
	"const loadCodemodeExecutor = async () => ({ executeCodemode: async () => ({}) });",
);
source = source.replace('import { codemodeRenderers } from "./renderer.ts";', "const codemodeRenderers = {};");
source = source.replace('join(getDocsPath(), "codemode.md")', 'getDocsPath() + "/codemode.md"');
source += "\nexport { describeScriptCall, describeOutput };\n";
writeFileSync(join(tmp, "tool.ts"), source);
writeFileSync(
	join(tmp, "config-stub.ts"),
	'import { resolve, join } from "node:path";\nexport function getDocsPath() { return resolve(join(process.env.PI_PACKAGE_DIR ?? "/pkg", "docs")); }\n',
);

const mod = await import(pathToFileURL(join(tmp, "tool.ts")).href);

const echo = {
	name: "echo",
	description: "Echo text back.\n\nSecond paragraph.",
	parameters: {
		type: "object",
		properties: { text: { type: "string", description: "Text to echo" } },
		required: ["text"],
		additionalProperties: false,
	},
};
const read = {
	name: "read",
	description: "Read a file.",
	parameters: {
		type: "object",
		properties: { path: { type: "string" } },
		required: ["path"],
		additionalProperties: false,
	},
	outputSchema: { type: "string" },
};
const stats = {
	name: "stats",
	description: "Return structured stats",
	parameters: { type: "object", properties: {}, additionalProperties: false },
	outputSchema: {
		type: "object",
		properties: { files: { type: "number" }, names: { type: "array", items: { type: "string" } } },
		required: ["files", "names"],
	},
};
const mcp = {
	name: "mcp__docs__search",
	description: "Search the docs.",
	parameters: {
		type: "object",
		properties: { query: { type: "string" } },
		required: ["query"],
	},
	outputSchema: {
		type: "object",
		properties: {
			content: { type: "array", items: { type: "object" } },
			structuredContent: {
				type: "object",
				properties: { hit: { type: "boolean" } },
				required: ["hit"],
			},
			isError: { type: "boolean" },
			_meta: { type: "object" },
		},
		required: ["content"],
	},
};
const deferredTool = {
	name: "secret",
	description: "Deferred tool.",
	parameters: { type: "object", properties: {} },
};
const long = {
	name: "long_tool",
	description: "A".repeat(4000),
	parameters: { type: "object", properties: {} },
};
const ns = (name) => ({ name, description: "Docs tools", instructions: "Prefer search." });

const cases = [
	{ name: "empty-models", tools: [], options: { models: true, inlineBudget: 3000 } },
	{ name: "empty-no-models", tools: [], options: { models: false } },
	{ name: "echo-budget-none", tools: [echo], options: { models: true } },
	{ name: "read-stats", tools: [read, stats], options: { models: true, inlineBudget: 3000 } },
	{
		name: "deferred-excluded",
		tools: [echo, deferredTool],
		options: { models: true, deferred: ["secret"], inlineBudget: 3000 },
	},
	{
		name: "namespace-group",
		tools: [echo, { ...read, name: "mcp__dev__read" }],
		options: {
			models: true,
			namespaces: [{ tool: "mcp__dev__read", namespace: ns("mcp__dev") }],
			inlineBudget: 3000,
		},
	},
	{ name: "budget-rotation", tools: [long, echo, read], options: { models: true, inlineBudget: 40 } },
	{ name: "budget-zero", tools: [echo], options: { models: true, inlineBudget: 0 } },
	{ name: "mcp-shared-types", tools: [mcp, echo], options: { models: true, inlineBudget: 2000 } },
];

const result = { cases: [] };
for (const entry of cases) {
	const options = { ...entry.options };
	if (Array.isArray(options.deferred)) options.deferred = new Set(options.deferred);
	if (Array.isArray(options.namespaces)) {
		options.namespaces = new Map(options.namespaces.map((item) => [item.tool, item.namespace]));
	}
	result.cases.push({
		name: entry.name,
		tools: entry.tools,
		options: entry.options,
		description: mod.createCodemodeDescription(entry.tools, options),
	});
}

const callForms = [
	{ name: "echo", description: echo.description, parameters: echo.parameters },
	{ name: "read", description: read.description, parameters: read.parameters, outputSchema: read.outputSchema },
	{ name: "stats", description: stats.description, parameters: stats.parameters, outputSchema: stats.outputSchema },
	{ name: "mcp", description: mcp.description, parameters: mcp.parameters, outputSchema: mcp.outputSchema },
	{
		name: "opaque",
		description: "Opaque.",
		parameters: {},
		outputSchema: { anyOf: [{ type: "string" }, { type: "number" }] },
	},
];
result.scriptCalls = callForms.map((tool) => ({ ...tool, text: mod.describeScriptCall(tool) }));
result.describeOutputs = [
	{ name: "missing", outputSchema: null, text: mod.describeOutput(undefined) },
	{ name: "string", outputSchema: { type: "string" }, text: mod.describeOutput({ type: "string" }) },
	{ name: "object", outputSchema: stats.outputSchema, text: mod.describeOutput(stats.outputSchema) },
	{ name: "mcp", outputSchema: mcp.outputSchema, text: mod.describeOutput(mcp.outputSchema) },
	{
		name: "union",
		outputSchema: { anyOf: [{ type: "string" }, { type: "number" }] },
		text: mod.describeOutput({ anyOf: [{ type: "string" }, { type: "number" }] }),
	},
];

writeFileSync(join(here, "codemode-description.json"), JSON.stringify(result, null, 1));
rmSync(tmp, { recursive: true, force: true });
console.log(`wrote ${join(here, "codemode-description.json")}`);