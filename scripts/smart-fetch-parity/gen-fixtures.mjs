// TE06 parity fixture generator: drives the PINNED upstream
// (agent-smart-fetch v0.3.17 / b0111612) through golden inputs and records
// input/output pairs for byte-level comparison against the Rust port
// (rpi-ext-smart-fetch).
//
// Run:  node_modules/.bin/tsx scripts/smart-fetch-parity/gen-fixtures.mjs
// Deps: external, installed under /tmp/rpi-smart-fetch-parity-deps
//       (tsx + the upstream runtime deps; see scripts/smart-fetch-parity/README.md).
//
// Upstream-private functions (formatByteCount, the error builders, the DOM
// fallback chain, download filename derivation) are exercised through their
// exported callers: buildFetchErrorResponseText, createDefuddleFetch with a
// mocked transport/defuddle — the same trick the upstream unit tests use.

import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";

// The pinned upstream sources resolve their bare imports (typebox, linkedom,
// …) from a staging tree under /tmp — external/ must stay pristine (G4).
// Staging layout: <deps>/node_modules + <deps>/core-src + <deps>/pi-src
// (copies, NOT symlinks: node realpaths symlinks and would resolve bare
// imports back inside external/). run.sh re-copies before generating.
const STAGING = process.env.RPI_SF_STAGING ?? "/tmp/rpi-smart-fetch-parity-deps";

const format = await import(join(STAGING, "core-src/format.ts"));
const tool = await import(join(STAGING, "core-src/tool.ts"));
const settings = await import(join(STAGING, "pi-src/settings.ts"));
const extract = await import(join(STAGING, "core-src/extract.ts"));

const OUT_DIR = new URL("../../fixtures/generated/smart-fetch-parity/", import.meta.url)
  .pathname;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

const record = (name, input, output) => ({ name, input, output });

// JS truncation can split a surrogate pair, leaving a lone surrogate that
// JSON.stringify escapes as \udXXX — unrepresentable in Rust Strings (the
// declared FR-P0-9 [VARIANT]). Replace lone surrogates with U+FFFD so the
// fixtures stay parseable while recording the drift.
const sanitizeLoneSurrogates = (text) =>
  typeof text === "string" ? text.replace(/(?:[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF])/g, "\uFFFD") : text;
const j = (value) => JSON.stringify(value);

function run(functionName, cases, fn) {
  return cases.map(({ name, input }) => record(name, input, fn(input)));
}

// A FetchResponseLike built from plain data (upstream unit-test shape).
// `stream: true` wraps the body in a ReadableStream so the TE07 download
// branch (`streamResponseToFile`'s getReader path) runs for real instead of
// tripping the old "not used in parity scenarios" readable() stub.
function makeResponse({ status = 200, statusText = "OK", url, headers = {}, body = "", stream = false, onBodyRead }) {
  const headerMap = new Map(Object.entries(headers));
  const bytes = new TextEncoder().encode(body);
  const fireBodyRead = onBodyRead ?? (() => {});
  const realStream = new ReadableStream({
    start(controller) {
      controller.enqueue(bytes);
      controller.close();
    },
  });
  return {
    ok: status >= 200 && status < 300,
    status,
    statusText,
    url,
    headers: { get: (name) => headerMap.get(name.toLowerCase()) ?? null },
    // The body stream proxies getReader so the synthesized engine events
    // fire at consumption time (the real wreq-js stream emits body_progress/
    // body_complete as the body is read, not when fetch() resolves).
    body: stream
      ? {
          getReader() {
            fireBodyRead();
            return realStream.getReader();
          },
          get locked() {
            return realStream.locked;
          },
        }
      : null,
    text: async () => {
      fireBodyRead();
      return body;
    },
    arrayBuffer: async () => {
      fireBodyRead();
      return bytes.buffer;
    },
    readable: () => {
      throw new Error("not used in parity scenarios");
    },
  };
}

// The synthesized engine-event pair both drives share (TE-D27 alignment):
// response_headers at fetch() resolve, body_progress/body_complete when the
// body is consumed. body_progress only fires with a content-length.
function engineEventBridge(options, step, url) {
  const headerMap = new Map(
    Object.entries({ ...(step.headers ?? {}) }).map(([k, v]) => [k.toLowerCase(), v]),
  );
  const emit = (event) => options?.onRequestEvent?.(event);
  const contentLengthRaw = headerMap.get("content-length");
  const contentLength = contentLengthRaw ? Number(contentLengthRaw) : null;
  if (typeof options?.onRequestEvent !== "function") {
    return { fireBodyRead: () => {} };
  }
  emit({
    type: "response_headers",
    url,
    status: step.status ?? 200,
    contentLength,
  });
  return {
    fireBodyRead: () => {
      if (contentLength && contentLength > 0 && step.body) {
        emit({
          type: "body_progress",
          contentLength,
          downloadedBytes: new TextEncoder().encode(step.body).length,
        });
      }
      emit({ type: "body_complete" });
    },
  };
}

// Fixed download directory for the file-result scenarios: both the generator
// (upstream) and the Rust replay clear it before running, so the EEXIST
// retry path stays out of the deterministic fixtures.
const DOWNLOAD_TEMP_DIR = "/tmp/smart-fetch-parity-downloads";

// Drive `defuddleFetch` with a scripted transport + defuddle and return the
// raw FetchResult/FetchError JSON plus the request options the transport saw.
// TE08: also collects the FetchExecutionHooks event stream (status/progress
// pairs, in fire order) — the mock synthesizes the engine `onRequestEvent`
// frames the rpi pipeline emits at its own positions (TE-D27 alignment:
// response arrival ≈ response_headers, buffered read end ≈ body_complete;
// body_progress only when a content-length header exists — download path).
async function drivePipeline({ fetchScript, defuddleResult, opts }) {
  const captured = { calls: [] };
  const progressEvents = [];
  // work on a copy: the scenario's fetchScript stays intact for the fixture
  const script = [...fetchScript];
  const fetchDependency = async (url, options) => {
    captured.calls.push({ url, options });
    const step = script.shift();
    if (!step) throw new Error("unexpected extra fetch call");
    if (step.throw) {
      const error = new Error(step.throw.message);
      if (step.throw.name) error.name = step.throw.name;
      throw error;
    }
    const { fireBodyRead } = engineEventBridge(options, step, url);
    return makeResponse({ url, ...step, onBodyRead: fireBodyRead });
  };
  const defuddleDependency = async () =>
    defuddleResult ?? { content: undefined, wordCount: 0 };

  const defuddleFetch = extract.createDefuddleFetch({
    fetch: fetchDependency,
    defuddle: defuddleDependency,
    getProfiles: () => ["chrome_145"],
  });
  const hooks = {
    onStatusChange(status) {
      progressEvents.push({ kind: "status", status });
    },
    onProgressChange(update) {
      progressEvents.push({
        kind: "progress",
        status: update.status,
        progress: update.progress,
        phase: update.phase ?? null,
      });
    },
  };
  const result = await defuddleFetch(opts, hooks);
  return { result, captured, progressEvents };
}

// ---------------------------------------------------------------------------
// A. format.ts pure functions (exported surface)
// ---------------------------------------------------------------------------

const markdownToTextCases = [
  { name: "headings", input: "# Title\n## Sub\nnormal" },
  { name: "emphasis", input: "**bold** and *ital* alone" },
  { name: "links-and-images", input: "[text](https://x) and ![alt](img.png)" },
  { name: "image-only-link-precedence", input: "a [l](u) ![img](y) b" },
  { name: "quotes", input: "> quoted line\nnormal" },
  { name: "lists", input: "- dash\n* star\n+ plus\n1. ordered" },
  { name: "inline-code", input: "use `cargo test` now" },
  { name: "multiline", input: "# H\n\npara one\n\n- a\n- b\n\n> q\n\n**x**" },
  { name: "no-markup", input: "plain text  with  spaces" },
  { name: "empty", input: "" },
];

const truncateCases = [
  { name: "under", input: { content: "abc", maxChars: 10 } },
  { name: "exact", input: { content: "abcde", maxChars: 5 } },
  { name: "over", input: { content: "abcdef", maxChars: 3 } },
  { name: "ascii-mid-word", input: { content: "0123456789", maxChars: 4 } },
  { name: "astral-bmp-mix", input: { content: "a😀b😀c😀d😀", maxChars: 4 } },
  { name: "astral-count-variant", input: { content: "😀😀😀😀", maxChars: 3 } },
  { name: "newline-boundary", input: { content: "line1\nline2\nline3", maxChars: 7 } },
];

const wordCountCases = [
  { name: "simple", input: "one two three" },
  { name: "padded", input: "   spaces   everywhere   " },
  { name: "tabs-newlines", input: "a\tb\nc\r\nd" },
  { name: "empty", input: "" },
  { name: "whitespace-only", input: " \n\t " },
  { name: "unicode-words", input: "日本語 テキスト 対拍" },
];

const byteCountCases = [
  0, 1, 512, 999, 1023, 1024, 1152, 1536, 10240, 999_999, 1_048_576, 1_572_864,
  1_073_741_824, 1_099_511_627_776,
].map((bytes) => ({ name: `bytes-${bytes}`, input: bytes }));

const escapeHtmlCases = [
  { name: "all-escapes", input: `<a href="x">&'</a>` },
  { name: "clean", input: "plain" },
  { name: "unicode", input: "café — emoji 😀" },
];

const parseJsonCases = [
  { name: "object", input: '{"b":1,"a":[1,2]}' },
  { name: "nested", input: '{"x":{"y":[{"z":null,true,false}]}}' },
  { name: "float", input: '{"pi":3.14159,"exp":1e21,"tiny":0.000001}' },
  { name: "string-escapes", input: '{"s":"line\\nbreak\\t\\"quoted\\""}' },
  { name: "array-root", input: '[1,"two",{"three":3}]' },
  { name: "invalid", input: "{invalid}" },
  { name: "empty", input: "" },
  { name: "null-root", input: "null" },
  { name: "unicode-string", input: '{"ja":"日本語"}' },
];

const renderJsonCases = [
  { name: "json", input: { format: "json" } },
  { name: "text", input: { format: "text" } },
  { name: "html", input: { format: "html" } },
  { name: "markdown", input: { format: "markdown" } },
  { name: "raw", input: { format: "raw" } },
].map((c) => ({ ...c, input: { ...c.input, formatted: '{\n  "k": "v<&>"\n}' } }));

const stripCommentsCases = [
  { name: "md-comments", input: { format: "markdown" } },
  { name: "text-comments", input: { format: "text" } },
  { name: "html-comments", input: { format: "html" } },
  { name: "no-comments", input: { format: "markdown" } },
].map((c, i) => ({
  ...c,
  input: {
    ...c.input,
    content:
      i === 3
        ? "article body only"
        : i === 2
          ? '<p>body</p>\n<hr><div class="t comments"><p>c1</p></div>'
          : "article body\n\n---\n\n## Comments\n\n- first\n- second\n",
  },
}));

const resultFixtures = {
  content_full: {
    kind: "content", url: "https://ex.com/a", finalUrl: "https://ex.com/final",
    title: "T", author: "A", published: "2026-01-01", site: "ex.com",
    language: "en", wordCount: 42, content: "body", browser: "chrome_145", os: "windows",
  },
  content_sparse: {
    kind: "content", url: "u", finalUrl: "f", title: "", author: "", published: "",
    site: "", language: "", wordCount: 0, content: "x", browser: "b", os: "o",
  },
  content_with_type: {
    kind: "content", url: "u", finalUrl: "f", title: "", author: "", published: "",
    site: "", language: "", wordCount: 1, content: "x", browser: "b", os: "o",
    contentType: "application/json",
  },
  file: {
    kind: "file", url: "u", finalUrl: "f", title: "", author: "", published: "",
    site: "s", language: "", wordCount: 0, content: "", browser: "b", os: "o",
    filePath: "/tmp/x/report.pdf", fileSize: 12345, mimeType: "application/pdf",
  },
};

const headerCases = Object.entries(resultFixtures).flatMap(([key, result]) => [
  { name: `metadata-${key}`, input: { result, verbose: true } },
  { name: `compact-${key}`, input: { result, verbose: false } },
  { name: `response-text-${key}`, input: { result, verbose: true } },
  { name: `response-text-compact-${key}`, input: { result, verbose: false } },
]);

const errorFixtures = [
  { name: "invalid-url", input: { error: "Invalid URL: nope", code: "invalid_url", phase: "validation", retryable: false, url: "nope" } },
  { name: "unsupported-protocol", input: { error: "Only http/https URLs supported, got ftp:", code: "unsupported_protocol", phase: "validation", retryable: false, url: "ftp://x" } },
  { name: "http-500", input: { error: "Server returned HTTP 500 Internal Server Error for u.", code: "http_error", phase: "connecting", retryable: true, statusCode: 500, statusText: "Internal Server Error", timeoutMs: 15000, url: "u", finalUrl: "f" } },
  { name: "http-429", input: { error: "Server returned HTTP 429 Too Many Requests for u.", code: "http_error", phase: "waiting", retryable: true, statusCode: 429, statusText: "Too Many Requests", timeoutMs: 8000, url: "u" } },
  { name: "http-401", input: { error: "Auth", code: "http_error", statusCode: 401, statusText: "Unauthorized", timeoutMs: 5000, url: "u", retryable: false } },
  { name: "http-403-no-status-text", input: { error: "Auth", code: "http_error", statusCode: 403, retryable: false, url: "u" } },
  { name: "timeout-connecting", input: { error: "Timeout of 5000ms exceeded while connecting to https://x/.", code: "timeout", phase: "connecting", retryable: true, timeoutMs: 5000, url: "u", finalUrl: "f" } },
  { name: "timeout-waiting", input: { error: "t", code: "timeout", phase: "waiting", retryable: true, timeoutMs: 3333, url: "u" } },
  { name: "timeout-loading-file", input: { error: "t", code: "timeout", phase: "loading", retryable: true, timeoutMs: 10000, url: "u", statusCode: 200, statusText: "OK", mimeType: "application/zip", contentLength: 1572864, downloadedBytes: 393216 } },
  { name: "timeout-loading-text-response", input: { error: "t", code: "timeout", phase: "loading", retryable: true, timeoutMs: 7000, url: "u", mimeType: "text/html", contentLength: 0, downloadedBytes: 512 } },
  { name: "timeout-loading-no-length", input: { error: "t", code: "timeout", phase: "loading", retryable: true, timeoutMs: 15000, url: "u", downloadedBytes: 0 } },
  { name: "timeout-processing", input: { error: "t", code: "timeout", phase: "processing", retryable: true, timeoutMs: 4000, url: "u" } },
  { name: "timeout-unknown-phase", input: { error: "t", code: "timeout", phase: "unknown", retryable: true, timeoutMs: 20000, url: "u", contentLength: 5368709120, downloadedBytes: 524288 } },
  { name: "download-error", input: { error: "Unable to create a unique temp file for u", code: "download_error", phase: "loading", retryable: true, timeoutMs: 9000, url: "u", mimeType: "application/zip", contentLength: 1024, downloadedBytes: 1024 } },
  { name: "no-content", input: { error: "No content extracted from u.", code: "no_content", phase: "processing", retryable: false, url: "u" } },
  { name: "unexpected-response", input: { error: "Not a JSON response (content-type: text/html)", code: "unexpected_response", phase: "loading", retryable: false, url: "u", timeoutMs: 15000 } },
  { name: "network-retryable", input: { error: "Request failed while fetching u: x", code: "network_error", phase: "unknown", retryable: true, url: "u" } },
  { name: "network-dns-summary", input: { error: "DNS error: failed to lookup address for host.", code: "network_error", retryable: false, url: "u" } },
  { name: "error-only", input: { error: "Invalid JSON response" } },
];

// ---------------------------------------------------------------------------
// B. tool.ts pure functions
// ---------------------------------------------------------------------------

const defaultsCases = [
  { name: "empty", input: {} },
  { name: "full", input: { maxChars: 1000, timeoutMs: 2000, browser: "firefox_147", os: "macos", removeImages: true, includeReplies: false, batchConcurrency: 4 } },
  { name: "concurrency-fraction", input: { batchConcurrency: 3.7 } },
  { name: "concurrency-zero", input: { batchConcurrency: 0 } },
  { name: "concurrency-negative", input: { batchConcurrency: -2 } },
];

// ---------------------------------------------------------------------------
// C. settings.ts resolvePiSmartFetchSettings (tempDir default is the
// [VARIANT] rpi rename — the fixture normalizes it out).
// ---------------------------------------------------------------------------

const settingsCases = [
  { name: "both-empty", input: { global: {}, project: {} } },
  {
    name: "aliases-and-guards",
    input: {
      global: {
        webFetchVerboseByDefault: true,
        smartFetchDefaultMaxChars: "bad",
        webFetchDefaultMaxChars: 8000,
        smartFetchDefaultOs: "Haiku",
        smartFetchTempDir: " ",
      },
      project: { smartFetchDefaultOs: "ios", smartFetchDefaultIncludeReplies: false },
    },
  },
  {
    name: "project-overrides",
    input: {
      global: { smartFetchDefaultMaxChars: 1000, smartFetchDefaultBrowser: "chrome_140" },
      project: { smartFetchDefaultMaxChars: 2000 },
    },
  },
];

// ---------------------------------------------------------------------------
// D. pipeline scenarios via mocked transport/defuddle (createDefuddleFetch).
// P0 parity set: everything except the TE07 surfaces (meta-refresh recursion,
// alternate fallback, attachment download) and the P2 X/Twitter probes.
// ---------------------------------------------------------------------------

const HTML_ARTICLE = `<html><head><title>Fixture Article</title></head><body><article><h1>Main Title</h1><p>This article has enough words for readability scoring thresholds so the extractor path is exercised properly across formats.</p></article></body></html>`;
const HTML_FALLBACK = `<html><head><title>FB</title></head><body><h1>Head</h1><p>Fallback <b>bold</b> paragraph.</p><ul><li>one</li><li>two</li></ul></body></html>`;
const HTML_EMPTY = `<html><head><title>Empty</title></head><body><script>ignored()</script></body></html>`;

const pipelineScenarios = [
  {
    name: "invalid-url",
    opts: { url: "not a url" },
    fetchScript: [],
  },
  {
    name: "unsupported-protocol",
    opts: { url: "ftp://example.com/f" },
    fetchScript: [],
  },
  {
    name: "http-404",
    opts: { url: "https://ex.com/missing" },
    fetchScript: [{ status: 404, statusText: "Not Found", headers: { "content-type": "text/html" }, body: "gone" }],
  },
  {
    name: "http-500-retryable",
    opts: { url: "https://ex.com/err" },
    fetchScript: [{ status: 500, statusText: "Internal Server Error", body: "" }],
  },
  {
    name: "http-429-retryable",
    opts: { url: "https://ex.com/limited" },
    fetchScript: [{ status: 429, statusText: "Too Many Requests", body: "" }],
  },
  {
    name: "dns-error",
    opts: { url: "https://no-such-host.example.com/x" },
    fetchScript: [{ throw: { message: "getaddrinfo ENOTFOUND no-such-host.example.com: dns error" } }],
  },
  {
    name: "connect-error",
    opts: { url: "https://refused.example.com/x" },
    fetchScript: [{ throw: { message: "connect ECONNREFUSED 127.0.0.1:443: connection refused" } }],
  },
  {
    name: "tls-error",
    opts: { url: "https://self-signed.example.com/x" },
    fetchScript: [{ throw: { message: "unable to verify the first certificate: ssl handshake error" } }],
  },
  {
    name: "timeout-error",
    opts: { url: "https://slow.example.com/x", timeoutMs: 1500 },
    fetchScript: [{ throw: { message: "operation timed out" } }],
  },
  {
    name: "unknown-profile",
    opts: { url: "https://ex.com/x", browser: "chrome_99" },
    fetchScript: [{ throw: { message: "Invalid browser profile: chrome_99. Available profiles: chrome_100" } }],
  },
  {
    name: "json-response-markdown-format",
    opts: { url: "https://ex.com/api" },
    fetchScript: [{ status: 200, headers: { "content-type": "application/json" }, body: '{"b":1,"a":[1,2]}' }],
  },
  {
    name: "json-response-json-format",
    opts: { url: "https://ex.com/api", format: "json" },
    fetchScript: [{ status: 200, headers: { "content-type": "application/json" }, body: '{"k":"v"}' }],
  },
  {
    name: "json-body-text-format",
    opts: { url: "https://ex.com/api", format: "text" },
    fetchScript: [{ status: 200, headers: { "content-type": "text/plain" }, body: '{"k":1}' }],
  },
  {
    name: "json-body-html-format",
    opts: { url: "https://ex.com/api", format: "html" },
    fetchScript: [{ status: 200, headers: { "content-type": "text/plain" }, body: '{"k":1}' }],
  },
  {
    name: "invalid-json-json-format",
    opts: { url: "https://ex.com/api", format: "json" },
    fetchScript: [{ status: 200, headers: { "content-type": "application/json" }, body: "{broken" }],
  },
  {
    name: "non-json-json-format",
    opts: { url: "https://ex.com/page", format: "json" },
    fetchScript: [{ status: 200, headers: { "content-type": "application/xml" }, body: "<x>not json</x>" }],
  },
  {
    name: "plain-text-response",
    opts: { url: "https://ex.com/robots.txt" },
    fetchScript: [{ status: 200, headers: { "content-type": "text/plain" }, body: "User-agent: *\nDisallow: /" }],
  },
  {
    name: "plain-text-html-format",
    opts: { url: "https://ex.com/robots.txt", format: "html" },
    fetchScript: [{ status: 200, headers: { "content-type": "text/plain" }, body: "a < b & c" }],
  },
  {
    name: "markdown-content-type",
    opts: { url: "https://ex.com/readme" },
    fetchScript: [{ status: 200, headers: { "content-type": "text/markdown" }, body: "# Heading\n\ntext" }],
  },
  {
    name: "extraction-success-markdown",
    opts: { url: "https://ex.com/article" },
    fetchScript: [{ status: 200, headers: { "content-type": "text/html" }, body: HTML_ARTICLE }],
    defuddleResult: { content: "Extracted **article** body.", wordCount: 4, title: "Fixture Article", author: "A. Author", published: "2026-02-03", site: "Example", language: "en" },
  },
  {
    name: "extraction-success-text-format",
    opts: { url: "https://ex.com/article", format: "text" },
    fetchScript: [{ status: 200, headers: { "content-type": "text/html" }, body: HTML_ARTICLE }],
    defuddleResult: { content: "Extracted **article** body.", wordCount: 4 },
  },
  {
    name: "extraction-success-html-format",
    opts: { url: "https://ex.com/article", format: "html" },
    fetchScript: [{ status: 200, headers: { "content-type": "text/html" }, body: HTML_ARTICLE }],
    defuddleResult: { content: "<p>cleaned</p>", wordCount: 1 },
  },
  {
    name: "dom-fallback-markdown",
    opts: { url: "https://ex.com/fallback" },
    fetchScript: [{ status: 200, headers: { "content-type": "text/html" }, body: HTML_FALLBACK }],
    defuddleResult: { content: undefined, wordCount: 0 },
  },
  {
    name: "dom-fallback-text",
    opts: { url: "https://ex.com/fallback", format: "text" },
    fetchScript: [{ status: 200, headers: { "content-type": "text/html" }, body: HTML_FALLBACK }],
    defuddleResult: { content: undefined, wordCount: 0 },
  },
  {
    name: "dom-fallback-html-uses-raw-body",
    opts: { url: "https://ex.com/fallback", format: "html" },
    fetchScript: [{ status: 200, headers: { "content-type": "text/html" }, body: HTML_FALLBACK }],
    defuddleResult: { content: undefined, wordCount: 0 },
  },
  {
    name: "no-content",
    opts: { url: "https://ex.com/empty" },
    fetchScript: [{ status: 200, headers: { "content-type": "text/html" }, body: HTML_EMPTY }],
    defuddleResult: { content: undefined, wordCount: 0 },
  },
  {
    name: "maxchars-truncation",
    opts: { url: "https://ex.com/long", maxChars: 20 },
    fetchScript: [{ status: 200, headers: { "content-type": "text/plain" }, body: "0123456789012345678901234567890123456789" }],
  },
  {
    name: "maxchars-astral-variant",
    opts: { url: "https://ex.com/emoji", maxChars: 7 },
    fetchScript: [{ status: 200, headers: { "content-type": "text/plain" }, body: "a😀b😀c😀d😀e😀f😀g😀h" }],
  },
  {
    name: "raw-no-maxchars",
    opts: { url: "https://ex.com/raw", format: "raw" },
    fetchScript: [{ status: 200, headers: { "content-type": "text/html" }, body: "<html><body>raw body 1234567890</body></html>" }],
  },
  {
    name: "raw-explicit-maxchars",
    opts: { url: "https://ex.com/raw", format: "raw", maxChars: 8 },
    fetchScript: [{ status: 200, headers: { "content-type": "application/json" }, body: '{"key":"value"}' }],
  },
  {
    name: "non-html-content-type",
    opts: { url: "https://ex.com/binaryish", tempDir: DOWNLOAD_TEMP_DIR },
    fetchScript: [
      {
        status: 200,
        headers: { "content-type": "application/octet-stream" },
        body: "binary-ish textual body",
        stream: true,
      },
    ],
  },
  {
    name: "custom-headers-merge",
    opts: { url: "https://ex.com/headers", headers: { Accept: "text/custom", "X-Token": "secret" } },
    fetchScript: [{ status: 200, headers: { "content-type": "text/plain" }, body: "ok" }],
  },
  {
    name: "format-raw-accept-header",
    opts: { url: "https://ex.com/accept", format: "raw" },
    fetchScript: [{ status: 200, headers: { "content-type": "text/plain" }, body: "ok" }],
  },
  {
    name: "format-json-accept-header",
    opts: { url: "https://ex.com/accept", format: "json" },
    fetchScript: [{ status: 200, headers: { "content-type": "application/json" }, body: '{"ok":true}' }],
  },
  // -------------------------------------------------------------------------
  // TE07 parity set: meta-refresh recursion (FR-P1-2), alternate fallback
  // (FR-P1-3) and the download branch (FR-P1-4).
  // -------------------------------------------------------------------------
  {
    name: "meta-refresh-follow",
    opts: { url: "https://ex.com/start" },
    fetchScript: [
      {
        status: 200,
        headers: { "content-type": "text/html" },
        body: `<html><head><meta http-equiv="refresh" content="0;url=/final"></head><body>redirecting…</body></html>`,
      },
      {
        status: 200,
        headers: { "content-type": "text/html" },
        body: HTML_ARTICLE,
      },
    ],
    defuddleResult: { content: "Final page body.", wordCount: 3, title: "Fixture Article" },
  },
  {
    name: "meta-refresh-follow-raw-format",
    opts: { url: "https://ex.com/start", format: "raw" },
    fetchScript: [
      {
        status: 200,
        headers: { "content-type": "text/html" },
        body: `<html><head><meta http-equiv="refresh" content="2;url=https://ex.com/raw-target"></head><body>redirecting…</body></html>`,
      },
      { status: 200, headers: { "content-type": "text/plain" }, body: "raw landed body" },
    ],
  },
  {
    name: "meta-refresh-delay-30-not-followed",
    opts: { url: "https://ex.com/slowmeta" },
    fetchScript: [
      {
        status: 200,
        headers: { "content-type": "text/html" },
        body: `<html><head><meta http-equiv="refresh" content="30;url=/final"></head><body>still here</body></html>`,
      },
    ],
    defuddleResult: { content: "Content stays on page.", wordCount: 4 },
  },
  {
    name: "meta-refresh-limit-exceeded",
    opts: { url: "https://ex.com/hop-0" },
    fetchScript: [0, 1, 2, 3, 4, 5].map((hop) => ({
      status: 200,
      headers: { "content-type": "text/html" },
      body: `<html><head><meta http-equiv="refresh" content="0;url=/hop-${hop + 1}"></head><body>loop</body></html>`,
    })),
  },
  {
    name: "alternate-json-format-fallback",
    opts: { url: "https://ex.com/page", format: "json" },
    fetchScript: [
      {
        status: 200,
        headers: { "content-type": "text/html" },
        body: `<html><head><link rel="alternate" type="application/json" href="/api.json"></head><body>not json</body></html>`,
      },
      { status: 200, headers: { "content-type": "application/json" }, body: '{"alt":true}' },
    ],
  },
  {
    name: "alternate-empty-content-fallback",
    opts: { url: "https://ex.com/emptyish" },
    fetchScript: [
      {
        status: 200,
        headers: { "content-type": "text/html" },
        body: `<html><head><link rel="alternate" type="text/markdown" href="/full.md"></head><body><script>only a script</script></body></html>`,
      },
      { status: 200, headers: { "content-type": "text/markdown" }, body: "# Alternate Body\n\nPlenty of markdown words on the alternate endpoint." },
    ],
    defuddleResult: { content: undefined, wordCount: 0 },
  },
  {
    name: "alternate-thin-content-fallback",
    opts: { url: "https://ex.com/thin" },
    fetchScript: [
      {
        status: 200,
        headers: { "content-type": "text/html" },
        body: `<html><head><link rel="alternate" type="text/markdown" href="/full.md"></head><body><article><p>too thin</p></article></body></html>`,
      },
      { status: 200, headers: { "content-type": "text/markdown" }, body: "# Full Alternate\n\nThis alternate response carries a comfortable amount of words for the thin-content fallback threshold." },
    ],
    defuddleResult: { content: "too thin", wordCount: 2 },
  },
  {
    name: "alternate-thin-content-unqualified-type-kept",
    opts: { url: "https://ex.com/thin-alone" },
    fetchScript: [
      {
        status: 200,
        headers: { "content-type": "text/html" },
        body: `<html><head><link rel="alternate" type="application/rss+xml" href="/feed.xml"></head><body><article><p>too thin but no qualified alternate</p></article></body></html>`,
      },
    ],
    defuddleResult: { content: "too thin but no qualified alternate", wordCount: 6 },
  },
  {
    name: "download-attachment-disposition",
    opts: { url: "https://ex.com/files/report.pdf", tempDir: DOWNLOAD_TEMP_DIR },
    fetchScript: [
      {
        status: 200,
        headers: {
          "content-type": "application/pdf",
          "content-disposition": 'attachment; filename="Mock Report.pdf"',
        },
        body: "PDF-BYTES-0123456789",
        stream: true,
      },
    ],
  },
  {
    name: "download-filename-star-disposition",
    opts: { url: "https://ex.com/dl", tempDir: DOWNLOAD_TEMP_DIR },
    fetchScript: [
      {
        status: 200,
        headers: {
          "content-type": "text/plain",
          "content-disposition": "attachment; filename*=UTF-8''na%C3%AFve%20doc.txt",
        },
        body: "plain but forced to download by disposition",
        stream: true,
      },
    ],
  },
];

// ---------------------------------------------------------------------------
// generate
// ---------------------------------------------------------------------------

// Fresh download dir so the file-result fixtures stay deterministic (a
// leftover file would trip the EEXIST retry and shift the recorded path).
rmSync(DOWNLOAD_TEMP_DIR, { recursive: true, force: true });

mkdirSync(OUT_DIR, { recursive: true });

const formatOut = {
  markdownToText: run("markdownToText", markdownToTextCases, (input) => format.markdownToText(input)),
  truncateContent: run("truncateContent", truncateCases, ({ content, maxChars }) => sanitizeLoneSurrogates(format.truncateContent(content, maxChars))),
  estimateWordCount: run("estimateWordCount", wordCountCases, (input) => format.estimateWordCount(input)),
  escapeHtml: run("escapeHtml", escapeHtmlCases, (input) => format.escapeHtml(input)),
  parseAndFormatJson: run("parseAndFormatJson", parseJsonCases, (input) => format.parseAndFormatJson(input)),
  renderJsonContent: run("renderJsonContent", renderJsonCases, ({ formatted, format: fmt }) => format.renderJsonContent(formatted, fmt)),
  stripExtractorComments: run("stripExtractorComments", stripCommentsCases, ({ content, format: fmt }) => format.stripExtractorComments(content, fmt)),
  metadataHeaders: run("metadataHeaders", headerCases, ({ result, verbose }) =>
    verbose ? format.buildMetadataHeader(result) : format.buildCompactMetadataHeader(result),
  ),
  responseTexts: run("responseTexts", headerCases, ({ result, verbose }) => format.buildFetchResponseText(result, { verbose })),
  errorResponseTexts: run("errorResponseTexts", errorFixtures, (error) => format.buildFetchErrorResponseText(error)),
  errorSummaries: run("errorSummaries", errorFixtures, (error) => format.buildUserFacingFetchErrorSummary(error)),
  batchResponseText: [
    {
      name: "mixed-batch",
      input: {
        verbose: true,
        result: {
          items: [
            { index: 0, request: { url: "https://ex.com/a" }, status: "done", progress: 1, result: resultFixtures.content_full },
            { index: 1, request: { url: "bad" }, status: "error", progress: 1, error: "Error: Invalid URL: bad" },
            { index: 2, request: { url: "https://ex.com/c" }, status: "error", progress: 1, error: "Error: Server returned HTTP 500 Internal Server Error for https://ex.com/c.\n\nThe server failed while processing the request. Retrying later may help." },
          ],
          total: 3, succeeded: 1, failed: 2, batchConcurrency: 8,
        },
      },
      output: null, // filled below
    },
  ],
};
formatOut.batchResponseText[0].output = format.buildBatchFetchResponseText(
  formatOut.batchResponseText[0].input.result,
  { verbose: formatOut.batchResponseText[0].input.verbose },
);

writeFileSync(join(OUT_DIR, "format.json"), JSON.stringify(formatOut, null, 2) + "\n");

const toolOut = {
  resolveFetchToolDefaults: run("resolveFetchToolDefaults", defaultsCases, (input) => {
    const resolved = tool.resolveFetchToolDefaults(input);
    // drop tempDir (absent when unset — key-order/undefined JSON churn)
    return resolved;
  }),
};
writeFileSync(join(OUT_DIR, "tool.json"), JSON.stringify(toolOut, null, 2) + "\n");

const settingsOut = {
  resolvePiSmartFetchSettings: run("resolvePiSmartFetchSettings", settingsCases, ({ global, project }) => {
    const resolved = settings.resolvePiSmartFetchSettings(global, project);
    // tempDir default is the declared rpi [VARIANT] (smart-fetch-rpi vs -pi):
    // normalize to "<TMPDIR>/<name>" so the two sides can diff.
    resolved.tempDir = resolved.tempDir ? resolved.tempDir.replace(/^.*\/smart-fetch-/, "<TMPDIR>/smart-fetch-") : resolved.tempDir;
    return resolved;
  }),
};
writeFileSync(join(OUT_DIR, "settings.json"), JSON.stringify(settingsOut, null, 2) + "\n");

const pipelineOut = [];
for (const scenario of pipelineScenarios) {
  const { result, captured, progressEvents } = await drivePipeline(scenario);
  pipelineOut.push({
    name: scenario.name,
    input: { opts: scenario.opts, defuddleResult: scenario.defuddleResult ?? { content: undefined, wordCount: 0 }, fetchScript: scenario.fetchScript },
    output: {
      result,
      requestHeadersSeen: captured.calls[0]?.options?.headers ?? null,
      requestTimeout: captured.calls[0]?.options?.timeout ?? null,
      requestProxy: captured.calls[0]?.options?.proxy ?? null,
      requestRedirect: captured.calls[0]?.options?.redirect ?? null,
      // TE08 FR-P2-A parity surface: the hooks event stream in fire order.
      progressEvents,
    },
  });
}
writeFileSync(join(OUT_DIR, "pipeline.json"), JSON.stringify(pipelineOut, null, 2) + "\n");

// ---------------------------------------------------------------------------
// E. batch progress snapshots (TE08 FR-P2-C) — `executeBatchFetchToolCall`
// with batchConcurrency 1 (deterministic frame order) and the same mocked
// transport per request. statusStartedAt is a wall-clock timestamp on both
// sides: masked to 0 in the fixture (assertion-side masking mirrors it).
// ---------------------------------------------------------------------------

const batchProgressScenarios = [
  {
    name: "three-items-mixed-outcomes",
    requests: [{ url: "https://ex.com/a" }, { url: "https://ex.com/missing" }, { url: "https://ex.com/doc" }],
    fetchScript: [
      { status: 200, headers: { "content-type": "text/plain" }, body: "first body" },
      { status: 404, statusText: "Not Found", headers: { "content-type": "text/html" }, body: "gone" },
      { status: 200, headers: { "content-type": "text/plain" }, body: "third body" },
    ],
    defuddleResult: { content: undefined, wordCount: 0 },
  },
  {
    // attachment with a content-length: exercises the per-chunk
    // body_progress frames on the download path (TE-D27 approximation).
    name: "attachment-with-content-length",
    requests: [{ url: "https://ex.com/data.bin", tempDir: DOWNLOAD_TEMP_DIR }],
    fetchScript: [
      {
        status: 200,
        headers: { "content-type": "application/octet-stream", "content-length": "9" },
        body: "123456789",
        stream: true,
      },
    ],
    defuddleResult: { content: undefined, wordCount: 0 },
  },
  {
    name: "transport-error-item",
    requests: [{ url: "https://no-such-host.example.com/x" }, { url: "https://ex.com/ok" }],
    fetchScript: [
      { throw: { message: "getaddrinfo ENOTFOUND no-such-host.example.com: dns error" } },
      { status: 200, headers: { "content-type": "text/plain" }, body: "fine" },
    ],
    defuddleResult: { content: undefined, wordCount: 0 },
  },
  {
    name: "extraction-item",
    requests: [{ url: "https://ex.com/article" }],
    fetchScript: [{ status: 200, headers: { "content-type": "text/html" }, body: HTML_ARTICLE }],
    defuddleResult: { content: "Extracted body.", wordCount: 2 },
  },
];

async function driveBatchProgress({ requests, fetchScript, defuddleResult }) {
  const script = [...fetchScript];
  const fetchDependency = async (url, options) => {
    const step = script.shift();
    if (!step) throw new Error("unexpected extra fetch call");
    if (step.throw) {
      const error = new Error(step.throw.message);
      if (step.throw.name) error.name = step.throw.name;
      throw error;
    }
    // Same synthesized engine events as drivePipeline (TE-D27 alignment).
    const { fireBodyRead } = engineEventBridge(options, step, url);
    return makeResponse({ url, ...step, onBodyRead: fireBodyRead });
  };
  const defuddleDependency = async () =>
    defuddleResult ?? { content: undefined, wordCount: 0 };
  const defuddleFetch = extract.createDefuddleFetch({
    fetch: fetchDependency,
    defuddle: defuddleDependency,
    getProfiles: () => ["chrome_145"],
  });

  const snapshots = [];
  const result = await tool.executeBatchFetchToolCall({ requests }, tool.resolveFetchToolDefaults({}), {
    batchConcurrency: 1,
    onProgress(snapshot) {
      snapshots.push(
        JSON.parse(
          JSON.stringify(snapshot, (key, value) =>
            key === "statusStartedAt" ? 0 : sanitizeLoneSurrogates(value),
          ),
        ),
      );
    },
    // Route items through the mocked pipeline (the default executeItem would
    // hit the real network): params are already FetchOptions-shaped for
    // these scenarios.
    executeItem: (params, _defaults, hooks) =>
      defuddleFetch(params, hooks ?? {}),
  });
  return {
    snapshots,
    result: JSON.parse(
      JSON.stringify(result, (key, value) => (key === "filePath" ? "<FILE>" : value)),
    ),
  };
}

const batchProgressOut = [];
rmSync(DOWNLOAD_TEMP_DIR, { recursive: true, force: true });
for (const scenario of batchProgressScenarios) {
  const { snapshots, result } = await driveBatchProgress(scenario);
  batchProgressOut.push({
    name: scenario.name,
    input: { requests: scenario.requests, fetchScript: scenario.fetchScript, defuddleResult: scenario.defuddleResult ?? { content: undefined, wordCount: 0 } },
    output: { snapshots, result },
  });
}
writeFileSync(join(OUT_DIR, "batch-progress.json"), JSON.stringify(batchProgressOut, null, 2) + "\n");

console.log(
  `wrote: format.json (${Object.entries(formatOut).reduce((n, [, v]) => n + v.length, 0)} cases), ` +
    `tool.json, settings.json, pipeline.json (${pipelineOut.length} scenarios), ` +
    `batch-progress.json (${batchProgressOut.length} scenarios)`,
);
