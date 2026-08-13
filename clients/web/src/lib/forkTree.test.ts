import { describe, expect, it } from "vitest";
import { forkTree } from "./forkTree";
import type { ForkView } from "../api/types";

function fork(id: string, parent?: string, createdAtMs = 1): ForkView {
  return { id, parent, title: undefined, status: "idle", createdAtMs };
}

describe("forkTree", () => {
  it("places a fork of a fork under the fork it came from", () => {
    const placed = forkTree([fork("a"), fork("b", "a")]);
    expect(placed.map((p) => [p.fork.id, p.depth])).toEqual([
      ["a", 0],
      ["b", 1],
    ]);
  });

  it("nests to any depth", () => {
    const placed = forkTree([fork("a"), fork("b", "a"), fork("c", "b")]);
    expect(placed.map((p) => p.depth)).toEqual([0, 1, 2]);
  });

  it("orders siblings oldest first, so a rename never moves a row", () => {
    const placed = forkTree([fork("b", undefined, 20), fork("a", undefined, 10)]);
    expect(placed.map((p) => p.fork.id)).toEqual(["a", "b"]);
  });

  /* Deleting a fork leaves its children alive. They must still be reachable. */
  it("shows a fork whose parent is gone at the top level", () => {
    const placed = forkTree([fork("orphan", "deleted")]);
    expect(placed).toEqual([{ fork: fork("orphan", "deleted"), depth: 0 }]);
  });

  it("shows a fork the descent cannot reach rather than dropping it", () => {
    // Not producible by appending, but this walks data off a journal.
    const cyclic = [fork("a", "b"), fork("b", "a")];
    expect(forkTree(cyclic).map((p) => p.fork.id).sort()).toEqual(["a", "b"]);
  });

  it("is empty for a session nobody forked", () => {
    expect(forkTree([])).toEqual([]);
  });
});
