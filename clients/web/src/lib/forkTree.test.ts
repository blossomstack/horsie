import { describe, expect, it } from "vitest";
import { forkTree } from "./forkTree";
import type { ForkView } from "../api/types";

function fork(id: string, parent?: string, createdAtMs = 1): ForkView {
  return { id, parent, title: undefined, status: "idle", createdAtMs, lastActivityMs: createdAtMs };
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
    expect(placed).toEqual([
      { fork: fork("orphan", "deleted"), depth: 0, rails: [], last: true },
    ]);
  });

  it("marks the last child of a level, so it draws an elbow and not a tee", () => {
    const placed = forkTree([fork("a", undefined, 1), fork("b", undefined, 2)]);
    expect(placed.map((p) => [p.fork.id, p.last])).toEqual([
      ["a", false],
      ["b", true],
    ]);
  });

  /* The rail that makes a deep fork read as descended from its grandparent
     rather than merely printed below it. */
  it("carries a rail through the column of an ancestor with more siblings", () => {
    //  a          <- has a sibling below (c), so its column keeps a rail
    //  └ b        <- and b's own column is drawn beside that rail
    //  c
    const placed = forkTree([
      fork("a", undefined, 1),
      fork("b", "a", 2),
      fork("c", undefined, 3),
    ]);
    expect(placed.map((p) => [p.fork.id, p.rails])).toEqual([
      ["a", []],
      ["b", [true]],
      ["c", []],
    ]);
  });

  it("leaves the column blank once an ancestor has nothing below it", () => {
    // `a` is last at its level, so nothing continues past it in that column.
    const placed = forkTree([fork("a", undefined, 1), fork("b", "a", 2)]);
    expect(placed.map((p) => p.rails)).toEqual([[], [false]]);
  });

  it("gives a rail per ancestor level, however deep", () => {
    const placed = forkTree([
      fork("a", undefined, 1),
      fork("b", "a", 2),
      fork("c", "b", 3),
    ]);
    expect(placed.map((p) => p.rails.length)).toEqual([0, 1, 2]);
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
