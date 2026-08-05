import { describe, expect, it } from "vitest";
import { afterRemoval, DEFINITION, isSelected, moveItem } from "./stepList";

describe("moveItem", () => {
  it("moves an item later, shifting the ones it passes", () => {
    expect(moveItem(["a", "b", "c"], 0, 2)).toEqual(["b", "c", "a"]);
  });

  it("moves an item earlier", () => {
    expect(moveItem(["a", "b", "c"], 2, 0)).toEqual(["c", "a", "b"]);
  });

  it("returns the same list when nothing moves", () => {
    const list = ["a", "b"];
    expect(moveItem(list, 1, 1)).toBe(list);
  });

  /// An arrow key at either end, and a drop outside the list, both land here.
  it("ignores an index outside the list", () => {
    const list = ["a", "b"];
    expect(moveItem(list, 0, -1)).toBe(list);
    expect(moveItem(list, 1, 2)).toBe(list);
    expect(moveItem(list, 5, 0)).toBe(list);
  });
});

describe("afterRemoval", () => {
  const sel = (id: string) => ({ kind: "step" as const, id });

  it("keeps the selection when some other step is removed", () => {
    expect(afterRemoval(["a", "b", "c"], 0, sel("c"))).toEqual(sel("c"));
  });

  it("selects the step that slides into the freed slot", () => {
    expect(afterRemoval(["a", "b", "c"], 1, sel("b"))).toEqual(sel("c"));
  });

  it("falls back to the previous step when the last one is removed", () => {
    expect(afterRemoval(["a", "b"], 1, sel("b"))).toEqual(sel("a"));
  });

  it("falls back to the definition when the only step is removed", () => {
    expect(afterRemoval(["a"], 0, sel("a"))).toEqual(DEFINITION);
  });

  it("leaves the definition selected", () => {
    expect(afterRemoval(["a"], 0, DEFINITION)).toEqual(DEFINITION);
  });

  it("ignores an index that names no step", () => {
    expect(afterRemoval(["a"], 3, sel("a"))).toEqual(sel("a"));
  });
});

describe("isSelected", () => {
  it("is false for the definition", () => {
    expect(isSelected(DEFINITION, "a")).toBe(false);
  });

  it("matches only the selected id", () => {
    expect(isSelected({ kind: "step", id: "a" }, "a")).toBe(true);
    expect(isSelected({ kind: "step", id: "a" }, "b")).toBe(false);
  });
});
