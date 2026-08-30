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
import type { InboxScope } from "../../api/client";
import { InboxState, type InboxMessageView } from "../../api/types";
import {
  answerConfirm,
  confirmSnapshot,
  resetConfirm,
} from "../../lib/confirm";
import { InboxPage } from "./InboxPage";

// vitest runs without `globals`, so testing-library's auto-cleanup never
// fires; without this the second render's queries see the first test's DOM.
afterEach(cleanup);
afterEach(resetConfirm);
afterEach(() => vi.clearAllMocks());

const markRead = vi.fn(async (_ids: string[]) => ({}));
const remove = vi.fn(async (_ids: string[]) => ({}));
const reply = vi.fn(async (_id: string, _text: string) => ({}));
const list = vi.fn(async (scope: InboxScope) => page(scope));

vi.mock("../../api/client", () => ({
  api: {
    inbox: {
      list: (scope: InboxScope) => list(scope),
      markRead: (ids: string[]) => markRead(ids),
      remove: (ids: string[]) => remove(ids),
      reply: (id: string, text: string) => reply(id, text),
    },
    sessions: {
      list: async () => ({
        sessions: [
          {
            id: "s1",
            name: "the deploy",
            status: "idle",
            createdAt: 1,
            annotations: [],
            subSessions: [],
          },
        ],
      }),
    },
  },
  MAIN_AGENT: "main",
  ApiRequestError: class extends Error {},
}));

function notice(id: string, over: Partial<InboxMessageView> = {}): InboxMessageView {
  return {
    id,
    body: { kind: "Notice", value: { body: "the build went green" } },
    state: InboxState.Open,
    sessionId: "s1",
    agentId: "main",
    title: `notice ${id}`,
    createdAt: Date.now(),
    ...over,
  };
}

function ask(id: string, over: Partial<InboxMessageView> = {}): InboxMessageView {
  return {
    id,
    body: {
      kind: "Ask",
      value: {
        question: "Which colour?",
        choices: ["blue", "red"],
        multiple: false,
        toolCallId: `call-${id}`,
      },
    },
    state: InboxState.Open,
    sessionId: "s1",
    agentId: "main",
    title: `ask ${id}`,
    createdAt: Date.now(),
    ...over,
  };
}

/** What the server answers for each slice. `open` carries both kinds — a
 * notice is open until it is replied to — which is the case the "needs
 * answer" view has to narrow. */
function page(scope: InboxScope) {
  const messages =
    scope === "unread"
      ? [ask("a1")]
      : [ask("a1"), notice("n1", { readAt: 2 })];
  return { messages, unread: 1, openAsks: 1 };
}

function renderPage() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(
    <QueryClientProvider client={client}>
      <MemoryRouter>
        <InboxPage />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("InboxPage", () => {
  it("lists both kinds, each with the session it came from", async () => {
    renderPage();
    const rows = await screen.findAllByTestId("inbox-row");
    expect(rows).toHaveLength(2);
    expect(rows[0].textContent).toContain("ask a1");
    expect(rows[1].textContent).toContain("notice n1");
    // Resolved from the session list: the message itself carries only an id.
    expect(rows[0].textContent).toContain("the deploy");
    // Unread is the absence of `readAt`, not a field of its own.
    expect(rows[0].querySelector('[data-testid="inbox-unread-dot"]')).toBeTruthy();
    expect(rows[1].querySelector('[data-testid="inbox-unread-dot"]')).toBeNull();
  });

  it("narrows 'needs answer' to the questions, not everything still open", async () => {
    renderPage();
    await screen.findAllByTestId("inbox-row");
    fireEvent.click(screen.getByTestId("inbox-filter-answer"));
    await waitFor(() =>
      expect(screen.getAllByTestId("inbox-row")).toHaveLength(1),
    );
    expect(list).toHaveBeenCalledWith("open");
    expect(screen.getByTestId("inbox-row").textContent).toContain("ask a1");
  });

  it("marks a message read when it is opened", async () => {
    renderPage();
    const rows = await screen.findAllByTestId("inbox-row");
    fireEvent.click(rows[0].querySelector('[data-testid="inbox-open"]')!);
    await waitFor(() => expect(markRead).toHaveBeenCalledWith(["a1"]));
    expect(await screen.findByTestId("inbox-message")).toBeTruthy();
  });

  it("answers an open ask with the same control the transcript uses", async () => {
    renderPage();
    const rows = await screen.findAllByTestId("inbox-row");
    fireEvent.click(rows[0].querySelector('[data-testid="inbox-open"]')!);
    await screen.findByTestId("inbox-message");

    fireEvent.click(
      screen.getByTestId("inbox-message").querySelector('[data-value="blue"]')!,
    );
    fireEvent.click(screen.getByTestId("ask-user-send"));
    await waitFor(() => expect(reply).toHaveBeenCalledWith("a1", "blue"));
  });

  it("replies to a notice as an ordinary message to its agent", async () => {
    renderPage();
    const rows = await screen.findAllByTestId("inbox-row");
    fireEvent.click(rows[1].querySelector('[data-testid="inbox-open"]')!);
    await screen.findByTestId("inbox-message");

    fireEvent.change(screen.getByTestId("inbox-reply-text"), {
      target: { value: "nice" },
    });
    fireEvent.click(screen.getByTestId("inbox-reply-send"));
    await waitFor(() => expect(reply).toHaveBeenCalledWith("n1", "nice"));
  });

  it("deletes a selection, once confirmed", async () => {
    renderPage();
    await screen.findAllByTestId("inbox-row");
    fireEvent.click(screen.getByTestId("inbox-select-a1"));
    fireEvent.click(screen.getByTestId("inbox-select-n1"));

    fireEvent.click(screen.getByTestId("inbox-delete-selected"));
    // The confirm is what stands between a stray click and a declined
    // question.
    await waitFor(() => expect(confirmSnapshot()).not.toBeNull());
    expect(remove).not.toHaveBeenCalled();

    answerConfirm(true);
    await waitFor(() => expect(remove).toHaveBeenCalledWith(["a1", "n1"]));
  });

  it("says how many open asks a delete would decline, and what that does", async () => {
    renderPage();
    await screen.findAllByTestId("inbox-row");
    fireEvent.click(screen.getByTestId("inbox-select-a1"));
    fireEvent.click(screen.getByTestId("inbox-select-n1"));
    fireEvent.click(screen.getByTestId("inbox-delete-selected"));

    await waitFor(() => expect(confirmSnapshot()).not.toBeNull());
    const message = confirmSnapshot()!.message;
    // Two messages, one of them a question an agent is parked on.
    expect(message).toContain("2");
    expect(message).toContain("1 of them is a question");
    expect(message).toContain("declines the question");
    expect(message).toContain("carries on without one");
  });

  /// When the one thing selected *is* the question, the general wording counts
  /// asks separately from messages and reads as a mismatch — "delete this
  /// message? 1 of them is a question" — so this case gets its own sentence.
  it("says it plainly when the only selected message is the question", async () => {
    renderPage();
    await screen.findAllByTestId("inbox-row");
    fireEvent.click(screen.getByTestId("inbox-select-a1"));
    fireEvent.click(screen.getByTestId("inbox-delete-selected"));

    await waitFor(() => expect(confirmSnapshot()).not.toBeNull());
    const message = confirmSnapshot()!.message;
    expect(message).not.toContain("of them");
    expect(message).toContain("still parked on it");
    expect(message).toContain("declines the question");
  });

  it("leaves the notices alone in the warning when no ask is open", async () => {
    renderPage();
    await screen.findAllByTestId("inbox-row");
    fireEvent.click(screen.getByTestId("inbox-select-n1"));
    fireEvent.click(screen.getByTestId("inbox-delete-selected"));

    await waitFor(() => expect(confirmSnapshot()).not.toBeNull());
    expect(confirmSnapshot()!.message).not.toContain("declines");
  });

  it("cancelling the confirm deletes nothing", async () => {
    renderPage();
    await screen.findAllByTestId("inbox-row");
    fireEvent.click(screen.getByTestId("inbox-select-a1"));
    fireEvent.click(screen.getByTestId("inbox-delete-selected"));

    await waitFor(() => expect(confirmSnapshot()).not.toBeNull());
    answerConfirm(false);
    await waitFor(() => expect(confirmSnapshot()).toBeNull());
    expect(remove).not.toHaveBeenCalled();
  });
});
