// TE06 extraction-metric harness (design §5.2): runs the PINNED upstream
// extraction (defuddle 0.19.2, same options as the pipeline's defuddle call)
// over the frozen corpus, then compares against the Rust dom_smoothie
// baseline produced by `cargo run -p rpi-ext-smart-fetch --example
// extract_baseline`, reporting:
//   - title exact-match rate (target 100%)
//   - metadata field agreement (author/published/site/language; target ≥90%)
//   - content token-bag F1 (calibration initial value: 0.85)
//
// Run: node_modules/.bin/tsx scripts/smart-fetch-parity/extract-metrics.mjs \
//        <(cargo run -q -p rpi-ext-smart-fetch --example extract_baseline)
// Writes fixtures/generated/smart-fetch-parity/extract-metrics-report.json.

import { readdirSync, readFileSync, writeFileSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { join } from "node:path";

// staging tree carries both the pinned upstream sources AND the npm deps
// (defuddle resolves from its node_modules, not from scripts/)
const STAGING = process.env.RPI_SF_STAGING ?? "/tmp/rpi-smart-fetch-parity-deps";
const { parseLinkedomHTML: parse } = await import(join(STAGING, "core-src/dom.ts"));
const defuddleNode = await import(join(STAGING, "node_modules/defuddle/dist/node.js"));
// dist/node.js is CJS: the named export rides on `.default` under tsx
const Defuddle = defuddleNode.Defuddle ?? defuddleNode.default?.Defuddle;

const ROOT = new URL("../..", import.meta.url).pathname;
const CORPUS = join(ROOT, "fixtures/smart-fetch-corpus");
const REPORT = join(
  ROOT,
  "fixtures/generated/smart-fetch-parity/extract-metrics-report.json",
);

// --- upstream baseline -------------------------------------------------------

const files = readdirSync(CORPUS)
  .filter((name) => name.endsWith(".html"))
  .sort();

const upstream = [];
for (const name of files) {
  const html = readFileSync(join(CORPUS, name), "utf-8");
  const url = `https://corpus.example.com/${name}`;
  // same call shape as the pipeline's runtimeDependencies.defuddle invocation
  const result = await Defuddle(parse(html, url), url, {
    markdown: true,
    removeImages: false,
    includeReplies: "extractors",
  });
  upstream.push({
    name,
    title: result.title ?? "",
    author: result.author ?? "",
    published: result.published ?? "",
    site: result.site ?? "",
    language: result.language ?? "",
    content: result.content ?? "",
  });
}

// --- rust baseline -----------------------------------------------------------

let rustRecords;
if (process.argv[2]) {
  rustRecords = JSON.parse(readFileSync(process.argv[2], "utf-8"));
} else {
  const stdout = execFileSync(
    "cargo",
    ["run", "-q", "-p", "rpi-ext-smart-fetch", "--example", "extract_baseline"],
    { cwd: ROOT, maxBuffer: 64 * 1024 * 1024 },
  );
  rustRecords = JSON.parse(stdout.toString());
}

// --- metrics -----------------------------------------------------------------

const tokenize = (text) =>
  (text ?? "")
    .toLowerCase()
    .split(/[^\p{L}\p{N}]+/u)
    .filter((token) => token.length > 0);

const bagF1 = (a, b) => {
  const count = new Map();
  for (const token of a) count.set(token, (count.get(token) ?? 0) + 1);
  let overlap = 0;
  for (const token of b) {
    const left = count.get(token) ?? 0;
    if (left > 0) {
      overlap += 1;
      count.set(token, left - 1);
    }
  }
  if (a.length === 0 && b.length === 0) return 1;
  if (a.length === 0 || b.length === 0) return 0;
  const precision = overlap / b.length;
  const recall = overlap / a.length;
  return precision + recall === 0 ? 0 : (2 * precision * recall) / (precision + recall);
};

const fieldAgreement = (key) => {
  let both = 0;
  let agree = 0;
  for (const up of upstream) {
    const rs = rustRecords.find((r) => r.name === up.name);
    if (!rs) continue;
    const u = (up[key] ?? "").trim();
    const r = String(rs[key] ?? "").trim();
    if (u === "" && r === "") continue; // both absent → not a disagreement
    both += 1;
    if (u === r) agree += 1;
  }
  return { agree, both, rate: both === 0 ? 1 : agree / both };
};

const pages = upstream.map((up) => {
  const rs = rustRecords.find((r) => r.name === up.name) ?? {};
  const upTokens = tokenize(up.content);
  const rsTokens = tokenize(String(rs.content ?? ""));
  return {
    name: up.name,
    titleUpstream: up.title,
    titleRust: rs.title ?? "",
    titleMatch: (up.title ?? "").trim() === String(rs.title ?? "").trim(),
    f1: Number(bagF1(upTokens, rsTokens).toFixed(4)),
  };
});

const titlesMatched = pages.filter((p) => p.titleMatch).length;
const f1Values = pages.map((p) => p.f1).sort((a, b) => a - b);
const mean = (list) => list.reduce((a, b) => a + b, 0) / (list.length || 1);
const summary = {
  corpusSize: pages.length,
  titleMatchRate: Number((titlesMatched / (pages.length || 1)).toFixed(4)),
  metadata: {
    author: fieldAgreement("author"),
    published: fieldAgreement("published"),
    site: fieldAgreement("site"),
    language: fieldAgreement("language"),
  },
  contentTokenF1: {
    min: f1Values[0] ?? 0,
    median: f1Values[Math.floor(f1Values.length / 2)] ?? 0,
    mean: Number(mean(f1Values).toFixed(4)),
    pagesBelow0_85: f1Values.filter((f1) => f1 < 0.85).length,
  },
  thresholds: {
    titleTarget: 1.0,
    metadataTarget: 0.9,
    contentF1CalibrationInitial: 0.85,
    note: "TE06 records calibrated initial values; TE07 pins the thresholds into the acceptance script.",
  },
};

writeFileSync(REPORT, JSON.stringify({ summary, pages }, null, 2) + "\n");
console.log(JSON.stringify(summary, null, 2));
console.log(`report: ${REPORT}`);
