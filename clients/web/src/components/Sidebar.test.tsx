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
import { answerConfirm, confirmSnapshot, resetConfirm } from "../lib/confirm";
import type { SessionSummary } from "../api/types";
import { SessionStatusKind } from "../api/types";
import { Sidebar } from "./Sidebar";

// jsdom has no window.matchMedia, which useTheme reads at module scope via
// ThemeToggle; the theme toggle is irrelevant to grouping.
vi.mock("./ThemeToggle", () => ({ ThemeToggle: () => null }));

vi.mock("../api/client", () => ({
  api: {
    sessions: {
      list: vi.fn(),
      setAnnotations: vi.fn(),
      remove: vi.fn(),
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
    status: SessionStatusKind.Idle,
    createdAt: 1,
    annotations: group ? [{ key: "group", value: group }] : [],
  };
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  localStorage.clear();
  // The confirm store is module-level, so it outlives the component.
  resetConfirm();
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

  it("stays flat until a group exists, but the row menu still deletes", async () => {
    vi.mocked(api.sessionGroups.list).mockResolvedValue({ groups: [] });
    vi.mocked(api.sessions.list).mockResolvedValue({
      sessions: [session("1")],
    });
    renderSidebar();
    await screen.findByTestId("session-row");
    expect(screen.queryByLabelText("Collapse Ungrouped")).toBeNull();

    // No group means nowhere to move to, so the menu is Delete and nothing
    // else — but it is still there, which it was not before.
    fireEvent.click(screen.getByTestId("session-row-menu-1"));
    expect(screen.queryByTestId("move-to-group-ungrouped")).toBeNull();
    expect(screen.getByTestId("delete-session-1")).toBeTruthy();
  });

  it("deletes a session from the row menu, once confirmed", async () => {
    vi.mocked(api.sessionGroups.list).mockResolvedValue({ groups: [] });
    vi.mocked(api.sessions.list).mockResolvedValue({
      sessions: [session("1")],
    });
    vi.mocked(api.sessions.remove).mockResolvedValue({ ok: true });
    renderSidebar();
    await screen.findByTestId("session-row");

    fireEvent.click(screen.getByTestId("session-row-menu-1"));
    fireEvent.click(screen.getByTestId("delete-session-1"));
    // The confirm is what stands between a stray click and a lost session.
    expect(api.sessions.remove).not.toHaveBeenCalled();
    expect(confirmSnapshot()?.message).toContain("session 1");

    answerConfirm(true);
    await waitFor(() => expect(api.sessions.remove).toHaveBeenCalledWith("1"));
  });

  it("cancelling the confirm leaves the session alone", async () => {
    vi.mocked(api.sessionGroups.list).mockResolvedValue({ groups: [] });
    vi.mocked(api.sessions.list).mockResolvedValue({
      sessions: [session("1")],
    });
    renderSidebar();
    await screen.findByTestId("session-row");

    fireEvent.click(screen.getByTestId("session-row-menu-1"));
    fireEvent.click(screen.getByTestId("delete-session-1"));
    answerConfirm(false);
    await waitFor(() => expect(confirmSnapshot()).toBeNull());
    expect(api.sessions.remove).not.toHaveBeenCalled();
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

  it("creates a group from the tick, and dismisses the box from the cross", async () => {
    vi.mocked(api.sessionGroups.list).mockResolvedValue({ groups: [] });
    vi.mocked(api.sessionGroups.create).mockResolvedValue({
      group: { name: "web" },
    });
    vi.mocked(api.sessions.list).mockResolvedValue({ sessions: [] });
    renderSidebar();

    // Cancel: the box goes away and nothing is created.
    fireEvent.click(await screen.findByTestId("new-group-button"));
    fireEvent.click(screen.getByTestId("create-group-cancel"));
    expect(screen.queryByTestId("group-name-input")).toBeNull();
    expect(api.sessionGroups.create).not.toHaveBeenCalled();

    // The tick is inert until the name is non-empty.
    fireEvent.click(screen.getByTestId("new-group-button"));
    expect(
      screen.getByTestId<HTMLButtonElement>("create-group-confirm").disabled,
    ).toBe(true);
    fireEvent.change(screen.getByTestId("group-name-input"), {
      target: { value: "web" },
    });
    fireEvent.click(screen.getByTestId("create-group-confirm"));
    await waitFor(() =>
      expect(api.sessionGroups.create).toHaveBeenCalledWith("web"),
    );
    expect(screen.queryByTestId("group-name-input")).toBeNull();
  });

  it("collapses and expands a group by clicking its header row", async () => {
    vi.mocked(api.sessionGroups.list).mockResolvedValue({
      groups: [{ name: "web" }],
    });
    vi.mocked(api.sessions.list).mockResolvedValue({
      sessions: [session("1", "web")],
    });
    renderSidebar();
    const toggle = await screen.findByTestId("group-toggle-web");
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    expect(screen.getByTestId("group-section-web").textContent).toContain(
      "session 1",
    );

    fireEvent.click(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(screen.getByTestId("group-section-web").textContent).not.toContain(
      "session 1",
    );

    fireEvent.click(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
  });

  it("opens the menu without collapsing the group", async () => {
    vi.mocked(api.sessionGroups.list).mockResolvedValue({
      groups: [{ name: "web" }],
    });
    vi.mocked(api.sessions.list).mockResolvedValue({
      sessions: [session("1", "web")],
    });
    renderSidebar();
    fireEvent.click(await screen.findByTestId("group-menu-button-web"));
    expect(screen.getByTestId("rename-group-item")).toBeDefined();
    expect(
      screen.getByTestId("group-toggle-web").getAttribute("aria-expanded"),
    ).toBe("true");
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

  it("confirms a delete in the header, not behind the menu", async () => {
    vi.mocked(api.sessionGroups.list).mockResolvedValue({
      groups: [{ name: "web" }],
    });
    vi.mocked(api.sessionGroups.remove).mockResolvedValue({});
    vi.mocked(api.sessions.list).mockResolvedValue({ sessions: [] });
    renderSidebar();
    fireEvent.click(await screen.findByTestId("group-menu-button-web"));
    fireEvent.click(screen.getByTestId("delete-group-item"));

    // The prompt is on the rail itself, so the confirm is one click away —
    // the menu it came from has already closed.
    const confirm = await screen.findByTestId("group-delete-confirm-web");
    expect(confirm.textContent).toContain("Its sessions move to Ungrouped");
    fireEvent.click(screen.getByTestId("confirm-delete-group-item"));
    await waitFor(() =>
      expect(api.sessionGroups.remove).toHaveBeenCalledWith("web"),
    );
  });

  it("backs out of a delete without removing the group", async () => {
    vi.mocked(api.sessionGroups.list).mockResolvedValue({
      groups: [{ name: "web" }],
    });
    vi.mocked(api.sessions.list).mockResolvedValue({ sessions: [] });
    renderSidebar();
    fireEvent.click(await screen.findByTestId("group-menu-button-web"));
    fireEvent.click(screen.getByTestId("delete-group-item"));
    fireEvent.click(await screen.findByTestId("cancel-delete-group-item"));
    expect(screen.queryByTestId("group-delete-confirm-web")).toBeNull();
    expect(screen.getByTestId("group-toggle-web")).toBeDefined();
    expect(api.sessionGroups.remove).not.toHaveBeenCalled();
  });
  // Group *order* already survived a reload and collapse did not, so half of
  // an arrangement came back and half of it did not.
  it("remembers which groups are collapsed across a remount", async () => {
    vi.mocked(api.sessionGroups.list).mockResolvedValue({
      groups: [{ name: "web" }],
    });
    vi.mocked(api.sessions.list).mockResolvedValue({
      sessions: [session("1", "web")],
    });
    renderSidebar();
    expect(
      (await screen.findByTestId("group-section-web")).textContent,
    ).toContain("session 1");
    fireEvent.click(screen.getByTestId("group-toggle-web"));
    expect(screen.getByTestId("group-section-web").textContent).not.toContain(
      "session 1",
    );

    cleanup();
    renderSidebar();
    await waitFor(() =>
      expect(
        screen.getByTestId("group-section-web").textContent,
      ).not.toContain("session 1"),
    );
  });

  it("filters the rail by session title once there is enough to search", async () => {
    vi.mocked(api.sessionGroups.list).mockResolvedValue({ groups: [] });
    vi.mocked(api.sessions.list).mockResolvedValue({
      sessions: Array.from({ length: 9 }, (_, i) => session(String(i))),
    });
    renderSidebar();
    const box = await screen.findByTestId("session-filter");

    fireEvent.change(box, { target: { value: "session 3" } });
    await waitFor(() =>
      expect(screen.getAllByTestId("session-row")).toHaveLength(1),
    );

    // A filter matching nothing says so rather than reading as an empty account.
    fireEvent.change(box, { target: { value: "nothing here" } });
    await waitFor(() =>
      expect(screen.queryAllByTestId("session-row")).toHaveLength(0),
    );
    expect(screen.getByText(/No session matches/)).toBeDefined();
  });
});
