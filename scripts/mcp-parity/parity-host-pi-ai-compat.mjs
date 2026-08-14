// Runtime stub for `@earendil-works/pi-ai/compat` — upstream
// sampling-handler.ts value-imports `complete`. The parity drivers never
// register a sampling config, so the import binding is only referenced
// inside `registerSamplingHandler`; reaching this throw means a driver
// scenario strayed into sampling (out of parity scope).

export const complete = () => {
  throw new Error(
    "parity-host-stub: sampling complete() reached — outside the parity scenario set",
  );
};
