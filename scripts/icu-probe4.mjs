// Probe the actual terminalSpacingMarkRegex behavior of the runtime
// (Node 24 / ICU) for every code point, and dump a bitset.
// Part of the rpi-tui utils.rs Unicode table generation chain
// (scripts/gen-tui-unicode-data.py). Run with `node scripts/icu-probe4.mjs`.
//
// The regex is copied verbatim from packages/tui/src/utils.ts:46-47
// (dfe47d3fb). The upstream regex quantifies over a character class (`+`),
// so a whole-cluster match is equivalent to "every code point is in the
// class"; probing per code point captures the full semantics.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const outDir = process.argv[2] ?? path.join(path.dirname(fileURLToPath(import.meta.url)), 'probes');

// Verbatim copy of terminalSpacingMarkRegex (packages/tui/src/utils.ts:46-47);
// written as a RegExp string (double-escaped) so the file stays pure ASCII.
const terminalSpacingMark = new RegExp(
  '^(?:[\\p{Spacing_Mark}--[\\u1734\\u302E\\u302F]]' +
    '|[\\u065F\\u0F7F\\u102B\\u102C\\u1031\\u1033-\\u1035\\u1038\\u103A-\\u103E])+$',
  'v',
);

const buffer = Buffer.alloc(0x110000);
for (let cp = 0; cp <= 0x10ffff; cp++) {
  buffer[cp] = terminalSpacingMark.test(String.fromCodePoint(cp)) ? 1 : 0;
}
fs.mkdirSync(outDir, { recursive: true });
fs.writeFileSync(path.join(outDir, 'icu-terminalSpacingMark.bin'), buffer);
console.log('done, matched code points:', buffer.reduce((a, b) => a + b, 0));
