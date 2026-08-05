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
import type {
  EnvironmentInput,
  GitHubRepoList,
  GitHubStatus,
  SettingsView,
  VendorView,
} from "../../api/types";
import { EnvironmentEditPage } from "./EnvironmentEditPage";

const create = vi.fn(async (_body: EnvironmentInput) => ({}));
const config = vi.fn();
const ghStatus = vi.fn();
const ghRepos = vi.fn();

vi.mock("../../api/client", () => ({
  api: {
    environments: {
      get: vi.fn(),
      create: (body: EnvironmentInput) => create(body),
      update: vi.fn(),
    },
    config: { get: () => config() },
    github: { status: () => ghStatus(), repos: () => ghRepos() },
  },
  ApiRequestError: class extends Error {},
}));

function vendor(name: string, supportsProvisioning: boolean): VendorView {
  return { name, isDefault: false, capabilities: { supportsProvisioning } };
}

function setup({
  vendors = [vendor("fly", true), vendor("laptop", false)],
  connected = true,
  repos = ["o/api", "o/web"],
}: {
  vendors?: VendorView[];
  connected?: boolean;
  repos?: string[];
} = {}) {
  config.mockResolvedValue({ vendors } as unknown as SettingsView);
  ghStatus.mockResolvedValue({
    connected,
    appConfigured: true,
    repoCount: repos.length,
  } satisfies GitHubStatus);
  ghRepos.mockResolvedValue({
    repos: repos.map((fullName) => ({
      fullName,
      private: false,
      defaultBranch: "main",
    })),
  } satisfies GitHubRepoList);
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(
    <QueryClientProvider client={client}>
      {/* No :name param — the create form, which is the one that mounts
          without waiting on an environment fetch. */}
      <MemoryRouter initialEntries={["/environments/new"]}>
        <EnvironmentEditPage />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("EnvironmentEditPage", () => {
  it("offers only provisioning vendors in the dropdown", async () => {
    setup();
    const select = await screen.findByTestId<HTMLSelectElement>(
      "environment-vendor-input",
    );
    await waitFor(() => expect(select.options.length).toBe(2));
    // The placeholder plus `fly`; `laptop` cannot provision a workspace.
    expect([...select.options].map((o) => o.value)).toEqual(["", "fly"]);
  });

  it("says where to go when no vendor can provision a workspace", async () => {
    setup({ vendors: [vendor("laptop", false)] });
    const select = await screen.findByTestId<HTMLSelectElement>(
      "environment-vendor-input",
    );
    await waitFor(() => expect(select.disabled).toBe(true));
    expect(screen.getByText(/Settings › Runtimes/)).toBeDefined();
  });

  it("saves repos picked from GitHub as clone URLs with their refs", async () => {
    setup();
    fireEvent.change(await screen.findByTestId("environment-name-input"), {
      target: { value: "staging" },
    });
    fireEvent.change(screen.getByTestId("environment-vendor-input"), {
      target: { value: "fly" },
    });
    fireEvent.click(await screen.findByTestId("repo-toggle-o/web"));
    fireEvent.change(screen.getByTestId("repo-ref-o/web"), {
      target: { value: "dev" },
    });
    fireEvent.click(screen.getByTestId("save-environment-button"));

    await waitFor(() =>
      expect(create).toHaveBeenCalledWith(
        expect.objectContaining({
          name: "staging",
          vendor: "fly",
          repos: [{ url: "https://github.com/o/web", gitRef: "dev" }],
        }),
      ),
    );
  });

  it("filters the repo list", async () => {
    setup();
    fireEvent.change(await screen.findByTestId("repo-filter"), {
      target: { value: "web" },
    });
    expect(screen.queryByTestId("repo-toggle-o/api")).toBeNull();
    expect(screen.getByTestId("repo-toggle-o/web")).toBeDefined();
  });

  it("points at the integration instead of a URL box when GitHub is off", async () => {
    setup({ connected: false });
    expect(await screen.findByTestId("repo-github-prompt")).toBeDefined();
    expect(screen.queryByTestId("repo-filter")).toBeNull();
  });
});
