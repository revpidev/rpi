// Minimal `@earendil-works/pi-tui` value stub for the renderCall parity leg
// (TE09 FR-E). `tool-result-renderer.ts` imports `Text` as a VALUE (it
// constructs `new Text(...)` in renderToolCallLines), unlike the protocol
// legs where pi-tui is type-only and stubbed to throw.
//
// The stub models exactly the behavior the plain-theme render path relies
// on: `new Text(joined, 0, 0).render(80)` returns the text split on
// newlines. Width wrapping is a pi-tui rendering concern outside the parity
// surface (the fixtures' render cases keep every line well under 80).

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
