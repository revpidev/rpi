#!/usr/bin/env node
// Normalizer for official-conformance referee artifacts (wired in by
// run-parity-suite.sh). The referee writes per-run state into its output —
// timestamps, ephemeral server ports, epoch-derived session ids, retry
// timing jitter — which would show up as full-file churn on every rerun of
// the evidence chain. This collapses the run-volatile values to markers so
// the archived checks.json diffs only when the wire behavior changes.
//
//   node normalize-conformance.mjs <in-file> <out-file>
//
// Normalizations:
//   ISO-8601 timestamps (check.timestamp)      → "$ts"
//   host header ephemeral port localhost:NNNNN → "localhost:$port"
//   mcp-session-id "session-<epoch-millis>"    → "session-$epoch"
//   retry timing jitter (details.actualDelayMs) → dropped; the boolean
//     withinTolerance / tooEarly / slightlyLate / veryLate verdicts remain
//     as the stable evidence that the retry delay was respected.
//   checks array → stable-sorted by normalized content: the referee emits
//     independent checks (e.g. sse-retry's outgoing-response vs
//     outgoing-sse-event) from concurrent tasks, so their arrival order
//     jitters between runs even when every check passes identically (TE24
//     independent tracking item H1, closed in v0.1.4 M7). Sorting the
//     normalized checks makes the archived file byte-stable across reruns.
//
// Non-JSON files (stdout.txt / stderr.txt) get the textual substitutions
// only (ports), since that is all the referee puts there.

import { readFileSync, writeFileSync } from "node:fs";

const [inPath, outPath] = process.argv.slice(2);
if (!inPath || !outPath) {
  console.error("Usage: node normalize-conformance.mjs <in-file> <out-file>");
  process.exit(1);
}

const PORT_RE = /\b(?:localhost|127\.0\.0\.1):\d{2,5}\b/g;
const SESSION_RE = /\bsession-\d{9,}\b/g;

function normalizeText(text) {
  return text.replace(PORT_RE, "localhost:$port").replace(SESSION_RE, "session-$epoch");
}

const raw = readFileSync(inPath, "utf8");
let out;
if (inPath.endsWith(".json")) {
  const document = JSON.parse(raw);
  for (const check of document) {
    if (typeof check.timestamp === "string") check.timestamp = "$ts";
    const headers = check?.details?.headers;
    if (headers && typeof headers["mcp-session-id"] === "string") {
      headers["mcp-session-id"] = normalizeText(headers["mcp-session-id"]);
    }
    if (headers && typeof headers.host === "string") {
      headers.host = normalizeText(headers.host);
    }
    if (check?.details && "actualDelayMs" in check.details) {
      delete check.details.actualDelayMs;
    }
  }
  // Stable order by normalized content (see header note): two runs that
  // pass the same checks must produce byte-identical archives regardless
  // of the referee's concurrent emission order.
  document.sort((a, b) => {
    const left = JSON.stringify(a);
    const right = JSON.stringify(b);
    return left < right ? -1 : left > right ? 1 : 0;
  });
  out = JSON.stringify(document, null, 2) + "\n";
} else {
  out = normalizeText(raw);
}
writeFileSync(outPath, out);
