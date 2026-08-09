// Probe the actual Unicode property behavior of the runtime (Node 24 / ICU 76)
// for every code point, and dump bitsets for the properties used by utils.ts.
// Part of the rpi-tui utils.rs Unicode table generation chain
// (scripts/gen-tui-unicode-data.py). Run with `node scripts/icu-probe2.mjs`.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const outDir = process.argv[2] ?? path.join(path.dirname(fileURLToPath(import.meta.url)), 'probes');

const props = {
  di: /^\p{Default_Ignorable_Code_Point}$/u,
  mark: /^\p{Mark}$/u,
  cf: /^\p{Format}$/u,
};
const buffers = {};
for (const name of Object.keys(props)) buffers[name] = Buffer.alloc(0x110000);
for (let cp = 0; cp <= 0x10ffff; cp++) {
  const s = String.fromCodePoint(cp);
  for (const [name, re] of Object.entries(props)) {
    buffers[name][cp] = re.test(s) ? 1 : 0;
  }
}
fs.mkdirSync(outDir, { recursive: true });
for (const [name, b] of Object.entries(buffers)) {
  fs.writeFileSync(path.join(outDir, `icu-${name}.bin`), b);
}
console.log('done');
