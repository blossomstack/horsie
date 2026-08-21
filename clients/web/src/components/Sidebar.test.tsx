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
// ThemeToggle; the theme toggle is irrelevant to the rail's list behaviour.
vi.mock("./ThemeToggle", () => ({ ThemeToggle: () => null }));

vi.mock("../api/client", () => ({
  api: {
    sessions: {
      list: vi.fn(),
      setAnnotations: vi.fn(),
      remove: vi.fn(),
      deleteFork: vi.fn(),
    },
    // The rail's switcher reads both of these. A project is what the rail
    // below belongs to, so a Sidebar rendered without one is not a Sidebar.
    projects: { list: vi.fn().mockResolvedValue([]) },
    globalEventsUrl: () => "/api/p/p1/events",
  },
  getCurrentProject: () => "p1",
}));

function renderSidebar(at = "/") {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(
    <QueryClientProvider client={client}>
      <MemoryRouter initialEntries={[at]}>
        <Sidebar />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

function session(id: string, tags: string[] = []): SessionSummary {
  return {
    id,
    name: `session ${id}`,
    status: SessionStatusKind.Idle,
    createdAt: 1,
    annotations: tags.map((t) => ({ key: `tag.${t}`, value: "" })),
    forks: [],
  };
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  localStorage.clear();
  // The confirm store is module-level, so it outlives the component.
  resetConfirm();
});

describe("Sidebar tag filter", () => {
  it("hides the filter button until a tag exists", async () => {
    vi.mocked(api.sessions.list).mockResolvedValue({
      sessions: [session("1")],
    });
    renderSidebar();
    await screen.findByTestId("session-row");
    expect(screen.queryByTestId("tag-filter-button")).toBeNull();
  });

  it("cycles a chip through require and exclude, narrowing the list each way", async () => {
    vi.mocked(api.sessions.list).mockResolvedValue({
      sessions: [session("1", ["web"]), session("2")],
    });
    renderSidebar();
    await waitFor(() =>
      expect(screen.getAllByTestId("session-row")).toHaveLength(2),
    );

    fireEvent.click(screen.getByTestId("tag-filter-button"));
    const chip = screen.getByTestId("tag-chip-web");

    fireEvent.click(chip);
    expect(chip.getAttribute("data-state")).toBe("require");
    await waitFor(() =>
      expect(screen.getAllByTestId("session-row")).toHaveLength(1),
    );
    expect(
      screen.getByTestId("session-row").getAttribute("data-session-id"),
    ).toBe("1");

    fireEvent.click(chip);
    expect(chip.getAttribute("data-state")).toBe("exclude");
    await waitFor(() =>
      expect(screen.getAllByTestId("session-row")).toHaveLength(1),
    );
    expect(
      screen.getByTestId("session-row").getAttribute("data-session-id"),
    ).toBe("2");

    fireEvent.click(screen.getByTestId("clear-tag-filter"));
    await waitFor(() =>
      expect(screen.getAllByTestId("session-row")).toHaveLength(2),
    );
  });

  it("says so when the tag filter empties the list", async () => {
    vi.mocked(api.sessions.list).mockResolvedValue({
      sessions: [session("1", ["web"])],
    });
    renderSidebar();
    await screen.findByTestId("session-row");
    fireEvent.click(screen.getByTestId("tag-filter-button"));
    fireEvent.click(screen.getByTestId("tag-chip-web"));
    fireEvent.click(screen.getByTestId("tag-chip-web"));
    expect(await screen.findByTestId("no-tag-matches")).toBeTruthy();
  });

  // A filtered list that looks like the whole list is the one failure mode of
  // putting the filter behind a button.
  it("marks the button while a constraint is active, even with the panel shut", async () => {
    vi.mocked(api.sessions.list).mockResolvedValue({
      sessions: [session("1", ["web"])],
    });
    renderSidebar();
    await screen.findByTestId("session-row");
    const button = screen.getByTestId("tag-filter-button");
    const quiet = button.className;

    fireEvent.click(button);
    fireEvent.click(screen.getByTestId("tag-chip-web"));
    fireEvent.click(button);
    expect(screen.queryByTestId("tag-filter-panel")).toBeNull();
    expect(button.className).not.toBe(quiet);
  });

  it("remembers the filter across a remount", async () => {
    vi.mocked(api.sessions.list).mockResolvedValue({
      sessions: [session("1", ["web"]), session("2")],
    });
    renderSidebar();
    fireEvent.click(await screen.findByTestId("tag-filter-button"));
    fireEvent.click(screen.getByTestId("tag-chip-web"));
    await waitFor(() =>
      expect(screen.getAllByTestId("session-row")).toHaveLength(1),
    );

    cleanup();
    renderSidebar();
    await waitFor(() =>
      expect(screen.getAllByTestId("session-row")).toHaveLength(1),
    );
  });

  // Otherwise a tag whose last session was deleted hides the rail with no
  // visible cause: the chip that would explain it is not rendered either.
  it("drops a persisted constraint naming a tag nobody carries", async () => {
    localStorage.setItem(
      "horsie.session-tag-filter",
      JSON.stringify({ require: ["gone"], exclude: [] }),
    );
    vi.mocked(api.sessions.list).mockResolvedValue({
      sessions: [session("1"), session("2")],
    });
    renderSidebar();
    await waitFor(() =>
      expect(screen.getAllByTestId("session-row")).toHaveLength(2),
    );
  });

  it("ANDs the tag filter with the text filter", async () => {
    vi.mocked(api.sessions.list).mockResolvedValue({
      sessions: [
        ...Array.from({ length: 8 }, (_, i) => session(String(i), ["web"])),
        session("8"),
      ],
    });
    renderSidebar();
    fireEvent.click(await screen.findByTestId("tag-filter-button"));
    fireEvent.click(screen.getByTestId("tag-chip-web"));
    fireEvent.change(screen.getByTestId("session-filter"), {
      target: { value: "session 3" },
    });
    await waitFor(() =>
      expect(screen.getAllByTestId("session-row")).toHaveLength(1),
    );
    expect(
      screen.getByTestId("session-row").getAttribute("data-session-id"),
    ).toBe("3");
  });
});

describe("Sidebar sessions", () => {
  it("renders one flat list, with no group chrome", async () => {
    vi.mocked(api.sessions.list).mockResolvedValue({
      sessions: [session("1", ["web"]), session("2")],
    });
    renderSidebar();
    await waitFor(() =>
      expect(screen.getAllByTestId("session-row")).toHaveLength(2),
    );
    expect(screen.queryByTestId("new-group-button")).toBeNull();
  });

  it("deletes a session from the row menu, once confirmed", async () => {
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

  it("filters the rail by session title once there is enough to search", async () => {
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
    expect(screen.getByTestId("no-text-matches")).toBeTruthy();
  });

  it("sends the nameplate home", async () => {
    vi.mocked(api.sessions.list).mockResolvedValue({ sessions: [] });
    renderSidebar("/agents");
    expect(
      (await screen.findByTestId("home-link")).getAttribute("href"),
    ).toBe("/");
  });
});

describe("forks in the rail", () => {
  function fork(id: string, parent?: string, status = "idle", title?: string) {
    return { id, parent, title, status, createdAtMs: 1, lastActivityMs: 1 };
  }

  it("nests a fork of a fork under the fork it came from", async () => {
    const s = session("s1");
    s.forks = [
      fork("a", undefined, "idle", "first"),
      fork("b", "a", "idle", "second"),
    ];
    vi.mocked(api.sessions.list).mockResolvedValue({ sessions: [s] });
    renderSidebar();

    const rows = await screen.findAllByTestId("fork-row");
    expect(rows.map((r) => r.getAttribute("data-fork-id"))).toEqual(["a", "b"]);
    expect(rows.map((r) => r.getAttribute("data-depth"))).toEqual(["0", "1"]);
  });

  /* The session row is the main agent, each fork row is itself. A rollup would
     be a derived status that can disagree with the durable one. */
  it("badges each row with its own status, never a rollup", async () => {
    const s = session("s1");
    s.status = SessionStatusKind.Idle;
    s.forks = [fork("a", undefined, "running", "busy one")];
    vi.mocked(api.sessions.list).mockResolvedValue({ sessions: [s] });
    renderSidebar();

    const forkRow = await screen.findByTestId("fork-row");
    expect(forkRow.getAttribute("title")).toMatch(/running|working/i);
    const sessionRow = screen.getByTestId("session-row");
    expect(sessionRow.getAttribute("title")).not.toMatch(/running|working/i);
  });

  it("names an unnamed fork rather than showing its id", async () => {
    const s = session("s1");
    s.forks = [fork("a")];
    vi.mocked(api.sessions.list).mockResolvedValue({ sessions: [s] });
    renderSidebar();

    expect(await screen.findByText("Untitled fork")).toBeTruthy();
  });

  /* A fork's route is a descendant of its session's, and a `NavLink` counts a
     descendant as active. So opening a fork lit both rows and the rail claimed
     two conversations were on screen at once. */
  it("marks only the fork as open when a fork is the page", async () => {
    const s = session("s1");
    s.forks = [fork("a", undefined, "idle", "branch")];
    vi.mocked(api.sessions.list).mockResolvedValue({ sessions: [s] });
    renderSidebar("/sessions/s1/agents/a");

    const forkRow = await screen.findByTestId("fork-row");
    expect(forkRow.getAttribute("aria-current")).toBe("page");
    expect(
      screen.getByTestId("session-row").getAttribute("aria-current"),
    ).toBeNull();
  });

  it("marks the session as open when the session itself is the page", async () => {
    const s = session("s1");
    s.forks = [fork("a", undefined, "idle", "branch")];
    vi.mocked(api.sessions.list).mockResolvedValue({ sessions: [s] });
    renderSidebar("/sessions/s1");

    const sessionRow = await screen.findByTestId("session-row");
    expect(sessionRow.getAttribute("aria-current")).toBe("page");
    expect(
      screen.getByTestId("fork-row").getAttribute("aria-current"),
    ).toBeNull();
  });

  it("links a fork to its own agent page", async () => {
    const s = session("s1");
    s.forks = [fork("a", undefined, "idle", "branch")];
    vi.mocked(api.sessions.list).mockResolvedValue({ sessions: [s] });
    renderSidebar();

    const row = await screen.findByTestId("fork-row");
    expect(row.getAttribute("href")).toBe("/sessions/s1/agents/a");
  });
});
