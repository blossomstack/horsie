import { cleanup, render } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import Markdown from "./Markdown";

afterEach(cleanup);

/**
 * These pin the conditions under which highlight.js runs at all. Getting this
 * wrong hard-locked the browser tab permanently — see the note on `Markdown`.
 */
describe("Markdown highlighting", () => {
  const fenced = (lang: string, body: string) =>
    ["```" + lang, body, "```"].join("\n");

  it("highlights a fence that names its language", () => {
    const { container } = render(
      <Markdown text={fenced("js", "const a = 1;")} />,
    );
    // The same probe the negative cases use, so none of them can pass
    // vacuously against a selector that never matches.
    expect(container.querySelector(".hljs-keyword")).not.toBe(null);
  });

  it("leaves an unlabelled fence alone rather than trying every grammar", () => {
    // This is `detect: true` removed. Auto-detection ran every registered
    // grammar over every block, which is where the CPU went.
    const { container } = render(
      <Markdown text={fenced("", "const a = 1;")} />,
    );
    expect(container.querySelector(".hljs-keyword")).toBe(null);
    // Still rendered as code, just not coloured.
    expect(container.querySelector("pre code")).not.toBe(null);
  });

  it("does not highlight while the text is still streaming", () => {
    const { container } = render(
      <Markdown text={fenced("js", "const a = 1;")} highlight={false} />,
    );
    expect(container.querySelector(".hljs-keyword")).toBe(null);
    expect(container.querySelector("pre code")).not.toBe(null);
  });

  it("does not highlight a message past the size cap", () => {
    const huge = "x".repeat(41_000);
    const { container } = render(<Markdown text={fenced("js", huge)} />);
    expect(container.querySelector(".hljs-keyword")).toBe(null);
  });

  it("still renders ordinary markdown in every mode", () => {
    for (const highlight of [true, false]) {
      const { container, unmount } = render(
        <Markdown text="# Title\n\nsome **bold** text" highlight={highlight} />,
      );
      expect(container.querySelector("strong")?.textContent).toBe("bold");
      unmount();
    }
  });
});
