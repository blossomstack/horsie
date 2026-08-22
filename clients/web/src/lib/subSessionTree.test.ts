import { describe, expect, it } from "vitest";
import { subSessionReadyToOpen, subSessionTree } from "./subSessionTree";
import type { SubSessionView } from "../api/types";

function subSession(id: string, parent?: string, createdAtMs = 1): SubSessionView {
  return { id, parent, title: undefined, status: "idle", createdAtMs, lastActivityMs: createdAtMs };
}

describe("subSessionTree", () => {
  it("places a subSession of a subSession under the subSession it came from", () => {
    const placed = subSessionTree([subSession("a"), subSession("b", "a")]);
    expect(placed.map((p) => [p.subSession.id, p.depth])).toEqual([
      ["a", 0],
      ["b", 1],
    ]);
  });

  it("nests to any depth", () => {
    const placed = subSessionTree([subSession("a"), subSession("b", "a"), subSession("c", "b")]);
    expect(placed.map((p) => p.depth)).toEqual([0, 1, 2]);
  });

  it("orders siblings oldest first, so a rename never moves a row", () => {
    const placed = subSessionTree([subSession("b", undefined, 20), subSession("a", undefined, 10)]);
    expect(placed.map((p) => p.subSession.id)).toEqual(["a", "b"]);
  });

  /* Deleting a sub session leaves its children alive. They must still be reachable. */
  it("shows a subSession whose parent is gone at the top level", () => {
    const placed = subSessionTree([subSession("orphan", "deleted")]);
    expect(placed).toEqual([
      { subSession: subSession("orphan", "deleted"), depth: 0, rails: [], last: true },
    ]);
  });

  it("marks the last child of a level, so it draws an elbow and not a tee", () => {
    const placed = subSessionTree([subSession("a", undefined, 1), subSession("b", undefined, 2)]);
    expect(placed.map((p) => [p.subSession.id, p.last])).toEqual([
      ["a", false],
      ["b", true],
    ]);
  });

  /* The rail that makes a deep sub session read as descended from its grandparent
     rather than merely printed below it. */
  it("carries a rail through the column of an ancestor with more siblings", () => {
    //  a          <- has a sibling below (c), so its column keeps a rail
    //  └ b        <- and b's own column is drawn beside that rail
    //  c
    const placed = subSessionTree([
      subSession("a", undefined, 1),
      subSession("b", "a", 2),
      subSession("c", undefined, 3),
    ]);
    expect(placed.map((p) => [p.subSession.id, p.rails])).toEqual([
      ["a", []],
      ["b", [true]],
      ["c", []],
    ]);
  });

  it("leaves the column blank once an ancestor has nothing below it", () => {
    // `a` is last at its level, so nothing continues past it in that column.
    const placed = subSessionTree([subSession("a", undefined, 1), subSession("b", "a", 2)]);
    expect(placed.map((p) => p.rails)).toEqual([[], [false]]);
  });

  it("gives a rail per ancestor level, however deep", () => {
    const placed = subSessionTree([
      subSession("a", undefined, 1),
      subSession("b", "a", 2),
      subSession("c", "b", 3),
    ]);
    expect(placed.map((p) => p.rails.length)).toEqual([0, 1, 2]);
  });

  it("shows a subSession the descent cannot reach rather than dropping it", () => {
    // Not producible by appending, but this walks data off a journal.
    const cyclic = [subSession("a", "b"), subSession("b", "a")];
    expect(subSessionTree(cyclic).map((p) => p.subSession.id).sort()).toEqual(["a", "b"]);
  });

  it("is empty for a session nobody subSessioned", () => {
    expect(subSessionTree([])).toEqual([]);
  });
});

describe("subSessionReadyToOpen", () => {
  const row = (id: string, status: string): SubSessionView => ({
    id,
    status,
    createdAtMs: 1,
    lastActivityMs: 1,
  });

  it("holds while the subSession is still provisioning", () => {
    expect(subSessionReadyToOpen([row("f1", "provisioning")], "f1")).toBe(false);
  });

  it("holds when the roster has not reached us yet", () => {
    expect(subSessionReadyToOpen([], "f1")).toBe(false);
    expect(subSessionReadyToOpen(undefined, "f1")).toBe(false);
  });

  it("opens once the subSession has a history", () => {
    expect(subSessionReadyToOpen([row("f1", "idle")], "f1")).toBe(true);
  });

  // A seed that failed is still worth opening: the sub session's own page is where
  // the failure is reported, and holding would strand the user on the source
  // with no sign anything went wrong.
  it("opens a subSession whose seed failed", () => {
    expect(subSessionReadyToOpen([row("f1", "failed")], "f1")).toBe(true);
  });
});
