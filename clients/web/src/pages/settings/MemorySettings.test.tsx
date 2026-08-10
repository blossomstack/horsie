import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ApiRequestError } from "../../api/client";
import type { MemorySpaceView, MemoryView } from "../../api/types";
import { MemorySettings } from "./MemorySettings";

afterEach(cleanup);

const SPACE: MemorySpaceView = { name: "ops", description: "", memoryCount: 1 };

const MEMORY: MemoryView = {
  id: 1,
  space: "ops",
  name: "deploys",
  description: "how we ship",
  content: "ship on green",
  createdAt: "1",
  updatedAt: "1",
};

/** What the two reads this page makes answered: a list, or a failure. */
const memories: { current: MemoryView[] } = { current: [MEMORY] };
const memoriesFailed: { current: unknown } = { current: null };

beforeEach(() => {
  memories.current = [MEMORY];
  memoriesFailed.current = null;
});

vi.mock("../../hooks/useMemory", () => ({
  useMemorySpaces: () => ({ data: [SPACE], isLoading: false, isError: false }),
  useMemories: () => ({
    data: memoriesFailed.current ? undefined : memories.current,
    isLoading: false,
    isError: !!memoriesFailed.current,
    error: memoriesFailed.current,
  }),
  useCreateSpace: () => ({ mutateAsync: vi.fn(), isPending: false, error: null }),
  useDeleteSpace: () => ({ mutate: vi.fn(), isPending: false, error: null }),
  useCreateMemory: () => ({ mutateAsync: vi.fn(), isPending: false, error: null }),
  useDeleteMemory: () => ({ mutate: vi.fn(), isPending: false, error: null }),
  useUpdateMemory: () => ({ mutateAsync: vi.fn(), isPending: false, error: null }),
}));

describe("MemorySettings", () => {
  // `data?.length === 0` avoids the false "no memories in this space yet", but
  // it does so by rendering nothing at all — a space whose memories could not
  // be read looked exactly like one still loading, forever.
  it("says the memory read failed rather than showing an empty panel", () => {
    memoriesFailed.current = new ApiRequestError(
      0,
      "network",
      "Could not reach the horsie server. Is `horsie serve` running?",
    );
    render(<MemorySettings />);
    expect(screen.getByTestId("memories-error").textContent).toContain(
      "Couldn’t load memories",
    );
    expect(screen.queryByText(/No memories in this space yet/)).toBeNull();
  });

  it("still says so when the space is genuinely empty", () => {
    memories.current = [];
    render(<MemorySettings />);
    expect(screen.queryByTestId("memories-error")).toBeNull();
    expect(screen.queryByText(/No memories in this space yet/)).not.toBeNull();
  });
});
