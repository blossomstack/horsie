import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import { api } from "../api/client";
import type { SessionSummary } from "../api/types";
import { SessionStatusKind } from "../api/types";
import { SessionRow } from "./SessionRow";

vi.mock("../api/client", () => ({
  api: { sessions: { setAnnotations: vi.fn(), remove: vi.fn() } },
}));

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

const tagged: SessionSummary = {
  id: "s1",
  name: "one",
  status: SessionStatusKind.Idle,
  createdAt: 1,
  annotations: [{ key: "tag.web", value: "" }],
  forks: [],
};

function row(s: SessionSummary, tags: string[]) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(
    <QueryClientProvider client={client}>
      <MemoryRouter>
        <SessionRow s={s} tags={tags} />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("SessionRow tag menu", () => {
  it("unassigns a tag the session carries", async () => {
    vi.mocked(api.sessions.setAnnotations).mockResolvedValue({});
    row(tagged, ["web", "api"]);
    fireEvent.click(screen.getByTestId("session-row-menu-s1"));
    fireEvent.click(screen.getByTestId("toggle-tag-web"));
    await waitFor(() =>
      expect(api.sessions.setAnnotations).toHaveBeenCalledWith("s1", {
        set: [],
        remove: ["tag.web"],
      }),
    );
  });

  it("assigns a tag the session lacks", async () => {
    vi.mocked(api.sessions.setAnnotations).mockResolvedValue({});
    row(tagged, ["web", "api"]);
    fireEvent.click(screen.getByTestId("session-row-menu-s1"));
    fireEvent.click(screen.getByTestId("toggle-tag-api"));
    await waitFor(() =>
      expect(api.sessions.setAnnotations).toHaveBeenCalledWith("s1", {
        set: [{ key: "tag.api", value: "" }],
        remove: [],
      }),
    );
  });

  it("stays open across toggles, so two tags are one edit", () => {
    vi.mocked(api.sessions.setAnnotations).mockResolvedValue({});
    row(tagged, ["web", "api"]);
    fireEvent.click(screen.getByTestId("session-row-menu-s1"));
    fireEvent.click(screen.getByTestId("toggle-tag-api"));
    expect(screen.getByTestId("toggle-tag-web")).toBeTruthy();
  });

  it("creates a tag from the input, normalising what was typed", async () => {
    vi.mocked(api.sessions.setAnnotations).mockResolvedValue({});
    row(tagged, ["web"]);
    fireEvent.click(screen.getByTestId("session-row-menu-s1"));
    const input = screen.getByTestId("new-tag-input");
    fireEvent.change(input, { target: { value: "Bug Fix" } });
    fireEvent.keyDown(input, { key: "Enter" });
    await waitFor(() =>
      expect(api.sessions.setAnnotations).toHaveBeenCalledWith("s1", {
        set: [{ key: "tag.bug-fix", value: "" }],
        remove: [],
      }),
    );
  });

  it("sends nothing for a name that normalises to nothing", () => {
    row(tagged, []);
    fireEvent.click(screen.getByTestId("session-row-menu-s1"));
    const input = screen.getByTestId("new-tag-input");
    fireEvent.change(input, { target: { value: "  !!  " } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(api.sessions.setAnnotations).not.toHaveBeenCalled();
  });
});
