import { describe, expect, it } from "vitest";
import type { SessionSummary } from "../api/types";
import { SessionStatusKind } from "../api/types";
import {
  UNGROUPED,
  moveBefore,
  partitionSessions,
  reconcileOrder,
  sessionGroup,
  unionGroups,
} from "./sessionGroups";

function session(id: string, group?: string): SessionSummary {
  return {
    id,
    status: SessionStatusKind.Idle,
    createdAt: 1,
    annotations: group ? [{ key: "group", value: group }] : [],
    forks: [],
  };
}

describe("sessionGroup", () => {
  it("reads the group annotation", () => {
    expect(sessionGroup(session("a", "web"))).toBe("web");
    expect(sessionGroup(session("a"))).toBeUndefined();
  });
});

describe("unionGroups", () => {
  it("unions registered groups with annotation-only groups, deduped, reserved word dropped", () => {
    const sessions = [session("a", "web"), session("b", "ops")];
    expect(unionGroups(["api", "web", UNGROUPED], sessions)).toEqual([
      "api",
      "web",
      "ops",
    ]);
  });
});

describe("partitionSessions", () => {
  it("buckets every session and keeps empty groups present", () => {
    const sessions = [session("a", "web"), session("b")];
    const parts = partitionSessions(sessions, ["web", "empty"]);
    expect(parts.get("web")?.map((s) => s.id)).toEqual(["a"]);
    expect(parts.get(UNGROUPED)?.map((s) => s.id)).toEqual(["b"]);
    expect(parts.get("empty")).toEqual([]);
  });
});

describe("reconcileOrder", () => {
  it("keeps saved live entries, appends new groups sorted, ungrouped exactly once", () => {
    expect(
      reconcileOrder(["gone", "web", UNGROUPED], ["web", "api", "ops"]),
    ).toEqual(["web", UNGROUPED, "api", "ops"]);
  });

  it("appends ungrouped when the saved order lacks it", () => {
    expect(reconcileOrder(["web"], ["web"])).toEqual(["web", UNGROUPED]);
  });
});

describe("moveBefore", () => {
  it("reinserts the entry before the target", () => {
    expect(moveBefore(["a", "b", "c"], "c", "a")).toEqual(["c", "a", "b"]);
    expect(moveBefore(["a", "b"], "a", "missing")).toEqual(["b", "a"]);
  });
});
