import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { RoutineView } from "../../api/types";
import { answerConfirm, confirmSnapshot, resetConfirm } from "../../lib/confirm";
import { RoutinesPage } from "./RoutinesPage";

// vitest runs without `globals`, so testing-library's auto-cleanup never
// fires; without this the second render's queries see the first test's DOM.
afterEach(cleanup);
afterEach(resetConfirm);

const remove = vi.fn(async (_name: string) => {});
const list = vi.fn(async (): Promise<RoutineView[]> => routines);

vi.mock("../../api/client", () => ({
  api: {
    routines: {
      list: () => list(),
      remove: (name: string) => remove(name),
    },
  },
  ApiRequestError: class extends Error {},
}));

function routine(name: string, over: Partial<RoutineView> = {}): RoutineView {
  return {
    name,
    description: `${name} description`,
    target: { type: "Agent", value: { agent: "reviewer" } },
    environment: { type: "Runtime", value: { vendor: "local" } },
    prompt: "triage the inbox",
    schedule: { type: "Manual", value: {} },
    enabled: true,
    createdAt: "1",
    updatedAt: "1",
    ...over,
  };
}

const routines = [
  routine("nightly", {
    schedule: { type: "Every", value: { intervalSecs: 3600 } },
    nextRunAtMs: Date.now() + 60_000,
  }),
  routine("paused", { enabled: false }),
];

function renderPage() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(
    <QueryClientProvider client={client}>
      <MemoryRouter>
        <RoutinesPage />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("RoutinesPage", () => {
  // The schedule is the one fact a routine's row keeps: what it runs is in the
  // panel, but *when it next runs* is the thing you scan a roster of timers for.
  it("renders one row per routine with its schedule", async () => {
    const { findAllByTestId } = renderPage();
    const rows = await findAllByTestId("routine-row");
    expect(rows).toHaveLength(2);
    expect(rows[0].textContent).toContain("nightly");
    expect(rows[0].textContent).not.toContain("reviewer");
    expect(rows[0].textContent).toContain("every 1h");
    // Not "next just now": a future timestamp used to fall into the
    // "less than 45 seconds ago" branch of a past-only formatter.
    expect(rows[0].textContent).toContain("next in 1m");
    expect(rows[0].textContent).toContain("nightly description");
  });

  it("says a routine is paused rather than showing a next run", async () => {
    const { findAllByTestId } = renderPage();
    const rows = await findAllByTestId("routine-row");
    expect(rows[1].textContent).toContain("paused");
    expect(rows[1].textContent).not.toContain("next");
  });

  it("warns that the runs go too before deleting", async () => {
    const { findByTestId } = renderPage();
    fireEvent.click(await findByTestId("delete-routine-paused"));
    expect(confirmSnapshot()?.message).toContain("every session it created");
    answerConfirm(true);
    await waitFor(() => expect(remove).toHaveBeenCalledWith("paused"));
  });
});
