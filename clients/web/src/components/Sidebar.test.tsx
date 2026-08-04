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
import { Sidebar } from "./Sidebar";

// jsdom has no window.matchMedia, which useTheme reads at module scope via
// ThemeToggle; the theme toggle is irrelevant to grouping.
vi.mock("./ThemeToggle", () => ({ ThemeToggle: () => null }));

vi.mock("../api/client", () => ({
  api: {
    sessions: {
      list: vi.fn(),
      setAnnotations: vi.fn(),
    },
    sessionGroups: {
      list: vi.fn(),
      create: vi.fn(),
      rename: vi.fn(),
      remove: vi.fn(),
    },
    globalEventsUrl: () => "/api/events",
  },
}));

function renderSidebar() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(
    <QueryClientProvider client={client}>
      <MemoryRouter>
        <Sidebar />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

function session(id: string, group?: string): SessionSummary {
  return {
    id,
    name: `session ${id}`,
    createdAt: 1,
    annotations: group ? [{ key: "group", value: group }] : [],
  };
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  localStorage.clear();
});

describe("Sidebar groups", () => {
  it("renders union sections: registered, annotation-only, ungrouped", async () => {
    vi.mocked(api.sessionGroups.list).mockResolvedValue({
      groups: [{ name: "api" }],
    });
    vi.mocked(api.sessions.list).mockResolvedValue({
      sessions: [session("1", "web"), session("2")],
    });
    renderSidebar();
    await waitFor(() => {
      expect(screen.getByTestId("group-section-api")).toBeDefined();
      expect(screen.getByTestId("group-section-web")).toBeDefined();
      expect(screen.getByTestId("group-section-ungrouped")).toBeDefined();
    });
    expect(screen.getByTestId("group-section-ungrouped").textContent).toContain(
      "session 2",
    );
  });

  it("stays flat until a group exists: no Ungrouped header, no row menu", async () => {
    vi.mocked(api.sessionGroups.list).mockResolvedValue({ groups: [] });
    vi.mocked(api.sessions.list).mockResolvedValue({
      sessions: [session("1")],
    });
    renderSidebar();
    await screen.findByTestId("session-row");
    expect(screen.queryByLabelText("Collapse Ungrouped")).toBeNull();
    expect(screen.queryByTestId("session-row-menu-1")).toBeNull();
  });

  it("creates a group from the header button", async () => {
    vi.mocked(api.sessionGroups.list).mockResolvedValue({ groups: [] });
    vi.mocked(api.sessionGroups.create).mockResolvedValue({
      group: { name: "web" },
    });
    vi.mocked(api.sessions.list).mockResolvedValue({ sessions: [] });
    renderSidebar();
    fireEvent.click(await screen.findByTestId("new-group-button"));
    fireEvent.change(screen.getByTestId("group-name-input"), {
      target: { value: "web" },
    });
    fireEvent.keyDown(screen.getByTestId("group-name-input"), { key: "Enter" });
    await waitFor(() =>
      expect(api.sessionGroups.create).toHaveBeenCalledWith("web"),
    );
  });

  it("moves a session to a group from the row menu", async () => {
    vi.mocked(api.sessionGroups.list).mockResolvedValue({
      groups: [{ name: "web" }],
    });
    vi.mocked(api.sessions.list).mockResolvedValue({
      sessions: [session("1")],
    });
    vi.mocked(api.sessions.setAnnotations).mockResolvedValue({});
    renderSidebar();
    fireEvent.click(await screen.findByTestId("session-row-menu-1"));
    fireEvent.click(screen.getByTestId("move-to-group-web"));
    await waitFor(() =>
      expect(api.sessions.setAnnotations).toHaveBeenCalledWith("1", {
        set: [{ key: "group", value: "web" }],
        remove: [],
      }),
    );
  });

  it("deletes a group after the two-step confirm", async () => {
    vi.mocked(api.sessionGroups.list).mockResolvedValue({
      groups: [{ name: "web" }],
    });
    vi.mocked(api.sessionGroups.remove).mockResolvedValue({});
    vi.mocked(api.sessions.list).mockResolvedValue({ sessions: [] });
    renderSidebar();
    fireEvent.click(await screen.findByTestId("group-menu-button-web"));
    fireEvent.click(screen.getByTestId("delete-group-item"));
    fireEvent.click(await screen.findByTestId("group-menu-button-web"));
    fireEvent.click(screen.getByTestId("confirm-delete-group-item"));
    await waitFor(() =>
      expect(api.sessionGroups.remove).toHaveBeenCalledWith("web"),
    );
  });
});
