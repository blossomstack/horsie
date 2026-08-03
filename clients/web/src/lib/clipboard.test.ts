import { describe, expect, it } from "vitest";
import { renderedTextOf } from "./clipboard";

/** Builds a turn's DOM the way `Transcript` does: prose segments carry
 * `data-prose-segment`, everything else is tool traffic. */
function turn(html: string): HTMLElement {
  const el = document.createElement("div");
  el.innerHTML = html;
  return el;
}

describe("renderedTextOf", () => {
  it("takes only the prose, not the tool traffic around it", () => {
    // The bug this exists for: reading the container wholesale meant "copy as
    // plain text" returned tool-call names and work-group summaries that the
    // markdown button never included, so the two buttons disagreed.
    const el = turn(`
      <div data-prose-segment=""><p>First paragraph.</p></div>
      <div data-testid="work-group"><span>Ran 2 tools · 1.2s</span></div>
      <div data-prose-segment=""><p>Second paragraph.</p></div>
    `);
    const out = renderedTextOf(el);
    expect(out).toContain("First paragraph.");
    expect(out).toContain("Second paragraph.");
    expect(out).not.toContain("Ran 2 tools");
  });

  it("separates segments so they do not run together", () => {
    const el = turn(
      `<div data-prose-segment=""><p>One.</p></div>` +
        `<div data-prose-segment=""><p>Two.</p></div>`,
    );
    expect(renderedTextOf(el)).toBe("One.\n\nTwo.");
  });

  it("falls back to the whole node when nothing is marked as prose", () => {
    // A user turn has no prose segments — its bubble is the content.
    expect(renderedTextOf(turn("<div>Just the message.</div>"))).toBe(
      "Just the message.",
    );
  });

  it("collapses runs of blank lines and trims", () => {
    const el = turn(`<div data-prose-segment="">\n\n\n  Padded.  \n\n\n</div>`);
    expect(renderedTextOf(el)).toBe("Padded.");
  });

  it("returns empty for a missing node rather than throwing", () => {
    expect(renderedTextOf(null)).toBe("");
  });
});
