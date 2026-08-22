import { renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { CatalogEntryView, PluginView } from "../api/types";
import { useEntryCatalog } from "./useEntryCatalog";

const bundles = vi.fn<() => PluginView[] | undefined>();
const builtins = vi.fn<() => CatalogEntryView[] | undefined>(() => []);

vi.mock("./usePlugins", () => ({
  usePlugins: () => ({ data: bundles() }),
  useBuiltins: () => ({ data: builtins() }),
}));

function entry(kind: string, name: string): CatalogEntryView {
  return { kind, name, description: `${name} does a thing` };
}

function bundle(
  name: string,
  catalog: CatalogEntryView[],
  enabledDefault = false,
): PluginView {
  return {
    name,
    description: undefined,
    version: undefined,
    kind: {
      kind: "Claude",
      value: {
        url: "u",
        gitRef: undefined,
        subpath: undefined,
        marketplace: undefined,
        marketplaceEntry: undefined,
      },
    },
    catalog,
    hasHooks: false,
    enabledDefault,
    artifactSize: 1,
  };
}

describe("useEntryCatalog", () => {
  it("offers only what the selected bundles declare", () => {
    bundles.mockReturnValue([
      bundle("chosen", [entry("command", "commit")]),
      bundle("ignored", [entry("command", "deploy")]),
    ]);
    const { result } = renderHook(() => useEntryCatalog(["chosen"]));
    expect(result.current.map((e) => e.name)).toEqual(["commit"]);
  });

  it("falls back to the default-enabled bundles when nothing is selected", () => {
    bundles.mockReturnValue([
      bundle("on", [entry("skill", "tdd")], true),
      bundle("off", [entry("skill", "other")]),
    ]);
    // A session that chose nothing still gets the defaults at provisioning,
    // so the menu has to show them or it would lie about what `/` does.
    expect(renderHook(() => useEntryCatalog([])).result.current).toHaveLength(1);
    expect(
      renderHook(() => useEntryCatalog(undefined)).result.current[0].name,
    ).toBe("tdd");
  });

  it("accepts a Set, which is what the new-session draft hands out", () => {
    bundles.mockReturnValue([bundle("a", [entry("agent", "reviewer")])]);
    const { result } = renderHook(() => useEntryCatalog(new Set(["a"])));
    expect(result.current.map((e) => e.name)).toEqual(["reviewer"]);
  });

  it("keeps the first of a duplicated name, but not across sigils", () => {
    bundles.mockReturnValue([
      bundle("a", [entry("command", "review")]),
      bundle("b", [entry("command", "review"), entry("agent", "review")]),
    ]);
    const { result } = renderHook(() => useEntryCatalog(["a", "b"]));
    // `/review` collides and the first wins; `@review` is a different handle.
    expect(result.current.map((e) => `${e.kind}:${e.name}`)).toEqual([
      "command:review",
      "agent:review",
    ]);
  });

  /** A built-in is a control the product owns. Offering it only when a bundle
   * happens to be installed would hide it in the plainest session there is. */
  it("offers builtins even with no bundles at all", () => {
    bundles.mockReturnValue(undefined);
    builtins.mockReturnValue([entry("command", "compact")]);
    const { result } = renderHook(() => useEntryCatalog([]));
    expect(result.current.map((e) => e.name)).toEqual(["compact"]);
  });

  /** The menu has to agree with what actually runs, and the server consults
   * its builtin table before the plugin catalogue. */
  it("a bundle cannot shadow a builtin", () => {
    builtins.mockReturnValue([entry("command", "compact")]);
    bundles.mockReturnValue([
      bundle("impostor", [entry("command", "compact")], true),
    ]);
    const { result } = renderHook(() => useEntryCatalog([]));
    expect(result.current).toHaveLength(1);
    expect(result.current[0].description).toBe("compact does a thing");
  });
});
