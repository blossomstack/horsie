import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { AgentView } from "../../api/types";
import { answerConfirm, confirmSnapshot, resetConfirm } from "../../lib/confirm";
import { AgentsPage } from "./AgentsPage";

// vitest runs without `globals`, so testing-library's auto-cleanup never
// fires; without this the second render's queries see the first test's DOM.
afterEach(cleanup);
afterEach(resetConfirm);

const remove = vi.fn(async (_name: string) => {});
const list = vi.fn(async (): Promise<AgentView[]> => agents);

vi.mock("../../api/client", () => ({
  api: {
    agents: {
      list: () => list(),
      remove: (name: string) => remove(name),
    },
  },
  ApiRequestError: class extends Error {},
}));

function agent(name: string, over: Partial<AgentView> = {}): AgentView {
  return {
    name,
    description: `${name} description`,
    model: "sonnet",
    plugins: ["superpowers"],
    mcpServers: [],
    memorySpaces: ["horsie"],
    createdAt: "1",
    updatedAt: "1",
    ...over,
  };
}

const agents = [agent("reviewer"), agent("fixer", { model: "haiku" })];

function renderPage() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(
    <QueryClientProvider client={client}>
      <MemoryRouter>
        <AgentsPage />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("AgentsPage", () => {
  // A row says what this is and nothing else. The model and the channel counts
  // used to hang off every one of them, which in a 20rem column made a roster
  // you read rather than scanned — and all of it is in the panel beside it.
  it("renders one row per agent: the name and the description, and no more", async () => {
    const { findAllByTestId } = renderPage();
    const rows = await findAllByTestId("agent-row");
    expect(rows).toHaveLength(2);
    expect(rows[0].textContent).toContain("reviewer");
    expect(rows[0].textContent).toContain("reviewer description");
    expect(rows[0].textContent).not.toContain("sonnet");
    expect(rows[0].textContent).not.toContain("skill");
    expect(rows[1].textContent).toContain("fixer");
  });

  it("deletes the named agent once the confirm is accepted", async () => {
    const { findByTestId } = renderPage();
    fireEvent.click(await findByTestId("delete-agent-fixer"));
    expect(confirmSnapshot()?.message).toContain("fixer");
    answerConfirm(true);
    await waitFor(() => expect(remove).toHaveBeenCalledWith("fixer"));
  });
});
