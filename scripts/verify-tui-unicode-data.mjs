// Full-code-point drift guard for the TERMINAL_SPACING_MARK_RANGES table in
// crates/rpi-tui/src/utils.rs (v0.11 R5.3.2, design risk register #5:
// "code-point-level diff, no sampling").
//
// Ground truth is the pinned runtime's evaluation of the upstream regex
// (packages/tui/src/utils.ts:46-47, dfe47d3fb @ 4181f66) — the same regex
// icu-probe4.mjs uses to generate the table. This script re-probes every
// code point, parses the checked-in Rust table, and diffs the two sets.
//
// Usage: node scripts/verify-tui-unicode-data.mjs
// Exit 0 = table matches the runtime exactly; exit 1 = drift (regenerate
// with scripts/gen-tui-unicode-data.py after running the icu-probe chain).
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const utilsRs = fs.readFileSync(
  path.join(repoRoot, 'crates/rpi-tui/src/utils.rs'),
  'utf8',
);

// Verbatim copy of terminalSpacingMarkRegex (packages/tui/src/utils.ts:46-47);
// written as a RegExp string (double-escaped) so the file stays pure ASCII.
const terminalSpacingMark = new RegExp(
  '^(?:[\\p{Spacing_Mark}--[\\u1734\\u302E\\u302F]]' +
    '|[\\u065F\\u0F7F\\u102B\\u102C\\u1031\\u1033-\\u1035\\u1038\\u103A-\\u103E])+$',
  'v',
);

// Parse the checked-in Rust table: `static TERMINAL_SPACING_MARK_RANGES:
// &[(u32, u32)] = &[ (0x...., 0x....), ... ];`
const tableMatch = utilsRs.match(
  /static TERMINAL_SPACING_MARK_RANGES[^=]*= &\[([\s\S]*?)\];/,
);
if (!tableMatch) {
  console.error('TERMINAL_SPACING_MARK_RANGES not found in utils.rs');
  process.exit(1);
}
const ranges = [...tableMatch[1].matchAll(/\((0x[0-9a-fA-F]+),\s*(0x[0-9a-fA-F]+)\)/g)].map(
  (m) => [parseInt(m[1], 16), parseInt(m[2], 16)],
);
if (ranges.length === 0) {
  console.error('TERMINAL_SPACING_MARK_RANGES parsed as empty — parser broken?');
  process.exit(1);
}

const inTable = new Uint8Array(0x110000);
for (const [lo, hi] of ranges) {
  if (lo > hi || hi > 0x10ffff) {
    console.error(`invalid range (${lo.toString(16)}, ${hi.toString(16)})`);
    process.exit(1);
  }
  inTable.fill(1, lo, hi + 1);
}

let drifted = 0;
for (let cp = 0; cp <= 0x10ffff; cp++) {
  const expected = terminalSpacingMark.test(String.fromCodePoint(cp)) ? 1 : 0;
  if (inTable[cp] !== expected) {
    if (drifted < 20) {
      console.error(
        `U+${cp.toString(16).toUpperCase().padStart(4, '0')}: table=${inTable[cp]} runtime=${expected}`,
      );
    }
    drifted++;
  }
}

if (drifted > 0) {
  console.error(
    `DRIFT: ${drifted} code points differ. Regenerate via icu-probe4.mjs + gen-tui-unicode-data.py.`,
  );
  process.exit(1);
}
console.log(
  `OK: ${ranges.length} ranges, code-point-level parity with the runtime regex.`,
);
