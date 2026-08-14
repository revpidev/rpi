// Chainable proxy standing in for `zod` (see mcp-fixture-hooks.mjs). The
// upstream modules reached by the fixture generator only build UI-stream
// schemas at module scope; the schemas themselves are never used.

const target = function () {};
const z = new Proxy(target, {
  get: (_t, prop) => {
    if (prop === Symbol.toPrimitive) return () => 0;
    if (prop === "then") return undefined; // never look like a thenable
    return z;
  },
  apply: () => z,
  construct: () => z,
});

export { z };
export default z;
