// TE07 extraction-metric harness (design §5.2): runs the PINNED upstream
// extraction (defuddle 0.19.2, same options as the pipeline's defuddle call)
// over the frozen corpus, then compares against the Rust dom_smoothie
// baseline produced by `cargo run -p rpi-ext-smart-fetch --example
// extract_baseline`, reporting AND enforcing the thresholds pinned at TE07
// acceptance (2026-08-15; calibrated against the TE06 corpus):
//   - title exact-match rate = 1.0 (all pages)
//   - published / language agreement = 1.0
//   - author / site agreement = 1.0 on the META-BACKED pages only
//     (article-*.html carries <meta name=author>, lang-ja.html the only
//     og:site_name) — off-meta divergences are engine-heuristic noise
//     (defuddle misreads authors as site names; dom_smoothie's byline is
//     more eager), classified in the TE06 calibration report.
//   - content token-bag F1: non-forum pages each ≥ 0.85 with mean ≥ 0.93;
//     forum pages each ≥ 0.40 (the declared [VARIANT] core divergence —
//     reply-block boundaries).
// !!! Bumping dom_smoothie (or the corpus) REQUIRES recalibrating these
// !!! constants — an upgrade that drifts extraction must fail HERE, not in
// !!! production. This is the CI soft gate (design §5.2/§5).
//
// Run: node_modules/.bin/tsx scripts/smart-fetch-parity/extract-metrics.mjs \
//        <(cargo run -q -p rpi-ext-smart-fetch --example extract_baseline)
// Writes fixtures/generated/smart-fetch-parity/extract-metrics-report.json;
// exits non-zero when any pinned threshold regresses.

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

// --- pinned thresholds (TE07 acceptance; see header) ------------------------

const PINNED = {
  titleRate: 1.0,
  publishedRate: 1.0,
  languageRate: 1.0,
  authorMetaBackedRate: 1.0,
  siteMetaBackedRate: 1.0,
  nonForumF1PerPageMin: 0.85,
  nonForumF1MeanMin: 0.93,
  forumF1PerPageMin: 0.4,
};
// meta-backed subsets keyed off the deterministic corpus generator
// (gen-corpus.mjs): article-*.html embeds <meta name=author> +
// article:published_time; lang-ja.html is the only og:site_name page.
const metaBackedAuthors = pages.filter((p) => /^article-\d+\.html$/.test(p.name));
const metaBackedSite = pages.filter((p) => p.name === "lang-ja.html");
const forumPages = pages.filter((p) => p.name.startsWith("forum-"));
const nonForumPages = pages.filter((p) => !p.name.startsWith("forum-"));

const titleRate = titlesMatched / (pages.length || 1);
const agreementOf = (key, subset) => {
  let both = 0;
  let agree = 0;
  for (const up of upstream) {
    if (!subset.some((p) => p.name === up.name)) continue;
    const rs = rustRecords.find((r) => r.name === up.name);
    if (!rs) continue;
    const u = (up[key] ?? "").trim();
    const r = String(rs[key] ?? "").trim();
    if (u === "" && r === "") continue;
    both += 1;
    if (u === r) agree += 1;
  }
  return both === 0 ? 1 : agree / both;
};
const authorMetaRate = agreementOf("author", metaBackedAuthors);
const siteMetaRate = agreementOf("site", metaBackedSite);
const publishedRate = fieldAgreement("published").rate;
const languageRate = fieldAgreement("language").rate;
const nonForumF1Mean = mean(nonForumPages.map((p) => p.f1));

const violations = [];
if (titleRate < PINNED.titleRate)
  violations.push(`title rate ${titleRate} < ${PINNED.titleRate}`);
if (publishedRate < PINNED.publishedRate)
  violations.push(`published rate ${publishedRate} < ${PINNED.publishedRate}`);
if (languageRate < PINNED.languageRate)
  violations.push(`language rate ${languageRate} < ${PINNED.languageRate}`);
if (authorMetaRate < PINNED.authorMetaBackedRate)
  violations.push(`meta-backed author rate ${authorMetaRate} < ${PINNED.authorMetaBackedRate}`);
if (siteMetaRate < PINNED.siteMetaBackedRate)
  violations.push(`meta-backed site rate ${siteMetaRate} < ${PINNED.siteMetaBackedRate}`);
if (nonForumF1Mean < PINNED.nonForumF1MeanMin)
  violations.push(`non-forum F1 mean ${nonForumF1Mean.toFixed(4)} < ${PINNED.nonForumF1MeanMin}`);
for (const page of nonForumPages) {
  if (page.f1 < PINNED.nonForumF1PerPageMin)
    violations.push(`non-forum ${page.name} F1 ${page.f1} < ${PINNED.nonForumF1PerPageMin}`);
}
for (const page of forumPages) {
  if (page.f1 < PINNED.forumF1PerPageMin)
    violations.push(`forum ${page.name} F1 ${page.f1} < ${PINNED.forumF1PerPageMin}`);
}

const summary = {
  corpusSize: pages.length,
  titleMatchRate: Number(titleRate.toFixed(4)),
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
  pinnedGate: {
    titleRate,
    publishedRate,
    languageRate,
    authorMetaBackedRate: authorMetaRate,
    siteMetaBackedRate: siteMetaRate,
    nonForumF1Mean: Number(nonForumF1Mean.toFixed(4)),
    nonForumF1PerPageMin: Math.min(...nonForumPages.map((p) => p.f1), 1),
    forumF1PerPageMin: Math.min(...forumPages.map((p) => p.f1), 1),
    violations,
  },
  thresholds: {
    ...PINNED,
    calibrated: "2026-08-15 TE06 corpus (36 pages), pinned at TE07 acceptance",
    note: "Extraction-engine (dom_smoothie) upgrades MUST recalibrate — the gate fails loudly here.",
  },
};

writeFileSync(REPORT, JSON.stringify({ summary, pages }, null, 2) + "\n");
console.log(JSON.stringify(summary, null, 2));
console.log(`report: ${REPORT}`);
if (violations.length > 0) {
  console.error(`\nPINNED THRESHOLD VIOLATIONS (${violations.length}):`);
  for (const violation of violations) console.error(`  - ${violation}`);
  process.exit(1);
}
console.log("\nall pinned thresholds hold");
