import { cleanup, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it } from "vitest";
import { SubSessionMarker } from "./SubSessionMarker";

afterEach(cleanup);

function renderMarker(seed: string) {
  return render(
    <MemoryRouter>
      <SubSessionMarker
        value={{ id: "subSession-1", seed, atMs: 1 }}
        sessionId="s1"
      />
    </MemoryRouter>,
  );
}

describe("SubSessionMarker", () => {
  it("links to the sub session that branched off", () => {
    renderMarker("copy");
    expect(screen.getByRole("link").getAttribute("href")).toBe(
      "/sessions/s1/agents/subSession-1",
    );
  });

  /* The modes give the sub session genuinely different histories, so the marker says
     which — otherwise "branched from here" means three things. */
  it("says when the sub session got a summary rather than the history", () => {
    renderMarker("summary");
    expect(screen.getByRole("link").textContent).toMatch(/summary/);
    cleanup();
    renderMarker("copy");
    expect(screen.getByRole("link").textContent).not.toMatch(/summary/);
  });

  /* A fresh sub session carries nothing from here, so "branched from here" would claim
     a history it does not have. */
  it("says a fresh sub session was handed off rather than branched", () => {
    renderMarker("fresh");
    expect(screen.getByRole("link").textContent).toMatch(/handed off/);
    expect(screen.getByRole("link").textContent).not.toMatch(/branched/);
  });
});
