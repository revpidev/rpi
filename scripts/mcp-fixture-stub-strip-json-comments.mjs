// Passthrough fallback for `strip-json-comments` when the real package is
// not installed (see gen-mcp-adapter-fixtures.mjs header). Only usable for
// comment-free fixture inputs; the generator skips JSONC cases in this mode.

export default function stripJsonComments(text) {
  return text;
}
