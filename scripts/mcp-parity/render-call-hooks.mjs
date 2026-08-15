// Module resolution hooks for the renderCall parity leg (TE09 FR-E). Same
// out-of-tree dependency resolution as parity-hooks.mjs, except
// `@earendil-works/pi-tui` maps to the minimal value stub (Text) instead of
// the throwing one — the render functions construct components at runtime.

import { pathToFileURL } from "node:url";
import { join } from "node:path";

const DEPS = process.env.RPI_MCP_PARITY_DEPS;
if (!DEPS) {
  console.error(
    "RPI_MCP_PARITY_DEPS=<dir> must point at the out-of-tree node_modules root (see scripts/mcp-parity/README.md)",
  );
  process.exit(1);
}

const STUB = new URL("./parity-host-stub.mjs", import.meta.url).href;

const BARE_TO_STUB = new Map([
  ["@earendil-works/pi-ai/compat", new URL("./parity-host-pi-ai-compat.mjs", import.meta.url).href],
  ["@earendil-works/pi-ai", STUB],
  ["@earendil-works/pi-coding-agent", STUB],
  ["@earendil-works/pi-tui", new URL("./render-call-host-pi-tui.mjs", import.meta.url).href],
]);

export function resolve(specifier, context, nextResolve) {
  const stub = BARE_TO_STUB.get(specifier);
  if (stub) {
    return { url: stub, shortCircuit: true };
  }
  if (!specifier.startsWith(".") && !specifier.startsWith("/")) {
    if (specifier.startsWith("node:") || specifier === "http" || specifier === "https") {
      return nextResolve(specifier, context);
    }
    return nextResolve(specifier, {
      ...context,
      parentURL: pathToFileURL(join(DEPS, "package.json")).href,
    });
  }
  return nextResolve(specifier, context);
}
