import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { EnvironmentView } from "../../api/types";
import { EnvironmentsPage } from "./EnvironmentsPage";

// vitest runs without `globals`, so testing-library's auto-cleanup never
// fires; without this the second render's queries see the first test's DOM.
afterEach(cleanup);

const remove = vi.fn(async (_name: string) => {});
const list = vi.fn(async (): Promise<EnvironmentView[]> => environments);

vi.mock("../../api/client", () => ({
  api: {
    environments: {
      list: () => list(),
      remove: (name: string) => remove(name),
    },
  },
  ApiRequestError: class extends Error {},
}));

function env(
  name: string,
  over: Partial<EnvironmentView> = {},
): EnvironmentView {
  return {
    name,
    description: `${name} description`,
    vendor: "fly",
    repos: [{ url: "https://github.com/o/api" }],
    envVars: [],
    provision: [],
    createdAt: "1",
    updatedAt: "1",
    ...over,
  };
}

const environments = [env("staging"), env("prod", { vendor: "docker" })];

function renderPage() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(
    <QueryClientProvider client={client}>
      <MemoryRouter>
        <EnvironmentsPage />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("EnvironmentsPage", () => {
  it("renders one row per environment with its vendor and description", async () => {
    const { findAllByTestId } = renderPage();
    const rows = await findAllByTestId("environment-row");
    expect(rows).toHaveLength(2);
    expect(rows[0].textContent).toContain("staging");
    expect(rows[0].textContent).toContain("fly");
    expect(rows[0].textContent).toContain("staging description");
    expect(rows[0].textContent).toContain("1 repos");
    expect(rows[1].textContent).toContain("docker");
  });

  it("deletes the named environment once the confirm is accepted", async () => {
    const confirm = vi
      .spyOn(window, "confirm")
      .mockImplementation(() => true);
    const { findByTestId } = renderPage();
    fireEvent.click(await findByTestId("delete-environment-prod"));
    await waitFor(() => expect(remove).toHaveBeenCalledWith("prod"));
    confirm.mockRestore();
  });
});
