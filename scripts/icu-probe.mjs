// Probe the actual Unicode property behavior of the runtime (Node 24 / ICU 76)
// for every code point, and dump bitsets for the properties used by utils.ts.
// Part of the pir-tui utils.rs Unicode table generation chain
// (scripts/gen-tui-unicode-data.py). Run with `node scripts/icu-probe.mjs`.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const outDir = process.argv[2] ?? path.join(path.dirname(fileURLToPath(import.meta.url)), 'probes');

const cjk = /[\p{Script_Extensions=Han}\p{Script_Extensions=Hiragana}\p{Script_Extensions=Katakana}\p{Script_Extensions=Hangul}\p{Script_Extensions=Bopomofo}]/u;
const zeroWidth = /^(?:\p{Default_Ignorable_Code_Point}|\p{Control}|\p{Mark}|\p{Surrogate})+$/v;
const leading = /^[\p{Default_Ignorable_Code_Point}\p{Control}\p{Format}\p{Mark}\p{Surrogate}]+/v;
const rgiSingle = /^\p{RGI_Emoji}$/v;

const props = { cjk, zeroWidth, leading, rgiSingle };
const buffers = {};
for (const name of Object.keys(props)) {
  buffers[name] = Buffer.alloc(0x110000);
}

for (let cp = 0; cp <= 0x10ffff; cp++) {
  const s = String.fromCodePoint(cp);
  for (const [name, re] of Object.entries(props)) {
    // test on the single cp; for zeroWidth/leading the regex is per-char anyway
    buffers[name][cp] = re.test(s) ? 1 : 0;
  }
  if (cp % 0x10000 === 0xffff) process.stderr.write(`cp ${cp}\n`);
}

fs.mkdirSync(outDir, { recursive: true });
for (const [name, b] of Object.entries(buffers)) {
  fs.writeFileSync(path.join(outDir, `icu-${name}.bin`), b);
}
console.log('done');
