import { beforeEach, describe, expect, it } from "vitest";
import {
  DRAFT_STORAGE_KEY,
  emptyDraft,
  filterMcpServers,
  filterMemorySpaces,
  filterSkills,
  loadDraftPayload,
  parseDraftPayload,
  fromEnvironmentSpec,
  reconcileModelEnvironment,
  toEnvironmentSpec,
  type DraftPayload,
} from "./draftPersistence";

const sample: DraftPayload = {
  v: 2,
  environment: { kind: "runtime", vendor: "velos", repos: { "owner/repo": "" } },
  model: "sonnet",
  skills: ["bundle-a"],
  mcp: ["mcp-x"],
  memorySpaces: ["horsie"],
  tools: null,
  thinkingEffort: "high",
  artifacts: [],
};

beforeEach(() => localStorage.clear());

describe("parseDraftPayload", () => {
  it("accepts a well-formed payload", () => {
    expect(parseDraftPayload(sample)).toEqual(sample);
  });

  it("rejects a wrong version, including the v1 shape it replaced", () => {
    expect(parseDraftPayload({ ...sample, v: 3 })).toBeUndefined();
    // v1 carried `vendor`/`repos` where the environment now is. Not migrated:
    // "no usable stored draft" is already the first-visit path.
    expect(
      parseDraftPayload({
        v: 1,
        vendor: "velos",
        model: "sonnet",
        repos: {},
        skills: [],
        mcp: [],
        memorySpaces: [],
        thinkingEffort: "",
      }),
    ).toBeUndefined();
  });

  it("accepts a named environment", () => {
    const named = {
      ...sample,
      environment: { kind: "named" as const, name: "staging" },
    };
    expect(parseDraftPayload(named)).toEqual(named);
  });

  it("rejects an environment that is neither shape", () => {
    for (const environment of [
      undefined,
      null,
      "staging",
      { kind: "named" },
      { kind: "runtime", vendor: "v" },
      { kind: "runtime", vendor: 1, repos: {} },
      { kind: "elsewhere" },
    ]) {
      expect(parseDraftPayload({ ...sample, environment })).toBeUndefined();
    }
  });

  it("rejects non-objects and missing fields", () => {
    expect(parseDraftPayload(null)).toBeUndefined();
    expect(parseDraftPayload("nope")).toBeUndefined();
    const noModel: Record<string, unknown> = { ...sample };
    delete noModel.model;
    expect(parseDraftPayload(noModel)).toBeUndefined();
  });

  it("rejects wrongly-typed fields", () => {
    expect(parseDraftPayload({ ...sample, skills: "bundle-a" })).toBeUndefined();
    expect(parseDraftPayload({ ...sample, skills: [1] })).toBeUndefined();
    expect(parseDraftPayload({ ...sample, model: 42 })).toBeUndefined();
  });
});

describe("loadDraftPayload", () => {
  it("returns undefined when the key is absent", () => {
    expect(loadDraftPayload()).toBeUndefined();
  });

  it("returns undefined for corrupt JSON", () => {
    localStorage.setItem(DRAFT_STORAGE_KEY, "{not json");
    expect(loadDraftPayload()).toBeUndefined();
  });

  it("round-trips a stored payload", () => {
    localStorage.setItem(DRAFT_STORAGE_KEY, JSON.stringify(sample));
    expect(loadDraftPayload()).toEqual(sample);
  });
});

describe("reconcileModelEnvironment", () => {
  const aliases = ["sonnet", "opus"];
  const vendors = ["local", "velos"];
  const names = ["staging"];

  it("returns the same reference when both are still valid", () => {
    expect(reconcileModelEnvironment(sample, aliases, vendors, "local", names)).toBe(
      sample,
    );
  });

  it("falls back to the first model when the stored one is gone", () => {
    const next = reconcileModelEnvironment(
      { ...sample, model: "gone" },
      aliases,
      vendors,
      "local",
      names,
    );
    expect(next.model).toBe("sonnet");
  });

  it("falls back to the default vendor when the stored one is gone or inactive", () => {
    const next = reconcileModelEnvironment(
      { ...sample, environment: { kind: "runtime", vendor: "gone", repos: {} } },
      aliases,
      vendors,
      "local",
      names,
    );
    expect(next.environment).toEqual({ kind: "runtime", vendor: "local", repos: {} });
  });

  it("falls back to the default runtime when the named environment is gone", () => {
    const next = reconcileModelEnvironment(
      { ...sample, environment: { kind: "named", name: "gone" } },
      aliases,
      vendors,
      "local",
      names,
    );
    expect(next.environment).toEqual({ kind: "runtime", vendor: "local", repos: {} });
  });

  it("keeps a named environment while the list has not arrived", () => {
    const named: DraftPayload = {
      ...sample,
      environment: { kind: "named", name: "staging" },
    };
    expect(
      reconcileModelEnvironment(named, aliases, vendors, "local", undefined),
    ).toBe(named);
  });

  it("clears the model when no models are configured", () => {
    const next = reconcileModelEnvironment(sample, [], vendors, "local", names);
    expect(next.model).toBe("");
  });
});

describe("wire mapping", () => {
  it("a named environment travels as its name alone", () => {
    expect(toEnvironmentSpec({ kind: "named", name: "staging" }, true)).toEqual({
      type: "Named",
      value: { name: "staging" },
    });
  });

  it("an ad-hoc environment sends its repos as clone URLs", () => {
    expect(
      toEnvironmentSpec(
        { kind: "runtime", vendor: "velos", repos: { "owner/repo": "dev" } },
        true,
      ),
    ).toEqual({
      type: "Runtime",
      value: {
        vendor: "velos",
        repos: [{ url: "https://github.com/owner/repo", gitRef: "dev" }],
      },
    });
  });

  it("drops repos a vendor that cannot provision has nowhere to put", () => {
    expect(
      toEnvironmentSpec(
        { kind: "runtime", vendor: "local", repos: { "owner/repo": "" } },
        false,
      ),
    ).toEqual({ type: "Runtime", value: { vendor: "local", repos: undefined } });
  });

  it("round-trips through the wire shape", () => {
    const draft = {
      kind: "runtime" as const,
      vendor: "velos",
      repos: { "owner/repo": "dev" },
    };
    expect(fromEnvironmentSpec(toEnvironmentSpec(draft, true))).toEqual(draft);
    const named = { kind: "named" as const, name: "staging" };
    expect(fromEnvironmentSpec(toEnvironmentSpec(named, true))).toEqual(named);
  });
});

describe("selection filters", () => {
  it("filterSkills drops bundles that are no longer installed", () => {
    const next = filterSkills({ ...sample, skills: ["bundle-a", "gone"] }, new Set(["bundle-a"]));
    expect(next.skills).toEqual(["bundle-a"]);
  });

  it("filterMcpServers drops servers that are no longer enabled", () => {
    const next = filterMcpServers({ ...sample, mcp: ["mcp-x", "gone"] }, new Set(["mcp-x"]));
    expect(next.mcp).toEqual(["mcp-x"]);
  });

  it("filterMemorySpaces drops spaces that no longer exist", () => {
    const next = filterMemorySpaces(
      { ...sample, memorySpaces: ["horsie", "gone"] },
      new Set(["horsie"]),
    );
    expect(next.memorySpaces).toEqual(["horsie"]);
  });

  it("each filter returns the same reference when nothing is stale", () => {
    expect(filterSkills(sample, new Set(["bundle-a"]))).toBe(sample);
    expect(filterMcpServers(sample, new Set(["mcp-x"]))).toBe(sample);
    expect(filterMemorySpaces(sample, new Set(["horsie"]))).toBe(sample);
  });
});

describe("emptyDraft", () => {
  it("is all-empty with version 2", () => {
    expect(emptyDraft()).toEqual({
      v: 2,
      environment: { kind: "runtime", vendor: "", repos: {} },
      model: "",
      skills: [],
      mcp: [],
      memorySpaces: [],
      tools: null,
      thinkingEffort: "",
      artifacts: [],
    });
  });
});

describe("stored artifacts", () => {
  const ref = {
    id: "sha-1",
    mediaType: "image/png",
    byteSize: 1234,
    kind: { kind: "Image", value: { width: 8, height: 6 } },
    filename: "shot.png",
  };

  it("round-trips a reference", () => {
    const stored = { ...sample, artifacts: [ref] };
    expect(parseDraftPayload(stored)).toEqual(stored);
  });

  // References only: the bytes are already in the artifact service, and
  // localStorage has a few megabytes in total.
  it("stores plain JSON — no bytes, no Map, no Set", () => {
    const stored = { ...sample, artifacts: [ref] };
    expect(JSON.parse(JSON.stringify(stored))).toEqual(stored);
  });

  // A draft written before attachments existed has none, which is what an
  // empty list says — so there is nothing to migrate.
  it("reads a draft that predates the field as having no attachments", () => {
    const { artifacts: _dropped, ...older } = sample;
    expect(parseDraftPayload(older)?.artifacts).toEqual([]);
  });

  it("drops a malformed reference without losing the rest of the draft", () => {
    const parsed = parseDraftPayload({
      ...sample,
      artifacts: [
        ref,
        { id: "no-kind", mediaType: "image/png", byteSize: 1 },
        { id: 7, mediaType: "image/png", byteSize: 1, kind: { kind: "Image", value: {} } },
        "nonsense",
      ],
    });
    expect(parsed?.artifacts).toEqual([ref]);
    expect(parsed?.model).toBe(sample.model);
  });

  it("keeps a document reference, which carries no dimensions", () => {
    const doc = {
      id: "sha-2",
      mediaType: "application/pdf",
      byteSize: 10,
      kind: { kind: "Document", value: {} },
    };
    expect(parseDraftPayload({ ...sample, artifacts: [doc] })?.artifacts).toEqual(
      [doc],
    );
  });
});
