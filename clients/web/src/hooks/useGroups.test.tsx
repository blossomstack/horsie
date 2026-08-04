import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderHook, waitFor } from "@testing-library/react";
import type { ReactNode } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { api } from "../api/client";
import { useDeleteGroup, useGroupList, useRenameGroup } from "./useGroups";

vi.mock("../api/client", () => ({
  api: {
    sessionGroups: {
      list: vi.fn(),
      create: vi.fn(),
      rename: vi.fn(),
      remove: vi.fn(),
    },
  },
}));

function wrapper(client: QueryClient) {
  return ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
}

afterEach(() => vi.clearAllMocks());

describe("useGroupList", () => {
  it("returns group names", async () => {
    vi.mocked(api.sessionGroups.list).mockResolvedValue({
      groups: [{ name: "web" }, { name: "api" }],
    });
    const client = new QueryClient();
    const { result } = renderHook(() => useGroupList(), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(result.current.data).toEqual(["web", "api"]));
  });
});

describe("group mutations", () => {
  it("rename invalidates the groups and sessions queries", async () => {
    vi.mocked(api.sessionGroups.rename).mockResolvedValue({});
    const client = new QueryClient();
    const groupsSpy = vi.spyOn(client, "invalidateQueries");
    const { result } = renderHook(() => useRenameGroup(), {
      wrapper: wrapper(client),
    });
    await result.current.mutateAsync({ oldName: "web", name: "frontend" });
    expect(api.sessionGroups.rename).toHaveBeenCalledWith("web", "frontend");
    expect(groupsSpy).toHaveBeenCalledWith({ queryKey: ["groups"] });
    expect(groupsSpy).toHaveBeenCalledWith({ queryKey: ["sessions"] });
  });

  it("delete invalidates the groups and sessions queries", async () => {
    vi.mocked(api.sessionGroups.remove).mockResolvedValue({});
    const client = new QueryClient();
    const spy = vi.spyOn(client, "invalidateQueries");
    const { result } = renderHook(() => useDeleteGroup(), {
      wrapper: wrapper(client),
    });
    await result.current.mutateAsync("web");
    expect(spy).toHaveBeenCalledWith({ queryKey: ["groups"] });
    expect(spy).toHaveBeenCalledWith({ queryKey: ["sessions"] });
  });
});
