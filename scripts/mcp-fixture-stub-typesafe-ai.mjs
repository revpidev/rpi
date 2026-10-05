// Minimal stub for the Jev TypeSafe SDK (`@typesafe-ai/sdk`) used by the
// fixture generator (scripts/gen-mcp-adapter-fixtures.mjs). The upstream
// `config.ts` pulls `jev-client.ts` in for `validateJevSettings`; the SDK
// itself is only instantiated when a Jev evaluation runs, which the pure
// config/search fixtures never do. Keep the import surface (class + default
// export) so module evaluation succeeds.
export class TypeSafeClient {
  constructor() {}
}

export default { TypeSafeClient };