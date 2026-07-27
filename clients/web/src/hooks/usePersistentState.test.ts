import { act, renderHook } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import { usePersistentState } from "./usePersistentState";

const KEY = "test-persistent-state";

beforeEach(() => localStorage.clear());

describe("usePersistentState", () => {
  it("returns the initial value when the key is absent", () => {
    const { result } = renderHook(() => usePersistentState(KEY, "hello"));
    expect(result.current[0]).toBe("hello");
  });

  it("hydrates from an existing stored JSON value", () => {
    localStorage.setItem(KEY, JSON.stringify("stored"));
    const { result } = renderHook(() => usePersistentState(KEY, "hello"));
    expect(result.current[0]).toBe("stored");
  });

  it("writes through to localStorage on set", () => {
    const { result } = renderHook(() => usePersistentState(KEY, 0));
    act(() => result.current[1](42));
    expect(result.current[0]).toBe(42);
    expect(JSON.parse(localStorage.getItem(KEY)!)).toBe(42);
  });

  it("falls back to the initial value on corrupt JSON", () => {
    localStorage.setItem(KEY, "{not json");
    const { result } = renderHook(() => usePersistentState(KEY, "hello"));
    expect(result.current[0]).toBe("hello");
  });

  it("falls back to the initial value when deserialize returns undefined", () => {
    localStorage.setItem(KEY, JSON.stringify({ v: 99 }));
    const { result } = renderHook(() =>
      usePersistentState(KEY, { v: 1 }, {
        deserialize: (raw) => {
          const p = raw as { v?: number };
          return p.v === 1 ? { v: 1 } : undefined;
        },
      }),
    );
    expect(result.current[0]).toEqual({ v: 1 });
  });

  it("round-trips a Set through custom serializers", () => {
    const { result } = renderHook(() =>
      usePersistentState<Set<string>>(KEY, new Set(), {
        serialize: (s) => [...s],
        deserialize: (raw) => (Array.isArray(raw) ? new Set(raw as string[]) : undefined),
      }),
    );
    act(() => result.current[1](new Set(["a", "b"])));
    expect(JSON.parse(localStorage.getItem(KEY)!)).toEqual(["a", "b"]);

    const again = renderHook(() =>
      usePersistentState<Set<string>>(KEY, new Set(), {
        serialize: (s) => [...s],
        deserialize: (raw) => (Array.isArray(raw) ? new Set(raw as string[]) : undefined),
      }),
    );
    expect([...again.result.current[0]].sort()).toEqual(["a", "b"]);
  });
});
