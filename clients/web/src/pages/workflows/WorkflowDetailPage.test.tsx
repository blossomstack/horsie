import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import type {
  WorkflowRunsResponse,
  WorkflowRunSummary,
  WorkflowStatus,
  WorkflowView,
} from "../../api/types";
import { WorkflowDetailPage } from "./WorkflowDetailPage";

// vitest runs without `globals`, so testing-library's auto-cleanup never
// fires; without this the second render's queries see the first test's DOM.
afterEach(cleanup);

const workflow: WorkflowView = {
  name: "triage",
  description: "",
  start: "look",
  steps: [{ name: "look", agent: "wf-step", prompt: "look at it" }],
  createdAt: "1",
  updatedAt: "1",
};

function run(id: string, status: WorkflowStatus["type"]): WorkflowRunSummary {
  return {
    session: {
      id,
      name: `run ${id}`,
      // What every past run reports: a session says nothing about itself until
      // something loads it, and a finished run is precisely one that is not
      // loaded. The row must not be reading this.
      status: undefined,
      createdAt: 1,
      workflow: "triage",
      annotations: [],
    },
    status: { type: status, value: {} } as WorkflowStatus,
  };
}

const runs = vi.fn(
  async (): Promise<WorkflowRunsResponse> => ({
    runs: [run("a", "Finished"), run("b", "Failed"), run("c", "AwaitingInput")],
  }),
);

vi.mock("../../api/client", () => ({
  api: {
    workflows: {
      get: async () => workflow,
      runs: () => runs(),
    },
  },
  ApiRequestError: class extends Error {},
}));

function renderPage() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(
    <QueryClientProvider client={client}>
      <MemoryRouter initialEntries={["/workflows/triage"]}>
        <Routes>
          <Route path="/workflows/:name" element={<WorkflowDetailPage />} />
        </Routes>
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("WorkflowDetailPage runs", () => {
  /** Every row rendered `—` because it showed the *session's* status, which is
   * null for anything not currently loaded — so success, failure and a run
   * parked on a question were indistinguishable. */
  it("shows each run's own outcome, not the session's", async () => {
    const { findAllByTestId } = renderPage();
    const rows = await findAllByTestId("workflow-run-row");
    expect(rows).toHaveLength(3);

    const badges = rows.map((r) => r.querySelector("[data-testid=run-status]"));
    expect(badges.map((b) => b?.getAttribute("data-status"))).toEqual([
      "Finished",
      "Failed",
      "AwaitingInput",
    ]);
    expect(badges.map((b) => b?.textContent)).toEqual([
      "Finished",
      "Failed",
      "Awaiting input",
    ]);
    for (const row of rows) expect(row.textContent).not.toContain("—");
  });
});
