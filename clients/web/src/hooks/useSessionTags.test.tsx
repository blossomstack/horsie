import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderHook } from "@testing-library/react";
import type { ReactNode } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { api } from "../api/client";
import { useSetSessionTag } from "./useSessionTags";

vi.mock("../api/client", () => ({
  api: { sessions: { setAnnotations: vi.fn() } },
}));

function wrapper(client: QueryClient) {
  return ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
}

afterEach(() => vi.clearAllMocks());

describe("useSetSessionTag", () => {
  it("sets an empty-valued tag key when turning a tag on", async () => {
    vi.mocked(api.sessions.setAnnotations).mockResolvedValue({});
    const client = new QueryClient();
    const { result } = renderHook(() => useSetSessionTag(), {
      wrapper: wrapper(client),
    });
    await result.current.mutateAsync({ id: "s1", tag: "web", on: true });
    expect(api.sessions.setAnnotations).toHaveBeenCalledWith("s1", {
      set: [{ key: "tag.web", value: "" }],
      remove: [],
    });
  });

  it("removes the key when turning a tag off, and invalidates both queries", async () => {
    vi.mocked(api.sessions.setAnnotations).mockResolvedValue({});
    const client = new QueryClient();
    const spy = vi.spyOn(client, "invalidateQueries");
    const { result } = renderHook(() => useSetSessionTag(), {
      wrapper: wrapper(client),
    });
    await result.current.mutateAsync({ id: "s1", tag: "web", on: false });
    expect(api.sessions.setAnnotations).toHaveBeenCalledWith("s1", {
      set: [],
      remove: ["tag.web"],
    });
    expect(spy).toHaveBeenCalledWith({ queryKey: ["sessions"] });
    expect(spy).toHaveBeenCalledWith({ queryKey: ["session", "s1"] });
  });
});
