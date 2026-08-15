// TE06 extraction-metric corpus generator (design §5.2): synthesizes a
// deterministic 36-page static HTML corpus across the six shapes the
// requirements call out — article / docs / forum / code-heavy / table /
// footnote pages, plus nav/ad noise, multilingual and edge variants. The
// corpus freezes into the repo so both the upstream baseline (defuddle
// 0.19.2) and the Rust port (dom_smoothie) run identical inputs.
//
// Run: node_modules/.bin/tsx scripts/smart-fetch-parity/gen-corpus.mjs

import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const OUT = new URL("../../fixtures/smart-fetch-corpus/", import.meta.url).pathname;
mkdirSync(OUT, { recursive: true });

// deterministic pseudo-random content (no Math.random — corpus must be
// reproducible byte-for-byte)
let seed = 42;
const rand = () => {
  seed = (seed * 1103515245 + 12345) % 2 ** 31;
  return seed / 2 ** 31;
};
const pick = (list) => list[Math.floor(rand() * list.length)];
const int = (min, max) => min + Math.floor(rand() * (max - min + 1));

const WORDS = "structure narrative argument evidence analysis context detail signal measure framework approach outcome finding review summary critique method sample variant boundary instance pattern transition emphasis resolution".split(" ");
const SENTENCES = Array.from({ length: 40 }, () =>
  Array.from({ length: int(8, 18) }, () => pick(WORDS)).join(" "),
);
const paragraph = (count) => Array.from({ length: count }, (_, i) => SENTENCES[(i * 7 + int(0, 6)) % SENTENCES.length]).join(". ") + ".";
const paragraphs = (count) => Array.from({ length: count }, () => `<p>${paragraph(int(3, 6))}</p>`).join("\n      ");

const page = (title, body, head = "") =>
  `<!DOCTYPE html>\n<html lang="en">\n  <head>\n    <meta charset="utf-8">\n    <title>${title}</title>\n    ${head}\n  </head>\n  <body>\n${body}\n  </body>\n</html>\n`;

const noise = `
    <nav><a href="/">Home</a> <a href="/about">About</a> <a href="/tags">Tags</a></nav>
    <div class="sidebar"><div class="ad">Buy things now limited offer</div><ul><li>Related link</li><li>Archive 2024</li></ul></div>`;

const corpus = [];

// 1. article pages (6)
for (let i = 1; i <= 6; i++) {
  corpus.push({
    name: `article-${i}.html`,
    html: page(
      `Article ${i} — study of ${pick(WORDS)}`,
      `<article>
      <h1>Article ${i}: a study of ${pick(WORDS)}</h1>
      <div class="byline">By Author ${String.fromCharCode(64 + i)}. Name</div>
      <time datetime="2026-0${i}-1${i}">2026-0${i}-1${i}</time>
      ${paragraphs(int(6, 12))}
      <blockquote>${SENTENCES[i]}</blockquote>
      ${paragraphs(int(2, 5))}
    </article>${i % 2 === 0 ? noise : ""}`,
      `<meta name="author" content="Author ${String.fromCharCode(64 + i)}. Name"><meta property="article:published_time" content="2026-0${i}-1${i}T10:00:00Z">`,
    ),
  });
}

// 2. docs pages (6) — headings hierarchy + lists
for (let i = 1; i <= 6; i++) {
  corpus.push({
    name: `docs-${i}.html`,
    html: page(
      `Handbook §${i}: ${pick(WORDS)}`,
      `<main>
      <h1>Handbook section ${i}</h1>
      <p>Introduction paragraph. ${SENTENCES[i + 3]}</p>
      <h2>Installation</h2>
      <p>${paragraph(2)}</p>
      <ul><li>Step one ${pick(WORDS)}</li><li>Step two ${pick(WORDS)}</li><li>Step three ${pick(WORDS)}</li></ul>
      <h2>Configuration</h2>
      <p>${paragraph(2)}</p>
      <ol><li>First option</li><li>Second option</li><li>Third option</li></ol>
      <h3>Advanced</h3>
      ${paragraphs(2)}
    </main>`,
    ),
  });
}

// 3. forum threads (6) — posts + replies (comment-ish structure)
for (let i = 1; i <= 6; i++) {
  const replies = Array.from({ length: int(3, 7) }, (_, r) => `<div class="comment"><div class="comment-author">user${r}</div><p>${paragraph(int(1, 3))}</p></div>`).join("\n      ");
  corpus.push({
    name: `forum-${i}.html`,
    html: page(
      `Discussion: ${pick(WORDS)} thread ${i}`,
      `<div class="thread">
      <h1>Discussion thread ${i}</h1>
      <div class="op"><p>${paragraph(int(4, 8))}</p></div>
      <div class="comments">${replies}</div>
    </div>${noise}`,
    ),
  });
}

// 4. code-heavy pages (6)
for (let i = 1; i <= 6; i++) {
  corpus.push({
    name: `code-${i}.html`,
    html: page(
      `Snippet collection ${i}`,
      `<article>
      <h1>Snippet collection ${i}</h1>
      <p>${paragraph(2)}</p>
      <pre><code>fn main() {
    let value = ${i};
    println!("case {value}");
}</code></pre>
      <p>${paragraph(1)}</p>
      <pre><code class="language-python">def run(n=${i}):
    return [x * n for x in range(20)]</code></pre>
      <p>${paragraph(2)}</p>
    </article>`,
    ),
  });
}

// 5. table pages (6)
for (let i = 1; i <= 6; i++) {
  const rows = Array.from({ length: int(6, 12) }, (_, r) => `<tr><td>${pick(WORDS)}</td><td>${int(1, 999)}</td><td>${pick(WORDS)}</td></tr>`).join("\n        ");
  corpus.push({
    name: `table-${i}.html`,
    html: page(
      `Benchmark results ${i}`,
      `<article>
      <h1>Benchmark results ${i}</h1>
      <p>${paragraph(2)}</p>
      <table>
        <thead><tr><th>Case</th><th>Score</th><th>Notes</th></tr></thead>
        <tbody>${rows}</tbody>
      </table>
      <p>${paragraph(2)}</p>
    </article>`,
    ),
  });
}

// 6. footnote + multilingual + edge pages (6)
corpus.push({
  name: "footnote-1.html",
  html: page(
    "Citations and footnotes",
    `<article>
      <h1>Citations and footnotes</h1>
      ${paragraphs(6)}
      <hr>
      <ol class="footnotes"><li id="fn1">First source note. ${SENTENCES[0]}</li><li id="fn2">Second source note.</li><li id="fn3">Third source note. ${SENTENCES[5]}</li></ol>
    </article>`,
  ),
});
corpus.push({
  name: "footnote-2.html",
  html: page(
    "Long form with notes",
    `<article>
      <h1>Long form with notes</h1>
      ${paragraphs(9)}
      <div class="references"><p>[1] Reference one.</p><p>[2] Reference two. ${SENTENCES[9]}</p></div>
    </article>`,
  ),
});
corpus.push({
  name: "lang-ja.html",
  html: `<!DOCTYPE html>\n<html lang="ja">\n<head><meta charset="utf-8"><title>日本語の記事</title><meta property="og:site_name" content="日本語サイト"></head>\n<body><article><h1>構造化された記事の見出し</h1><p>これは日本語の本文です。抽出エンジンの動作を確認するためのサンプルテキストがここに入ります。段落が複数あることで可読性スコアが上がります。</p><p>二番目の段落にはさらに文章が続きます。指標の計算に十分な語数を確保するためもう一度同じような文を繰り返します。</p></article></body>\n</html>\n`,
});
corpus.push({
  name: "lang-de.html",
  html: page(
    "Deutscher Artikel",
    `<article><h1>Ein strukturierter Artikel</h1><div class="byline">Von Autor Nommer</div><p>Dies ist ein deutscher Beispielsatz für die Extraktion. Weitere Sätze folgen hier, damit die Bewertung genügt.</p><p>Ein zweiter Abschnitt mit zusätzlichen Inhalten und ausreichend Wörtern für die Metrik.</p></article>`,
  ),
});
corpus.push({
  name: "edge-no-title.html",
  html: `<!DOCTYPE html>\n<html><head><meta charset="utf-8"></head><body><article><p>${paragraph(8)}</p></article></body></html>\n`,
});
corpus.push({
  name: "edge-thin.html",
  html: page("Thin page", `<div><h1>Thin</h1><p>Only one short line.</p></div>`),
});

for (const { name, html } of corpus) {
  writeFileSync(join(OUT, name), html);
}
console.log(`wrote ${corpus.length} corpus pages to ${OUT}`);
