import { describe, expect, it } from "vitest";
import type { SessionSummary } from "../api/types";
import { SessionStatusKind } from "../api/types";
import {
  allTags,
  cycleTag,
  EMPTY_FILTER,
  filterIsActive,
  matchesTagFilter,
  normalizeTagName,
  reconcileFilter,
  sessionTags,
  tagState,
} from "./sessionTags";

function session(
  id: string,
  tags: string[] = [],
  extra: { key: string; value: string }[] = [],
): SessionSummary {
  return {
    id,
    name: `session ${id}`,
    status: SessionStatusKind.Idle,
    createdAt: 1,
    annotations: [
      ...tags.map((t) => ({ key: `tag.${t}`, value: "" })),
      ...extra,
    ],
    forks: [],
  };
}

describe("sessionTags", () => {
  it("reads tag.* keys, sorted, ignoring other annotations", () => {
    expect(
      sessionTags(
        session("a", ["web", "api"], [{ key: "source", value: "routine" }]),
      ),
    ).toEqual(["api", "web"]);
  });

  it("treats a bare `tag.` key as no tag", () => {
    expect(sessionTags(session("a", [], [{ key: "tag.", value: "" }]))).toEqual(
      [],
    );
  });

  it("keeps a dotted tag whole", () => {
    expect(sessionTags(session("a", ["v2.migration"]))).toEqual([
      "v2.migration",
    ]);
  });
});

describe("allTags", () => {
  it("unions and dedupes across sessions, sorted", () => {
    expect(allTags([session("a", ["web"]), session("b", ["api", "web"])])).toEqual([
      "api",
      "web",
    ]);
  });

  it("forgets a tag once its last carrier is gone", () => {
    expect(allTags([session("a")])).toEqual([]);
  });
});

describe("normalizeTagName", () => {
  it("lowercases and hyphenates whitespace", () => {
    expect(normalizeTagName("  Bug   Fix ")).toBe("bug-fix");
  });

  it("strips characters the annotation key charset rejects", () => {
    expect(normalizeTagName("we:b!")).toBe("web");
  });

  it("rejects a name that normalises to nothing", () => {
    expect(normalizeTagName("  !!  ")).toBeUndefined();
  });

  it("rejects a name over 124 characters", () => {
    expect(normalizeTagName("a".repeat(125))).toBeUndefined();
    expect(normalizeTagName("a".repeat(124))).toBe("a".repeat(124));
  });
});

describe("matchesTagFilter", () => {
  const s = session("a", ["web", "done"]);

  it("matches everything when empty", () => {
    expect(matchesTagFilter(s, EMPTY_FILTER)).toBe(true);
    expect(matchesTagFilter(session("b"), EMPTY_FILTER)).toBe(true);
  });

  it("ANDs every required tag", () => {
    expect(matchesTagFilter(s, { require: ["web", "done"], exclude: [] })).toBe(
      true,
    );
    expect(matchesTagFilter(s, { require: ["web", "api"], exclude: [] })).toBe(
      false,
    );
  });

  it("rejects a session carrying an excluded tag", () => {
    expect(matchesTagFilter(s, { require: [], exclude: ["done"] })).toBe(false);
    expect(
      matchesTagFilter(session("b"), { require: [], exclude: ["done"] }),
    ).toBe(true);
  });

  it("matches nothing when a tag is both required and excluded", () => {
    expect(matchesTagFilter(s, { require: ["web"], exclude: ["web"] })).toBe(
      false,
    );
  });
});

describe("cycleTag / tagState", () => {
  it("cycles neutral to require to exclude and back", () => {
    let f = EMPTY_FILTER;
    expect(tagState(f, "web")).toBe("neutral");
    f = cycleTag(f, "web");
    expect(tagState(f, "web")).toBe("require");
    f = cycleTag(f, "web");
    expect(tagState(f, "web")).toBe("exclude");
    f = cycleTag(f, "web");
    expect(tagState(f, "web")).toBe("neutral");
    expect(filterIsActive(f)).toBe(false);
  });

  it("leaves other tags alone", () => {
    const f = cycleTag({ require: ["api"], exclude: [] }, "web");
    expect(f.require).toEqual(["api", "web"]);
  });
});

describe("reconcileFilter", () => {
  it("drops constraints naming a tag nobody carries", () => {
    expect(
      reconcileFilter({ require: ["web", "gone"], exclude: ["dead"] }, ["web"]),
    ).toEqual({ require: ["web"], exclude: [] });
  });
});
