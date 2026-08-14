// Module resolution hooks for the parity drivers
// (scripts/mcp-parity/upstream-*.mjs).
//
// The pinned upstream (`external/pi-mcp-adapter` @ 3d953f90) is imported
// directly and its bare imports are resolved against an OUT-OF-TREE
// dependency install (see scripts/mcp-parity/README.md) — nothing is ever
// written into `external/` (G4 red line).
//
// Host-packages only referenced with `import type` (earendil-works/*) are
// mapped to a throwing stub: tsx erases type-only imports, so the stub is
// only reached if a future driver accidentally needs them at runtime.

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
  ["@earendil-works/pi-tui", STUB],
]);

export function resolve(specifier, context, nextResolve) {
  const stub = BARE_TO_STUB.get(specifier);
  if (stub) {
    return { url: stub, shortCircuit: true };
  }
  // Resolve bare specifiers from the out-of-tree install: give the default
  // resolver a parent URL inside that tree (a file URL — directory URLs
  // break the node_modules walk).
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
