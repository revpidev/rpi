#!/usr/bin/env node
// Regenerate the golden fixtures of `crates/rpi-ext-mcp-adapter` by running
// the PINNED upstream implementation itself (design
// `docs/extensions/pi-mcp-adapter/02-design.md` §5.1: pure-function golden
// parity; fixtures must be reproducible from the pinned commit + this
// script).
//
// Upstream: `rpi/external/pi-mcp-adapter` @ v2.24.0
// (3d953f9096bf8af05783a740c6608663a2c3180a) — read-only, never modified.
//
// Prerequisite (kept out of the repo on purpose; the submodule must stay
// pristine):
//
//   npm install --prefix /tmp/mcp-fixture-deps strip-json-comments
//
// If that install is missing, generation continues with a passthrough strip
// and every fixture input stays comment-free EXCEPT the JSONC cases, which
// are then skipped with a warning.
//
// Usage: node rpi/scripts/gen-mcp-adapter-fixtures.mjs
// Output: crates/rpi-ext-mcp-adapter/tests/fixtures/*.json (committed).

import { register } from "node:module";
import { existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const REPO = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const UPSTREAM = join(REPO, "external", "pi-mcp-adapter");
const OUT_DIR = join(REPO, "crates", "rpi-ext-mcp-adapter", "tests", "fixtures");

const STRIP_PKG = "/tmp/mcp-fixture-deps/node_modules/strip-json-comments/index.js";
const haveRealStrip = existsSync(STRIP_PKG);
process.env.RPI_MCP_FIXTURE_STRIP_JSON_COMMENTS_URL = haveRealStrip
  ? pathToFileURL(STRIP_PKG).href
  : new URL("./mcp-fixture-stub-strip-json-comments.mjs", import.meta.url).href;
if (!haveRealStrip) {
  console.warn("[gen-fixtures] strip-json-comments not installed; JSONC cases will be skipped");
}

register(new URL("./mcp-fixture-hooks.mjs", import.meta.url));

function writeFixture(name, data) {
  const path = join(OUT_DIR, name);
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, `${JSON.stringify(data, null, 2)}\n`, "utf-8");
  console.log(`[gen-fixtures] wrote ${name}`);
}

// ---------------------------------------------------------------- ts-shape

async function genTsShape() {
  const { renderTsShape } = await import(
    `${pathToFileURL(join(UPSTREAM, "ts-shape.ts")).href}`
  );
  // Cases: `__tests__/ts-shape.test.ts` verbatim + edge extras (alias
  // collisions, pointer escapes, parenthesized unions, non-string consts).
  const cases = [
    {
      name: "required-and-optional-object-properties",
      input: {
        type: "object",
        properties: { query: { type: "string" }, limit: { type: "integer" } },
        required: ["query"],
      },
    },
    { name: "enum-unions", input: { enum: ["fast", "safe", null] } },
    {
      name: "nested-objects-and-arrays",
      input: {
        type: "object",
        properties: {
          config: {
            type: "object",
            properties: { tags: { type: "array", items: { type: "string" } } },
            required: ["tags"],
          },
        },
        required: ["config"],
      },
    },
    {
      name: "hoists-local-references",
      input: {
        type: "object",
        properties: { address: { $ref: "#/$defs/Address" } },
        required: ["address"],
        $defs: {
          Address: { type: "object", properties: { city: { type: "string" } }, required: ["city"] },
        },
      },
    },
    {
      name: "ignores-unsupported-unreferenced-definitions",
      input: {
        type: "object",
        properties: { query: { type: "string" } },
        required: ["query"],
        $defs: { Unused: { if: { type: "string" }, then: { type: "number" } } },
      },
    },
    {
      name: "closed-objects-with-referenced-union-variants",
      input: {
        type: "object",
        additionalProperties: false,
        properties: { blocks: { type: "array", items: { $ref: "#/$defs/block" } } },
        required: ["blocks"],
        $defs: {
          text: {
            type: "object",
            additionalProperties: false,
            properties: { type: { const: "text" }, text: { type: "string" } },
            required: ["type", "text"],
          },
          qr: {
            type: "object",
            additionalProperties: false,
            properties: { type: { const: "qr" }, data: { type: "string" } },
            required: ["type", "data"],
          },
          block: { oneOf: [{ $ref: "#/$defs/text" }, { $ref: "#/$defs/qr" }] },
        },
      },
    },
    { name: "exotic-if-then", input: { if: { type: "string" }, then: { type: "number" } } },
    { name: "exotic-remote-ref", input: { $ref: "https://example.com/schema" } },
    // --- extras beyond the upstream test file ---
    { name: "const-string", input: { const: "draft" } },
    { name: "const-number", input: { const: 42 } },
    { name: "const-number-float", input: { const: 1.5 } },
    { name: "type-array-union", input: { type: ["string", "null"] } },
    { name: "type-unknown-scalar", input: { type: "function" } },
    { name: "no-type", input: { description: "free-form" } },
    { name: "empty-object", input: { type: "object" } },
    { name: "empty-properties", input: { type: "object", properties: {} } },
    { name: "array-without-items", input: { type: "array" } },
    { name: "array-of-union-parenthesized", input: { type: "array", items: { type: ["string", "number"] } } },
    { name: "non-identifier-property-names", input: {
      type: "object",
      properties: { "with-dash": { type: "string" }, "with space": { type: "number" }, ok: { type: "boolean" } },
    } },
    { name: "definitions-group-alias", input: {
      type: "object",
      properties: { a: { $ref: "#/definitions/thing" } },
      definitions: { thing: { type: "string" } },
    } },
    { name: "alias-collision-gets-generated-name", input: {
      type: "object",
      properties: { a: { $ref: "#/$defs/has-dash" }, b: { $ref: "#/$defs/has-dash2" } },
      $defs: {
        "has-dash": { type: "string" },
        "has-dash2": { type: "number" },
      },
    } },
    { name: "pointer-escape-tokens", input: {
      type: "object",
      properties: { a: { $ref: "#/$defs/weird~0name" } },
      $defs: { "weird~name": { type: "string" } },
    } },
    { name: "unreferenced-definitions-group-ignored", input: {
      type: "object",
      properties: { q: { type: "string" } },
      definitions: { Unused: { not: { type: "string" } } },
    } },
    { name: "additional-properties-true-unsupported", input: {
      type: "object",
      properties: { q: { type: "string" } },
      additionalProperties: true,
    } },
    { name: "pattern-properties-unsupported", input: {
      type: "object",
      patternProperties: { "^x-": { type: "string" } },
    } },
    { name: "enum-with-objects-unsupported", input: { enum: [{ a: 1 }] } },
    { name: "nested-ref-in-definition", input: {
      type: "object",
      properties: { outer: { $ref: "#/$defs/Outer" } },
      $defs: {
        Outer: { type: "object", properties: { inner: { $ref: "#/$defs/Inner" } } },
        Inner: { type: "string" },
      },
    } },
    { name: "null-root", input: null },
    { name: "array-root", input: [1, 2] },
    { name: "anyof-empty-unsupported", input: { anyOf: [] } },
    { name: "enum-number-values", input: { enum: [1, 2.5, true] } },
  ];
  const out = cases.map(({ name, input }) => ({
    name,
    input,
    expected: renderTsShape(input),
  }));
  writeFixture("tsshape_cases.json", {
    provenance: "external/pi-mcp-adapter/ts-shape.ts @ 3d953f90, executed by gen-mcp-adapter-fixtures.mjs",
    cases: out,
  });
}

// ----------------------------------------------------------- name formatting

async function genNames() {
  const types = await import(`${pathToFileURL(join(UPSTREAM, "types.ts")).href}`);
  const {
    formatToolName,
    getServerPrefix,
    resolveServerFromToolName,
    sanitizePromptName,
    formatPromptCommandName,
    getToolNameCandidates,
  } = types;

  const serverNames = [
    "searxng", "my server", "my_server", "my-server", "filesystem-mcp", "-mcp",
    "FooMCP", "xcodebuild", "日本語", "a b", "a-20-b", "with_underscore", "trailing-mcp-MCP",
  ];
  const toolNames = ["web_search", "list.sims", "simple", "with-dash", "with space", "日本語tool"];
  const prefixes = ["server", "none", "short", "mcp"];

  const formatToolNameCases = [];
  for (const server of serverNames) {
    for (const tool of toolNames) {
      for (const prefix of prefixes) {
        formatToolNameCases.push({ server, tool, prefix, expected: formatToolName(tool, server, prefix) });
      }
    }
  }
  const serverPrefixCases = [];
  for (const server of serverNames) {
    for (const prefix of prefixes) {
      serverPrefixCases.push({ server, prefix, expected: getServerPrefix(server, prefix) });
    }
  }
  const candidateCases = [];
  for (const server of ["demo", "my server"]) {
    for (const tool of ["search", "list.sims"]) {
      for (const prefix of prefixes) {
        candidateCases.push({ server, tool, prefix, expected: [...getToolNameCandidates(tool, server, prefix)] });
      }
    }
  }

  const resolveCases = [
    { tool: "searxng_searxng_web_search", servers: ["searxng"], prefix: "server" },
    { tool: "my_20_server_web_search", servers: ["my server", "my_server"], prefix: "server" },
    { tool: "my_5f_server_search", servers: ["my server", "my_server"], prefix: "server" },
    { tool: "github_create_issue", servers: ["searxng", "github"], prefix: "server" },
    { tool: "searxng_2d_extra_deep_search", servers: ["searxng", "searxng-extra"], prefix: "server" },
    { tool: "filesystem_read_file", servers: ["filesystem-mcp"], prefix: "short" },
    { tool: "mcp_query", servers: ["-mcp"], prefix: "short" },
    { tool: "foo_query", servers: ["foo", "foo-mcp"], prefix: "short" },
    { tool: "mcp__my_2d_server_run", servers: ["my-server"], prefix: "mcp" },
    { tool: "searxng_web_search", servers: ["searxng"], prefix: "none" },
    { tool: "unknown_tool", servers: ["searxng", "github"], prefix: "server" },
    { tool: "web_search", servers: ["searxng"], prefix: "server" },
    { tool: "searxng_web_search", servers: [], prefix: "server" },
    { tool: "notsearxng_search", servers: ["searxng"], prefix: "server" },
    { tool: "searxngweb_search", servers: ["searxng"], prefix: "server" },
    { tool: "web_search", servers: ["noisy", "searxng"], prefix: "server" },
    { tool: "a_20_b_run", servers: ["a b", "a-20-b"], prefix: "server" },
    { tool: "a_2d_20_2d_b_run", servers: ["a b", "a-20-b"], prefix: "server" },
    { tool: "mcp__my_2d_server_run", servers: ["my-server", "my_server"], prefix: "mcp" },
    { tool: "mcp__my_5f_server_run", servers: ["my-server", "my_server"], prefix: "mcp" },
  ].map(({ tool, servers, prefix }) => ({
    tool, servers, prefix,
    expected: resolveServerFromToolName(tool, servers, prefix) ?? null,
  }));

  const promptNameCases = ["plan", "my prompt!", "   ", "123go", "_lead", "__a--b__", "a.b.c"].map((input) => ({
    input,
    expected: sanitizePromptName(input),
  }));

  // FR-P1-02: include/excludeTools glob matching (types.ts
  // matchesToolPattern / isToolIncluded / isToolExcluded / isToolAllowed).
  const { isToolAllowed } = types;
  const toolAllowedCases = [
    { tool: "search_records", server: "demo", prefix: "server", include: ["search_*"], exclude: undefined },
    { tool: "search_records", server: "demo", prefix: "server", include: ["demo_search_records"], exclude: undefined },
    { tool: "search_records", server: "demo", prefix: "server", include: ["other_*"], exclude: undefined },
    { tool: "search_records", server: "demo", prefix: "server", include: undefined, exclude: ["*_records"] },
    { tool: "search_records", server: "demo", prefix: "server", include: ["*"], exclude: ["search_*"] },
    { tool: "search_records", server: "demo", prefix: "server", include: ["search-records"], exclude: undefined },
    { tool: "list.sims", server: "xcodebuild", prefix: "server", include: ["xcodebuild_list?ims"], exclude: undefined },
    { tool: "list.sims", server: "xcodebuild", prefix: "short", include: ["xcodebuild_list_sims"], exclude: undefined },
    { tool: "list.sims", server: "xcodebuild", prefix: "mcp", include: ["mcp__xcodebuild_list_sims"], exclude: undefined },
    { tool: "list.sims", server: "xcodebuild", prefix: "none", include: ["list_sims"], exclude: undefined },
    { tool: "a", server: "s", prefix: "server", include: [], exclude: undefined },
    { tool: "a", server: "s", prefix: "server", include: "not-an-array", exclude: undefined },
    { tool: "a", server: "s", prefix: "server", include: [42, "a"], exclude: undefined },
    { tool: "a", server: "s", prefix: "server", include: ["*"], exclude: ["s_a"] },
  ].map((c) => ({
    ...c,
    expected: isToolAllowed(c.tool, c.server, c.prefix, c.include, c.exclude),
  }));
  const promptCommandCases = [
    { prompt: "plan", server: "my server", prefix: "server" },
    { prompt: "plan", server: "my_server", prefix: "server" },
    { prompt: "Do Thing!", server: "srv", prefix: "none" },
  ].map(({ prompt, server, prefix }) => ({
    prompt, server, prefix,
    expected: formatPromptCommandName(prompt, server, prefix),
  }));

  writeFixture("name_format_cases.json", {
    provenance: "external/pi-mcp-adapter/types.ts @ 3d953f90, executed by gen-mcp-adapter-fixtures.mjs",
    formatToolName: formatToolNameCases,
    getServerPrefix: serverPrefixCases,
    getToolNameCandidates: candidateCases,
    resolveServerFromToolName: resolveCases,
    sanitizePromptName: promptNameCases,
    formatPromptCommandName: promptCommandCases,
    isToolAllowed: toolAllowedCases,
  });
}

// ------------------------------------------------------------------- search

async function genSearch() {
  const { scoreToolMatch, rankToolMatches, paginate, rankSuggestions } = await import(
    `${pathToFileURL(join(UPSTREAM, "search-ranking.ts")).href}`
  );

  const tool = (name, description) => ({ name, originalName: name, description });
  const servers = [
    {
      name: "demo",
      definition: {
        command: "npx",
        searchKeywords: {
          "search_*": ["records", "fuzzy lookup"],
          search_records_advanced: ["fuzzy lookup", "legacy"],
        },
      },
      tools: [
        tool("search_records", "Find records"),
        tool("find_records", "Search records"),
        tool("search_records_advanced", "Advanced record search with filters"),
        tool("record_search", "Fuzzy lookup across records"),
      ],
    },
    {
      name: "better-icons",
      definition: { command: "npx" },
      tools: [tool("sync_icon", "Add an icon to your project's icons file.")],
    },
    {
      name: "disabled-srv",
      definition: { command: "npx", disabled: true },
      tools: [tool("hidden_tool", "Should never rank")],
    },
  ];

  const config = {
    mcpServers: Object.fromEntries(servers.map((s) => [s.name, s.definition])),
    settings: {},
  };
  const toolMetadata = new Map(servers.map((s) => [s.name, s.tools]));
  const state = { config, toolMetadata };

  const queries = [
    "search", "search missing", "simulator", "synchronize", "fuzzy lookup",
    "lookup legacy", "fuzzy", "advanced", "records", "icon", "sync",
  ];
  const rankCases = [];
  for (const query of queries) {
    for (const includeKeywords of [true, false]) {
      rankCases.push({
        query,
        includeKeywords,
        expected: rankToolMatches(state, query, undefined, includeKeywords).map((m) => ({
          server: m.server,
          tool: m.tool.name,
          score: m.score,
        })),
      });
    }
  }
  rankCases.push({
    query: "search",
    server: "demo",
    includeKeywords: true,
    expected: rankToolMatches(state, "search", "demo", true).map((m) => ({
      server: m.server, tool: m.tool.name, score: m.score,
    })),
  });

  const scoreCases = [];
  for (const query of queries.concat(["", "  "])) {
    for (const s of servers) {
      for (const t of s.tools) {
        const keywords = s.definition.searchKeywords
          ? (await import(`${pathToFileURL(join(UPSTREAM, "search-ranking.ts")).href}`))
              .resolveSearchKeywords(s.definition, t.originalName, s.name, "server")
          : undefined;
        scoreCases.push({
          server: s.name,
          tool: t.name,
          description: t.description,
          query,
          keywords,
          expected: scoreToolMatch(t, s.name, query, keywords),
        });
      }
    }
  }

  const paginateCases = [
    { items: ["a", "b", "c"], offset: 1, limit: 1 },
    { items: ["a", "b", "c"], offset: 5, limit: 1 },
    { items: ["a", "b", "c"], offset: 0, limit: 12 },
    { items: ["a", "b", "c"], offset: 2, limit: 5 },
    { items: [], offset: 0, limit: 12 },
  ].map(({ items, offset, limit }) => ({ items, offset, limit, expected: paginate(items, offset, limit) }));

  const suggestionCases = [
    { name: "demo_search_records", limit: 5 },
    { name: "demo_serch", limit: 5 },
    { name: "search_records", limit: 5 },
    { name: "nonexistent", limit: 5 },
    { name: "sync_icon", limit: 2 },
  ].map(({ name, limit }) => ({ name, limit, expected: rankSuggestions(state, name, limit) }));

  writeFixture("search_cases.json", {
    provenance: "external/pi-mcp-adapter/search-ranking.ts @ 3d953f90, executed by gen-mcp-adapter-fixtures.mjs",
    state: {
      servers: servers.map((s) => ({
        name: s.name,
        definition: s.definition,
        tools: s.tools,
      })),
      globalPrefix: "server",
    },
    rank: rankCases,
    score: scoreCases,
    paginate: paginateCases,
    suggestions: suggestionCases,
  });
}

// ------------------------------------------------------------- config merge

// Layer -> upstream path mapping (the rpi side maps `pi-global` to
// `~/.rpi/agent/mcp.json` and `pi-project` to `<cwd>/.rpi/mcp.json`, ADR-0001;
// the merge semantics under test are identical).
const LAYERS = {
  "shared-global": (home, proj) => join(home, ".config", "mcp", "mcp.json"),
  "agents-global": (home) => join(home, ".agents", "mcp.json"),
  "agents-nested-global": (home) => join(home, ".agents", "mcp", "mcp.json"),
  "pi-global": (home) => join(home, ".pi", "agent", "mcp.json"),
  "shared-project": (_home, proj) => join(proj, ".mcp.json"),
  "pi-project": (_home, proj) => join(proj, ".pi", "mcp.json"),
};

const URL_A = "https://litellm.internal/mcp/";
const URL_B = "https://attacker.example/mcp/";

const CONFIG_CASES = [
  {
    name: "six-source-fold",
    layers: {
      "shared-global": {
        settings: { idleTimeout: 5, requestTimeoutMs: 1500, showStatusIcon: true },
        mcpServers: { shared: { command: "generic" }, genericOnly: { command: "generic-only" } },
      },
      "agents-global": {
        mcpServers: { shared: { command: "agents-flat" }, agentsFlatOnly: { command: "agents-flat-only" } },
      },
      "agents-nested-global": {
        mcpServers: { shared: { command: "agents-nested" } },
      },
      "pi-global": {
        settings: { toolPrefix: "short", directTools: true },
        mcpServers: { shared: { command: "pi-global" }, piOnly: { command: "pi-only" } },
      },
      "shared-project": {
        settings: { toolPrefix: "none", oauthDir: "shared-oauth" },
        mcpServers: { shared: { command: "project" }, projectOnly: { command: "project-only" } },
      },
      "pi-project": {
        settings: { autoAuth: true, oauthDir: ".pi/oauth", showStatusIcon: false },
        mcpServers: { shared: { command: "project-pi" }, projectPiOnly: { command: "project-pi-only" } },
      },
    },
  },
  {
    name: "same-url-keeps-inherited-auth",
    layers: {
      "shared-global": { mcpServers: { litellm: { url: URL_A, headers: { Authorization: "Bearer secret-vk" } } } },
      "pi-global": { mcpServers: { litellm: { url: URL_A, directTools: true } } },
    },
  },
  {
    name: "url-change-strips-credentials",
    layers: {
      "shared-global": { mcpServers: { litellm: {
        url: URL_A,
        headers: { Authorization: "Bearer ${RPI_MCP_FIXTURE_UNUSED}" },
        bearerTokenEnv: "RPI_MCP_FIXTURE_UNUSED",
        bearerToken: "secret-bearer-token",
        oauth: { clientId: "client", clientSecret: "oauth-client-secret" },
      } } },
      "pi-global": { mcpServers: { litellm: { url: URL_B } } },
    },
  },
  {
    name: "url-change-keeps-only-override-auth",
    layers: {
      "shared-global": { mcpServers: { litellm: { url: URL_A, headers: { Authorization: "Bearer secret-vk" } } } },
      "pi-global": { mcpServers: { litellm: { url: URL_B, headers: { Authorization: "Bearer override-token" } } } },
    },
  },
  {
    name: "url-change-preserves-oauth-false",
    layers: {
      "shared-global": { mcpServers: { litellm: { url: URL_A, oauth: false } } },
      "pi-global": { mcpServers: { litellm: { url: URL_B } } },
    },
  },
  {
    name: "three-source-laundering-prevented",
    layers: {
      "shared-global": { mcpServers: { litellm: { url: URL_A, headers: { Authorization: "Bearer secret-vk" } } } },
      "pi-global": { mcpServers: { litellm: { headers: { Authorization: "Bearer secret-vk" } } } },
      "shared-project": { mcpServers: { litellm: { url: URL_B } } },
    },
  },
  {
    name: "socket-transport-switch",
    layers: {
      "shared-global": { mcpServers: {
        toSocket: { command: "old", args: ["--old"], env: { OLD: "1" }, cwd: "/old" },
        toCommand: { socket: "/old.sock" },
        toUrl: { socket: "/old.sock" },
      } },
      "pi-global": { mcpServers: {
        toSocket: { socket: "/shared.sock" },
        toCommand: { command: "new" },
        toUrl: { url: "https://example.test/mcp" },
      } },
    },
  },
  {
    name: "malformed-entries-dropped",
    layers: {
      "shared-project": { mcpServers: {
        valid: { command: "node" },
        nullEntry: null,
        listEntry: [],
        stringEntry: "node",
      } },
    },
  },
  {
    name: "legacy-mcp-servers-key",
    layers: {
      "shared-project": { "mcp-servers": { legacy: { command: "x" } } },
    },
  },
];

if (haveRealStrip) {
  CONFIG_CASES.push({
    name: "jsonc-comments-and-trailing-commas",
    rawLayers: {
      "pi-global": `{
        // leading comment
        "imports": ["vscode",],
        "mcpServers": {
          "global": { "command": "global-server", },
        },
      }`,
      "shared-project": `{
        /* block
           comment */
        "mcpServers": {
          "project": { "command": "project-server", },
        },
      }`,
    },
  });
}

async function genConfigMerge() {
  const cases = [];
  for (const testCase of CONFIG_CASES) {
    const sandbox = mkdtempSync(join(tmpdir(), "rpi-mcp-fixture-"));
    const home = join(sandbox, "home");
    const proj = join(sandbox, "proj");
    mkdirSync(home, { recursive: true });
    mkdirSync(proj, { recursive: true });

    const layers = testCase.layers ?? {};
    for (const [layer, content] of Object.entries(layers)) {
      const path = LAYERS[layer](home, proj);
      mkdirSync(dirname(path), { recursive: true });
      writeFileSync(path, `${JSON.stringify(content, null, 2)}\n`, "utf-8");
    }
    const rawLayers = testCase.rawLayers ?? {};
    for (const [layer, raw] of Object.entries(rawLayers)) {
      const path = LAYERS[layer](home, proj);
      mkdirSync(dirname(path), { recursive: true });
      writeFileSync(path, raw, "utf-8");
    }

    process.env.HOME = home;
    delete process.env.PI_CODING_AGENT_DIR;
    // Fresh module graph per case: config.ts computes its path constants at
    // module load from homedir().
    const configModule = await import(
      `${pathToFileURL(join(UPSTREAM, "config.ts")).href}?case=${testCase.name}`
    );
    const merged = configModule.loadMcpConfig(undefined, proj);

    cases.push({
      name: testCase.name,
      layers: testCase.layers ?? {},
      rawLayers: testCase.rawLayers ?? {},
      expected: {
        mcpServers: merged.mcpServers,
        ...(merged.settings !== undefined ? { settings: merged.settings } : {}),
        ...(merged.imports !== undefined ? { imports: merged.imports } : {}),
      },
    });
    rmSync(sandbox, { recursive: true, force: true });
  }
  writeFixture("config_merge_cases.json", {
    provenance: "external/pi-mcp-adapter/config.ts loadMcpConfig @ 3d953f90, executed by gen-mcp-adapter-fixtures.mjs",
    note: "imports are expanded by upstream loadMcpConfig but NOT by the P0 rpi port [VARIANT]; no fixture case uses imports-dependent servers.",
    cases,
  });
}

// ------------------------------------------------------------- config hash

async function genConfigHash() {
  const { computeServerHash } = await import(
    `${pathToFileURL(join(UPSTREAM, "metadata-cache.ts")).href}`
  );

  const cases = [
    {
      name: "stdio-basic",
      entry: { command: "node", args: ["server.js", "--flag"] },
    },
    {
      name: "env-interpolation",
      env: { RPI_MCP_FIXTURE_TOKEN: "secret-1" },
      entry: { command: "node", env: { TOKEN: "$env:RPI_MCP_FIXTURE_TOKEN", PLAIN: "x" } },
    },
    {
      name: "env-brace-forms",
      env: { RPI_MCP_FIXTURE_TOKEN: "secret-1" },
      entry: { command: "node", env: { A: "${RPI_MCP_FIXTURE_TOKEN}", B: "{env:RPI_MCP_FIXTURE_TOKEN}" } },
    },
    {
      name: "http-headers-interpolation",
      env: { RPI_MCP_FIXTURE_TOKEN: "secret-1" },
      entry: { url: "https://example.test/mcp", headers: { Authorization: "Bearer ${RPI_MCP_FIXTURE_TOKEN}" } },
    },
    {
      name: "bearer-token-env",
      env: { RPI_MCP_FIXTURE_TOKEN: "secret-1" },
      entry: { url: "https://example.test/mcp", bearerTokenEnv: "RPI_MCP_FIXTURE_TOKEN" },
    },
    {
      name: "bearer-literal-escaped-bang",
      entry: { url: "https://example.test/mcp", bearerToken: "!!not-a-cmd" },
    },
    {
      name: "bearer-literal-with-interpolation",
      env: { RPI_MCP_FIXTURE_TOKEN: "secret-1" },
      entry: { url: "https://example.test/mcp", bearerToken: "tok-${RPI_MCP_FIXTURE_TOKEN}" },
    },
    {
      name: "socket-tilde-expansion",
      env: { HOME: "/home/mcpfixture" },
      entry: { socket: "~/mcp.sock" },
    },
    {
      name: "cwd-relative-passthrough",
      entry: { command: "node", cwd: "relative/dir" },
    },
    {
      name: "tool-filters",
      entry: { command: "node", includeTools: ["a", "b"], excludeTools: ["c"], exposeResources: false },
    },
    {
      name: "protocol-and-oauth-fields",
      entry: {
        url: "https://example.test/mcp",
        protocolVersion: "auto",
        auth: "oauth",
        oauth: { clientId: "x" },
      },
    },
    {
      name: "runtime-fields-ignored-a",
      entry: { command: "node", lifecycle: "eager", idleTimeout: 0, debug: true, requestTimeoutMs: 500 },
      expectSameAs: "runtime-fields-ignored-b",
    },
    {
      name: "runtime-fields-ignored-b",
      entry: { command: "node" },
    },
    {
      name: "command-secret-marker-left-literal",
      entry: { command: "node", env: { TOKEN: "!echo hi" } },
    },
    {
      name: "url-env-interpolation",
      env: { RPI_MCP_FIXTURE_HOST: "example.test" },
      entry: { url: "https://${RPI_MCP_FIXTURE_HOST}/mcp" },
    },
    {
      name: "unset-env-expands-empty",
      entry: { command: "node", env: { MISSING: "$env:RPI_MCP_FIXTURE_DEFINITELY_UNSET" } },
    },
  ];

  const savedEnv = { ...process.env };
  const out = [];
  try {
    for (const testCase of cases) {
      // Reset to a clean slate, then apply the case env.
      for (const key of Object.keys(process.env)) {
        if (key.startsWith("RPI_MCP_FIXTURE_")) delete process.env[key];
      }
      if (testCase.env?.HOME) {
        process.env.HOME = testCase.env.HOME;
      } else {
        process.env.HOME = savedEnv.HOME ?? "/nonexistent";
      }
      for (const [key, value] of Object.entries(testCase.env ?? {})) {
        if (key !== "HOME") process.env[key] = value;
      }
      out.push({
        name: testCase.name,
        env: testCase.env ?? {},
        entry: testCase.entry,
        expectedHash: computeServerHash(testCase.entry),
        ...(testCase.expectSameAs ? { expectSameAs: testCase.expectSameAs } : {}),
      });
    }
  } finally {
    process.env = savedEnv;
  }
  writeFixture("config_hash_cases.json", {
    provenance: "external/pi-mcp-adapter/metadata-cache.ts computeServerHash @ 3d953f90, executed by gen-mcp-adapter-fixtures.mjs",
    cases: out,
  });
}

// ----------------------------------------------------------------- glob/filter

async function genGlob() {
  const { matchesToolPattern, isToolAllowed, getToolNameCandidates, resolveSearchKeywords } = await import(
    `${pathToFileURL(join(UPSTREAM, "types.ts")).href}`
  );
  const { resolveSearchKeywords: resolveSearchKeywordsRanked } = await import(
    `${pathToFileURL(join(UPSTREAM, "search-ranking.ts")).href}`
  ).catch(() => ({}));

  const servers = [
    { name: "github", toolName: "search_prs", prefix: "server" },
    { name: "filesystem", toolName: "read_file", prefix: "short" },
    { name: "demo-server", toolName: "advanced.query", prefix: "mcp" },
  ];

  const matchCases = [];
  for (const { name: server, toolName, prefix } of servers) {
    const candidates = getToolNameCandidates(toolName, server, prefix);
    const patterns = [
      [],
      ["*"],
      ["*_prs"],
      ["search_*"],
      ["*file*"],
      ["github_*"],
      [`${toolName}`],
      ["nonexistent"],
      ["???_*"],
      ["*.query"],
    ];
    for (const pattern of patterns) {
      matchCases.push({
        server,
        toolName,
        prefix,
        pattern,
        expected: matchesToolPattern(candidates, pattern),
        candidates: [...candidates],
      });
    }
  }

  const allowedCases = [];
  const includeExcludeCases = [
    { include: undefined, exclude: undefined },
    { include: ["search_*"], exclude: undefined },
    { include: ["*"], exclude: ["*_prs"] },
    { include: undefined, exclude: ["read_*"] },
    { include: ["github_search_prs"], exclude: [] },
    { include: ["other"], exclude: undefined },
  ];
  for (const { name: server, toolName, prefix } of servers) {
    for (const { include, exclude } of includeExcludeCases) {
      allowedCases.push({
        server,
        toolName,
        prefix,
        include,
        exclude,
        expected: isToolAllowed(toolName, server, prefix, include, exclude),
      });
    }
  }

  const keywordCases = [];
  const definitionWithKeywords = {
    searchKeywords: {
      "search_*": ["find records", "lookup"],
      "advanced_*": ["fuzzy search"],
      "nonmatch": ["should not appear"],
    },
  };
  for (const { name: server, toolName, prefix } of servers) {
    const keywords = resolveSearchKeywordsRanked
      ? resolveSearchKeywordsRanked(definitionWithKeywords, toolName, server, prefix)
      : resolveSearchKeywords(definitionWithKeywords, toolName, server, prefix);
    keywordCases.push({
      server,
      toolName,
      prefix,
      definition: definitionWithKeywords,
      expected: [...keywords],
    });
  }

  writeFixture("glob_cases.json", {
    provenance: "external/pi-mcp-adapter/types.ts matchesToolPattern/isToolAllowed/getToolNameCandidates + search-ranking.ts resolveSearchKeywords @ 3d953f90, executed by gen-mcp-adapter-fixtures.mjs",
    matches: matchCases,
    allowed: allowedCases,
    keywords: keywordCases,
  });
}

await genTsShape();
await genNames();
await genSearch();
await genConfigMerge();
await genConfigHash();
await genGlob();
console.log("[gen-fixtures] done");
