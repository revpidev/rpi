#!/usr/bin/env python3
r"""Generate Rust Unicode tables for rpi-tui utils.rs.

Data sources — the GROUND TRUTH is the pinned upstream runtime's behavior:
the regexes in utils.ts (`\p{Default_Ignorable_Code_Point}`, `\p{Mark}`,
`\p{Format}`, the cjkBreakRegex Script_Extensions union and `\p{RGI_Emoji}`)
are evaluated by the runtime's ICU (Node 24 / ICU 76). ICU 76 deviates from
the raw Unicode 16.0.0 UCD files in places (broader script ranges covering
unassigned gaps, some post-16.0 marks/emoji), so every property table below is
generated from probes of the actual runtime (see icu-probe*.mjs in this
directory):

  icu-di.bin / icu-mark.bin / icu-cf.bin   per-code-point regex probes
  icu-cjk.bin                              cjkBreakRegex probe
  icu-rgiSingle.bin                        /^\p{RGI_Emoji}$/ on single cps
  icu-rgi-sequences.txt                    RGI sequence matches (16.0 files
                                           probed against the runtime; the
                                           matched sets are byte-identical to
                                           the 16.0 file contents)

The UCD 16.0 files (https://www.unicode.org/Public/16.0.0/) are used as the
cross-check reference; the emoji sequence files
(https://www.unicode.org/Public/emoji/16.0/) provide the candidate sequences.

Run chain (probes first; each regenerates the .bin / .txt inputs under the
probe dir):
  node scripts/icu-probe.mjs     # cjk / zeroWidth / leading / rgiSingle
  node scripts/icu-probe2.mjs    # di / mark / cf
  node scripts/icu-probe3.mjs    # RGI sequences (needs the u16-* UCD files)
  node scripts/icu-probe4.mjs    # terminalSpacingMarkRegex (utils.ts:46-47)
  python3 scripts/gen-tui-unicode-data.py --out /tmp/generated-tables.rs
"""
import argparse
import re
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent


def load_probe(probe_dir, name):
    data = open(probe_dir / f'icu-{name}.bin', 'rb').read()
    return [i for i, b in enumerate(data) if b]


def ranges_from_cps(cps):
    out = []
    for cp in cps:
        if out and cp == out[-1][1] + 1:
            out[-1] = (out[-1][0], cp)
        else:
            out.append((cp, cp))
    return out


def parse_sequences(path):
    """Parse emoji sequence files into {section: [sequences]}."""
    sections = {}
    current = None
    for line in open(path, encoding='utf-8'):
        m = re.match(r'^# ([A-Za-z_]+)', line)
        if m and m.group(1) in ('Basic_Emoji', 'Emoji_Keycap_Sequence',
                                'RGI_Emoji_Flag_Sequence', 'RGI_Emoji_Tag_Sequence',
                                'RGI_Emoji_Modifier_Sequence', 'RGI_Emoji_ZWJ_Sequence'):
            current = m.group(1)
            sections.setdefault(current, [])
            continue
        line = line.split('#')[0].strip()
        if not line or current is None:
            continue
        fields = line.split(';')[0].split()
        has_range = any('..' in x for x in fields)
        cps = []
        for x in fields:
            if '..' in x:
                lo, hi = (int(y, 16) for y in x.split('..'))
                cps.extend(range(lo, hi + 1))
            else:
                cps.append(int(x, 16))
        if has_range:
            for c in cps:
                sections[current].append([c])
        else:
            sections[current].append(cps)
    return sections


def fmt_ranges(name, ranges, per_line=6):
    lines = [f"static {name}: &[(u32, u32)] = &["]
    for i in range(0, len(ranges), per_line):
        chunk = ranges[i:i + per_line]
        items = ", ".join(
            f"(0x{s:04X}, 0x{e:04X})" if s != e else f"(0x{s:04X}, 0x{s:04X})"
            for s, e in chunk)
        lines.append(f"    {items},")
    lines.append("];")
    return "\n".join(lines)


def fmt_seqlist(name, seqs):
    lines = [f"static {name}: &[&[u32]] = &["]
    for seq in seqs:
        items = ", ".join(f"0x{c:X}" for c in seq)
        lines.append(f"    &[{items}],")
    lines.append("];")
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--probe-dir', type=Path,
                        default=SCRIPT_DIR / 'probes',
                        help='directory with the icu-*.bin probes and '
                             'icu-rgi-sequences.txt (default: scripts/probes)')
    parser.add_argument('--ucd-dir', type=Path,
                        default=SCRIPT_DIR / 'probes',
                        help='directory with the Unicode 16.0 UCD files '
                             '(u16-emoji-sequences.txt etc.; default: --probe-dir)')
    parser.add_argument('--out', type=Path, required=True,
                        help='output .rs file (e.g. generated-tables.rs)')
    args = parser.parse_args()
    probe_dir = args.probe_dir
    ucd_dir = args.ucd_dir

    out = []
    out.append("// @generated — DO NOT EDIT BY HAND.")
    out.append("// Source: probes of the pinned upstream runtime (Node 24 / ICU 76) plus")
    out.append("// the Unicode 16.0 emoji sequence files; see utils.rs header notes and")
    out.append("// the T11 port report. Regenerated by scripts/gen-tui-unicode-data.py")
    out.append("// (probes via scripts/icu-probe*.mjs; inputs under scripts/probes/).")

    # 1-4. property probes
    di = ranges_from_cps(load_probe(probe_dir, 'di'))
    mark = ranges_from_cps(load_probe(probe_dir, 'mark'))
    cf = ranges_from_cps(load_probe(probe_dir, 'cf'))
    scx = ranges_from_cps(load_probe(probe_dir, 'cjk'))
    out.append("// Default_Ignorable_Code_Point as evaluated by the runtime (zeroWidthRegex / leadingNonPrintingRegex).")
    out.append(fmt_ranges("DEFAULT_IGNORABLE_RANGES", di))
    out.append("// General_Category=Mark (Mn/Mc/Me) as evaluated by the runtime (zeroWidthRegex / leadingNonPrintingRegex).")
    out.append(fmt_ranges("MARK_RANGES", mark))
    out.append("// General_Category=Format (Cf) as evaluated by the runtime (leadingNonPrintingRegex only).")
    out.append(fmt_ranges("FORMAT_RANGES", cf))
    out.append("// cjkBreakRegex Script_Extensions union as evaluated by the runtime.")
    out.append(fmt_ranges("CJK_BREAK_RANGES", scx))
    tsm = ranges_from_cps(load_probe(probe_dir, 'terminalSpacingMark'))
    out.append("// terminalSpacingMarkRegex (utils.ts:46-47, dfe47d3fb) as evaluated by the runtime:")
    out.append("// [\\p{Spacing_Mark}--[\\u1734\\u302E\\u302F]] plus the 14 non-spacing exceptions.")
    out.append(fmt_ranges("TERMINAL_SPACING_MARK_RANGES", tsm))

    # 5. RGI singles (Emoji_Presentation per the runtime)
    ep = ranges_from_cps(load_probe(probe_dir, 'rgiSingle'))
    out.append("// Single-codepoint RGI_Emoji as evaluated by the runtime.")
    out.append(fmt_ranges("EMOJI_PRESENTATION_RANGES", ep))

    # 6. sequences: candidates from the Unicode 16.0 files, verified against the runtime probe
    probe_seqs = {}
    for line in open(probe_dir / 'icu-rgi-sequences.txt', encoding='utf-8'):
        sec, seq = line.rstrip('\n').split('\t')
        probe_seqs.setdefault(sec, set()).add(seq)

    sections = parse_sequences(ucd_dir / 'u16-emoji-sequences.txt')
    basic = sections['Basic_Emoji']
    keycap = sections['Emoji_Keycap_Sequence']
    flags = sections['RGI_Emoji_Flag_Sequence']
    tags = sections['RGI_Emoji_Tag_Sequence']
    mods = sections['RGI_Emoji_Modifier_Sequence']
    zwj = parse_sequences(ucd_dir / 'u16-emoji-zwj-sequences.txt')['RGI_Emoji_ZWJ_Sequence']

    def seqkey(seq):
        return ','.join(str(c) for c in seq)

    # verify: the candidate sets match the runtime probe exactly
    assert set(seqkey(s) for s in basic) == probe_seqs.get('Basic_Emoji', set())
    assert set(seqkey(s) for s in keycap) == probe_seqs.get('Emoji_Keycap_Sequence', set())
    assert set(seqkey(s) for s in flags) == probe_seqs.get('RGI_Emoji_Flag_Sequence', set())
    assert set(seqkey(s) for s in tags) == probe_seqs.get('RGI_Emoji_Tag_Sequence', set())
    assert set(seqkey(s) for s in mods) == probe_seqs.get('RGI_Emoji_Modifier_Sequence', set())
    assert set(seqkey(s) for s in zwj) == probe_seqs.get('RGI_Emoji_ZWJ_Sequence', set())
    print("sequence sets verified against runtime probe")

    basic_bases = sorted(set(s[0] for s in basic))
    mod_bases = sorted(set(s[0] for s in mods))
    assert len(basic_bases) == 1393
    assert len(mod_bases) == 131
    assert len(flags) == 259
    assert len(tags) == 3
    assert len(keycap) == 12
    assert len(zwj) == 1468

    out.append("// Basic_Emoji bases (base + U+FE0F sequences) per the runtime probe.")
    out.append(fmt_ranges("EMOJI_BASIC_BASES", ranges_from_cps(basic_bases)))
    out.append("// Emoji modifier bases (base + U+1F3FB..U+1F3FF) per the runtime probe.")
    out.append(fmt_ranges("EMOJI_MODIFIER_BASES", ranges_from_cps(mod_bases)))
    out.append("// RGI flag sequences: RI pairs per the runtime probe.")
    out.append(fmt_ranges("RGI_FLAG_PAIRS", [(s[0], s[1]) for s in flags]))
    out.append("// Keycap bases {# * 0-9}; keycap sequence = base + U+FE0F + U+20E3.")
    out.append(fmt_ranges("KEYCAP_BASES", [(s[0], s[0]) for s in keycap]))
    out.append("// RGI tag sequences (3 subdivision flags) per the runtime probe.")
    out.append(fmt_seqlist("RGI_TAG_SEQUENCES", sorted(tags)))
    out.append("// RGI ZWJ sequences per the runtime probe (sorted lexicographically;")
    out.append("// the source file is NOT in lexicographic order — binary search requires it).")
    out.append(fmt_seqlist("RGI_ZWJ_SEQUENCES", sorted(zwj)))

    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text("\n".join(out) + "\n")
    print("OK")
    print("DI:", len(di), "Mark:", len(mark), "Cf:", len(cf), "scxCJK:", len(scx), "EP:", len(ep), "TSM:", len(tsm))


if __name__ == '__main__':
    main()
