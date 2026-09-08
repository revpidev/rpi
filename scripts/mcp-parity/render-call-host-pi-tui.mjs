// Minimal `@earendil-works/pi-tui` value stub for the renderCall parity leg
// (TE09 FR-E; width helpers added for the v2.32.1 target track, TE13).
//
// `tool-result-renderer.ts` imports `Text` as a VALUE (it constructs
// `new Text(...)` in renderToolCallLines), unlike the protocol legs where
// pi-tui is type-only and stubbed to throw. v2.32.1 additionally imports the
// width helpers `truncateToWidth` / `visibleWidth` at runtime for its
// hidden/truncated preview path.
//
// The stub models exactly the behavior the plain-theme render path relies
// on: `new Text(joined, 0, 0).render(80)` returns the text split on
// newlines, and the width helpers implement pi-tui's printable-ASCII fast
// path (dist/utils.js:952-977). Width wrapping/ANSI/wide-grapheme handling
// is a pi-tui rendering concern outside the parity surface; the fixtures'
// render cases stay printable-ASCII and well under 80 columns. If TE24
// re-records render vectors with ANSI/wide text, switch this mapping to the
// real `@earendil-works/pi-tui` from the target dependency root (see
// TARGET-TRACK.md §4.3).

export class Text {
	constructor(text, paddingX = 0, paddingY = 0) {
		this.text = String(text ?? "");
		this.paddingX = paddingX;
		this.paddingY = paddingY;
	}
	render(width) {
		return this.text.split("\n");
	}
}

export function visibleWidth(text) {
	return String(text ?? "").length;
}

export function truncateToWidth(text, maxWidth, ellipsis = "...", pad = false) {
	const value = String(text ?? "");
	if (maxWidth <= 0) return "";
	if (value.length === 0) return pad ? " ".repeat(maxWidth) : "";
	if (value.length <= maxWidth) {
		return pad ? value + " ".repeat(maxWidth - value.length) : value;
	}
	const targetWidth = maxWidth - ellipsis.length;
	if (targetWidth <= 0) return ellipsis.slice(0, maxWidth);
	return value.slice(0, targetWidth) + ellipsis;
}
