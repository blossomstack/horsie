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
      deleteAgent: vi.fn(),
    },
    inbox: { list: vi.fn() },
    // The rail's switcher reads both of these. A project is what the rail
    // below belongs to, so a Sidebar rendered without one is not a Sidebar.
    projects: { list: vi.fn().mockResolvedValue([]) },
    globalEventsUrl: () => "/api/p/p1/events",
  },
  getCurrentProject: () => "p1",
}));

function renderSidebar(at = "/", onHide?: () => void) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(
    <QueryClientProvider client={client}>
      <MemoryRouter initialEntries={[at]}>
        <Sidebar onHide={onHide} />
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
    subSessions: [],
  };
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  localStorage.clear();
  // The confirm store is module-level, so it outlives the component.
  resetConfirm();
});

describe("Sidebar", () => {
  it("requests hiding the sidebar", () => {
    const onHide = vi.fn();
    renderSidebar("/", onHide);

    fireEvent.click(screen.getByTestId("hide-sidebar-button"));

    expect(onHide).toHaveBeenCalledOnce();
  });
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
    // The mark is an attribute, not a class: it is the stylesheet that decides
    // what a key holding a value looks like. Asserting on `className` measured
    // the wrong thing and would keep passing if the styling were deleted.
    expect(button.getAttribute("data-marked")).toBeNull();

    fireEvent.click(button);
    fireEvent.click(screen.getByTestId("tag-chip-web"));
    fireEvent.click(button);
    expect(screen.queryByTestId("tag-filter-panel")).toBeNull();
    expect(button.getAttribute("data-marked")).toBe("true");
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

  it("shows no inbox badge when there is nothing in it", async () => {
    vi.mocked(api.sessions.list).mockResolvedValue({ sessions: [] });
    vi.mocked(api.inbox.list).mockResolvedValue({
      messages: [],
      unread: 0,
      openAsks: 0,
    });
    renderSidebar();
    await screen.findByTestId("inbox-link");
    await waitFor(() => expect(api.inbox.list).toHaveBeenCalled());
    expect(screen.queryByTestId("inbox-badge")).toBeNull();
  });

  /* The two numbers do not mean the same thing: an unread notice costs
     nothing, an open ask is an agent that has stopped. The badge has to be
     able to say the second one. */
  it("counts unread quietly, and an agent waiting on an answer loudly", async () => {
    vi.mocked(api.sessions.list).mockResolvedValue({ sessions: [] });
    vi.mocked(api.inbox.list).mockResolvedValue({
      messages: [],
      unread: 5,
      openAsks: 0,
    });
    renderSidebar();
    const quiet = await screen.findByTestId("inbox-badge");
    expect(quiet.textContent).toBe("5");
    const quietClass = quiet.className;

    cleanup();
    vi.mocked(api.inbox.list).mockResolvedValue({
      messages: [],
      unread: 5,
      openAsks: 2,
    });
    renderSidebar();
    const loud = await screen.findByTestId("inbox-badge");
    expect(loud.textContent).toBe("2");
    expect(loud.className).not.toBe(quietClass);
  });

  it("sends the nameplate home", async () => {
    vi.mocked(api.sessions.list).mockResolvedValue({ sessions: [] });
    renderSidebar("/agents");
    expect(
      (await screen.findByTestId("home-link")).getAttribute("href"),
    ).toBe("/");
  });
});

describe("sub sessions in the rail", () => {
  function subSession(id: string, parent?: string, status = "idle", title = "a branch") {
    return { id, parent, title, status, createdAtMs: 1, lastActivityMs: 1 };
  }

  /* The rail lists sessions. A session's sub sessions are its shape, and the
     graph draws that — lineage, status, and what each one spawned — so a row
     per sub session here was a second structural view with less to say. */
  it("lists sessions only, never the sub sessions under them", async () => {
    const s = session("s1");
    s.subSessions = [
      subSession("a", undefined, "idle", "first"),
      subSession("b", "a", "idle", "second"),
    ];
    vi.mocked(api.sessions.list).mockResolvedValue({ sessions: [s] });
    renderSidebar();

    await screen.findByTestId("session-row");
    expect(screen.queryAllByTestId("subSession-row")).toHaveLength(0);
    expect(screen.queryByText("first")).toBeNull();
    expect(screen.queryByText("second")).toBeNull();
  });

  /* With no row of its own, a sub session's session is the only thing that can
     say where the reader is. While each sub session had a row this was the
     opposite: the session row deliberately went dark so two rows could not
     both claim to be on screen. */
  it("marks the session as open while one of its sub sessions is the page", async () => {
    const s = session("s1");
    s.subSessions = [subSession("a", undefined, "idle", "branch")];
    vi.mocked(api.sessions.list).mockResolvedValue({ sessions: [s] });
    renderSidebar("/sessions/s1/agents/a");

    expect(
      (await screen.findByTestId("session-row")).getAttribute("aria-current"),
    ).toBe("page");
  });

  it("marks the session as open when the session itself is the page", async () => {
    const s = session("s1");
    s.subSessions = [subSession("a", undefined, "idle", "branch")];
    vi.mocked(api.sessions.list).mockResolvedValue({ sessions: [s] });
    renderSidebar("/sessions/s1");

    expect(
      (await screen.findByTestId("session-row")).getAttribute("aria-current"),
    ).toBe("page");
  });

  it("leaves another session's row dark", async () => {
    const one = session("s1");
    const two = session("s2");
    two.subSessions = [subSession("a", undefined, "idle", "branch")];
    vi.mocked(api.sessions.list).mockResolvedValue({ sessions: [one, two] });
    renderSidebar("/sessions/s2/agents/a");

    await screen.findAllByTestId("session-row");
    const rows = screen.getAllByTestId("session-row");
    const current = rows.filter((r) => r.getAttribute("aria-current") === "page");
    expect(current.map((r) => r.getAttribute("data-session-id"))).toEqual(["s2"]);
  });
});
