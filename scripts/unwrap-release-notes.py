#!/usr/bin/env python3
"""changes/v<version>.md -> unwrapped release notes (stdout).

Since the 2026-09-05 formatting convention the repo's markdown sources use
single-line paragraphs, so this pass is normally a no-op. It stays as an
idempotent normalizer: GitHub renders release-note bodies like issue
comments — a single newline becomes <br> — so any legacy hard-wrapped text
(v0.1.x-era files, pasted content) is still reflowed onto one line here.

Rules:
- Blank lines, headings, code fences and table rows are emitted verbatim
  and end the current block.
- A list / ordered item starts an accumulatable block: subsequent plain
  lines join it (hard-wrapped continuations), preserving the marker.
- Joining at the wrap point: no separator between CJK and CJK (incl. CJK
  punctuation), none after a trailing hyphen (a token split mid-word,
  e.g. `feat/statusline-` + `live-token-count`), and a single space
  otherwise — which keeps the author's CJK↔Latin spacing (盘古之白).

Usage: unwrap-release-notes.py <file.md>
"""

import sys

# CJK punctuation: CJK punctuation block, fullwidth forms, and the
# general-punctuation dashes/ellipses/quotes used inline (—— … " ").
PUNCT_RANGES = (
    (0x2014, 0x2027),   # — ‖ ‗ ' ' ‛ " " † ‡ • … ‥ ‧
    (0x2018, 0x201F),   # ' ' " " (kept inside the range above too)
    (0x3000, 0x303F),   # CJK punctuation ，。《》：；
    (0xFF00, 0xFFEF),   # fullwidth forms ！？，（）
)

# CJK ideographs: radicals + extensions + Unified Ideographs.
IDEO_RANGES = (
    (0x2E80, 0x2FDF),   # CJK radicals / Kangxi
    (0x3400, 0x4DBF),   # extension A
    (0x4E00, 0x9FFF),   # Unified Ideographs
    (0x20000, 0x2FA1F), # extensions B.. compatibility
)


def in_ranges(char: str, ranges: tuple[tuple[int, int], ...]) -> bool:
    code = ord(char)
    return any(low <= code <= high for low, high in ranges)


def join(left: str, right: str) -> str:
    """Join two fragments at a former wrap point (see module docstring)."""
    if not left or not right:
        return left + right
    if left.endswith("-"):
        return left + right  # token split mid-word across the wrap
    if in_ranges(left[-1], PUNCT_RANGES) or in_ranges(right[0], PUNCT_RANGES):
        return left + right  # CJK punctuation binds without a space
    if in_ranges(left[-1], IDEO_RANGES) and in_ranges(right[0], IDEO_RANGES):
        return left + right  # CJK runs wrap with no space in the source
    return left + " " + right  # CJK ideograph <-> Latin keeps its space


def classify(line: str) -> str:
    """blank | hard (verbatim block) | item (accumulatable start) | text."""
    stripped = line.strip()
    if not stripped:
        return "blank"
    if stripped.startswith(("#", "```", "|")):
        return "hard"
    head = stripped.split(" ", 1)[0]
    if head in ("-", "*", "+"):
        return "item"
    if len(head) > 1 and head.endswith(".") and head[:-1].isdigit():
        return "item"  # ordered item: "1." "12." …
    return "text"


def unwrap(text: str) -> str:
    out: list[str] = []
    current = ""

    def flush() -> None:
        nonlocal current
        if current:
            out.append(current)
            current = ""

    for line in text.splitlines():
        kind = classify(line)
        if kind == "blank":
            flush()
            out.append("")
        elif kind == "hard":
            flush()
            out.append(line.rstrip())
        elif kind == "item":
            flush()
            current = line.strip()
        else:  # continuation of the current paragraph / item
            current = join(current, line.strip()) if current else line.strip()
    flush()
    return "\n".join(out).rstrip() + "\n"


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    # Windows runners default redirected stdout to the cp1252 locale codec
    # (UnicodeEncodeError on CJK notes) and translate \n to \r\n; force
    # UTF-8 with LF so the emitted release notes are byte-identical on
    # every target. reconfigure is 3.7+; stdout without it (e.g. a
    # StringIO under test) already behaves.
    try:
        sys.stdout.reconfigure(encoding="utf-8", newline="\n")
    except AttributeError:
        pass
    with open(sys.argv[1], encoding="utf-8") as handle:
        print(unwrap(handle.read()), end="")
    return 0


if __name__ == "__main__":
    sys.exit(main())
