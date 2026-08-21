import { cleanup, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it } from "vitest";
import { ForkMarker } from "./ForkMarker";

afterEach(cleanup);

function renderMarker(mode: string) {
  return render(
    <MemoryRouter>
      <ForkMarker
        value={{ id: "fork-1", mode, atMs: 1 }}
        sessionId="s1"
      />
    </MemoryRouter>,
  );
}

describe("ForkMarker", () => {
  it("links to the conversation that branched off", () => {
    renderMarker("copy");
    expect(screen.getByRole("link").getAttribute("href")).toBe(
      "/sessions/s1/agents/fork-1",
    );
  });

  /* The modes give the fork genuinely different histories, so the marker says
     which — otherwise "forked from here" means three things. */
  it("says when the fork got a summary rather than the history", () => {
    renderMarker("summary");
    expect(screen.getByRole("link").textContent).toMatch(/summary/);
    cleanup();
    renderMarker("copy");
    expect(screen.getByRole("link").textContent).not.toMatch(/summary/);
  });

  /* A fresh fork carries nothing from here, so "forked from here" would claim
     a history it does not have. */
  it("says a fresh fork was handed off rather than forked", () => {
    renderMarker("fresh");
    expect(screen.getByRole("link").textContent).toMatch(/handed off/);
    expect(screen.getByRole("link").textContent).not.toMatch(/forked/);
  });
});
