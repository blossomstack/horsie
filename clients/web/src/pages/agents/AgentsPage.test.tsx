import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { AgentView } from "../../api/types";
import { AgentsPage } from "./AgentsPage";

// vitest runs without `globals`, so testing-library's auto-cleanup never
// fires; without this the second render's queries see the first test's DOM.
afterEach(cleanup);

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
    vendor: "local",
    model: "sonnet",
    repos: [],
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
  it("renders one row per agent with its model and description", async () => {
    const { findAllByTestId } = renderPage();
    const rows = await findAllByTestId("agent-row");
    expect(rows).toHaveLength(2);
    expect(rows[0].textContent).toContain("reviewer");
    expect(rows[0].textContent).toContain("sonnet");
    expect(rows[0].textContent).toContain("reviewer description");
    expect(rows[0].textContent).toContain("1 skills");
    expect(rows[1].textContent).toContain("haiku");
  });

  it("deletes the named agent once the confirm is accepted", async () => {
    const confirm = vi
      .spyOn(window, "confirm")
      .mockImplementation(() => true);
    const { findByTestId } = renderPage();
    fireEvent.click(await findByTestId("delete-agent-fixer"));
    await waitFor(() => expect(remove).toHaveBeenCalledWith("fixer"));
    confirm.mockRestore();
  });
});
