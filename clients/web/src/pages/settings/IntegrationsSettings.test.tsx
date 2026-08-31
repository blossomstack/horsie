import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { setCurrentProject } from "../../api/client";
import type { McpServerDetail, McpServerView } from "../../api/types";
import { IntegrationsSettings } from "./IntegrationsSettings";

// The page builds project-scoped URLs; without one every read throws.
beforeAll(() => setCurrentProject("proj"));
afterEach(cleanup);

const LINEAR: McpServerView = {
  name: "linear",
  url: "https://mcp.linear.app/mcp",
  enabled: true,
  auth: { kind: "None", value: {} },
  description: "Linear",
  userDescription: undefined,
  instructions: "Search before you file.",
  toolCount: 2,
  lastError: undefined,
};

const DETAIL: McpServerDetail = {
  server: LINEAR,
  tools: [
    { name: "search_issues", description: "find issues by text" },
    { name: "create_issue", description: "" },
  ],
};

const servers: { current: McpServerView[] } = { current: [LINEAR] };
/** How many times the per-server detail was actually fetched. */
const detailReads = { count: 0 };

beforeEach(() => {
  servers.current = [LINEAR];
  detailReads.count = 0;
});

vi.mock("../../hooks/useSettings", () => ({
  useSettings: () => ({ data: undefined, isLoading: false, isError: false }),
}));
vi.mock("../../hooks/useGithub", () => ({
  useGithubStatus: () => ({ data: undefined }),
  useGithubDisconnect: () => ({ mutate: vi.fn(), isPending: false }),
}));
vi.mock("../../hooks/useMcp", () => ({
  useMcpServers: () => ({ data: servers.current, isError: false }),
  useMcpServer: () => {
    detailReads.count += 1;
    return { data: DETAIL, isPending: false, isError: false };
  },
  useUpsertMcpServer: () => ({ mutateAsync: vi.fn(), isPending: false }),
  useDeleteMcpServer: () => ({ mutate: vi.fn(), isPending: false }),
  useTestMcpServer: () => ({ mutateAsync: vi.fn(), isPending: false }),
  useConnectMcpServer: () => ({ mutateAsync: vi.fn(), isPending: false }),
}));

const page = () =>
  render(
    <MemoryRouter>
      <IntegrationsSettings />
    </MemoryRouter>,
  );

describe("IntegrationsSettings — MCP", () => {
  // The description box holds only what a person typed. Pre-filling it with
  // the server's own words would turn them into something a person wrote the
  // first time anyone pressed Save.
  it("offers the server's own description as a placeholder, not as an answer", () => {
    page();
    const box = screen.getByLabelText("Description") as HTMLInputElement;
    expect(box.value).toBe("");
    expect(box.placeholder).toBe("Linear");
  });

  it("shows a typed description in the box instead", () => {
    servers.current = [{ ...LINEAR, userDescription: "our tracker" }];
    page();
    expect((screen.getByLabelText("Description") as HTMLInputElement).value).toBe(
      "our tracker",
    );
  });

  it("shows what the server says about itself", () => {
    page();
    expect(screen.getByText("Search before you file.")).toBeTruthy();
  });

  // The list read carries no tools, so opening one server is the only thing
  // that should cost a second request — and nothing should be fetched for a
  // row nobody has opened.
  it("reads a server's tools only once its list is opened", async () => {
    page();
    expect(screen.queryByTestId("mcp-tool-list")).toBeNull();
    expect(detailReads.count).toBe(0);

    fireEvent.click(screen.getByTestId("mcp-tools-toggle"));

    await waitFor(() => expect(screen.getByTestId("mcp-tool-list")).toBeTruthy());
    expect(screen.getByText("search_issues")).toBeTruthy();
    expect(screen.getByText("find issues by text")).toBeTruthy();
    // A tool that published no description says so rather than leaving a gap.
    expect(screen.getByText("no description")).toBeTruthy();
  });

  // A server that is down is still worth reading the tools of: the catalogue
  // is what it last advertised, not a live call.
  it("still offers the tool list for a server whose last connect failed", () => {
    servers.current = [
      { ...LINEAR, enabled: false, lastError: "connection refused" },
    ];
    page();
    expect(screen.getByTestId("mcp-tools-toggle")).toBeTruthy();
    expect(screen.getByText("not tested")).toBeTruthy();
  });

  // `undefined` is the one case with nothing to show: never connected, so
  // horsie has never seen this server's tools.
  it("offers no tool list for a server that has never connected", () => {
    servers.current = [
      { ...LINEAR, enabled: false, toolCount: undefined, description: undefined },
    ];
    page();
    expect(screen.queryByTestId("mcp-tools-toggle")).toBeNull();
  });
});
