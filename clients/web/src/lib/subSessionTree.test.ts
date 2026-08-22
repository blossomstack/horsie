import { describe, expect, it } from "vitest";
import { subSessionReadyToOpen, subSessionTree } from "./subSessionTree";
import type { SubSessionView } from "../api/types";

function subSession(id: string, parent?: string, createdAtMs = 1): SubSessionView {
  return { id, parent, title: "a branch", status: "idle", createdAtMs, lastActivityMs: createdAtMs };
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
      { subSession: subSession("orphan", "deleted"), depth: 0 },
    ]);
  });

  it("shows a subSession the descent cannot reach rather than dropping it", () => {
    // Not producible by appending, but this walks data off a journal.
    const cyclic = [subSession("a", "b"), subSession("b", "a")];
    expect(subSessionTree(cyclic).map((p) => p.subSession.id).sort()).toEqual(["a", "b"]);
  });

  it("is empty for a session nobody branched", () => {
    expect(subSessionTree([])).toEqual([]);
  });
});

describe("subSessionReadyToOpen", () => {
  const row = (id: string, status: string): SubSessionView => ({
    id,
    title: "a branch",
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
