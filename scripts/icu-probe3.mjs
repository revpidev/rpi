// Probe which sequences ICU 76's \p{RGI_Emoji} matches, for all candidates
// from the Unicode 16.0 emoji sequence files plus the 7 extra single cps.
// Part of the rpi-tui utils.rs Unicode table generation chain
// (scripts/gen-tui-unicode-data.py). Run with:
//   node scripts/icu-probe3.mjs [ucd-dir] [out-dir]
// (ucd-dir holds u16-emoji-sequences.txt / u16-emoji-zwj-sequences.txt from
// https://www.unicode.org/Public/emoji/16.0/; both default to scripts/probes.)
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const ucdDir = process.argv[2] ?? path.join(scriptDir, 'probes');
const outDir = process.argv[3] ?? ucdDir;

const rgi = /^\p{RGI_Emoji}$/v;

function parseSeq(p, sectionNames) {
  const out = {};
  let cur = null;
  for (const line of fs.readFileSync(p, 'utf8').split('\n')) {
    const m = line.match(/^# ([A-Za-z_]+)/);
    if (m && sectionNames.includes(m[1])) { cur = m[1]; out[cur] = out[cur] || []; continue; }
    const l = line.split('#')[0].trim();
    if (!l || cur === null || !out[cur]) continue;
    const fields = l.split(';')[0].trim().split(/\s+/);
    const hasRange = fields.some(f => f.includes('..'));
    const cps = [];
    for (const f of fields) {
      if (f.includes('..')) {
        const [a, b] = f.split('..').map(x => parseInt(x, 16));
        cps.push(...Array.from({ length: b - a + 1 }, (_, i) => a + i));
      } else cps.push(parseInt(f, 16));
    }
    if (hasRange) { for (const c of cps) out[cur].push([c]); }
    else out[cur].push(cps);
  }
  return out;
}

const SECTIONS = ['Basic_Emoji', 'Emoji_Keycap_Sequence', 'RGI_Emoji_Flag_Sequence',
  'RGI_Emoji_Tag_Sequence', 'RGI_Emoji_Modifier_Sequence', 'RGI_Emoji_ZWJ_Sequence'];
const seqs = parseSeq(path.join(ucdDir, 'u16-emoji-sequences.txt'), SECTIONS);
const zwj = parseSeq(path.join(ucdDir, 'u16-emoji-zwj-sequences.txt'), SECTIONS);

// extra candidates: the 7 ICU-76 RGI singles probed earlier
const extraSingles = [0x1f6d8, 0x1fa8a, 0x1fa8e, 0x1fac8, 0x1facd, 0x1faea, 0x1faef];
for (const c of extraSingles) {
  seqs['Basic_Emoji'].push([c, 0xfe0f]);
  for (let t = 0x1f3fb; t <= 0x1f3ff; t++) seqs['RGI_Emoji_Modifier_Sequence'].push([c, t]);
  seqs['Emoji_Keycap_Sequence'].push([c, 0xfe0f, 0x20e3]);
}
// all RI pairs
for (let a = 0x1f1e6; a <= 0x1f1ff; a++)
  for (let b = 0x1f1e6; b <= 0x1f1ff; b++)
    seqs['RGI_Emoji_Flag_Sequence'].push([a, b]);

const all = new Map();
for (const [section, list] of Object.entries(seqs)) {
  for (const seq of list) {
    const s = String.fromCodePoint(...seq);
    if (rgi.test(s)) {
      all.set(seq.join(','), section);
    }
  }
}
for (const [section, list] of Object.entries(zwj)) {
  for (const seq of list) {
    const s = String.fromCodePoint(...seq);
    if (rgi.test(s)) all.set(seq.join(','), section);
  }
}
const out = [...all.entries()].map(([seq, section]) => `${section}\t${seq}`).sort();
fs.mkdirSync(outDir, { recursive: true });
fs.writeFileSync(path.join(outDir, 'icu-rgi-sequences.txt'), out.join('\n') + '\n');
console.log('matched sequences:', out.length);
const bySec = {};
for (const l of out) { const sec = l.split('\t')[0]; bySec[sec] = (bySec[sec] || 0) + 1; }
console.log(bySec);
