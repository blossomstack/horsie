import { beforeEach, describe, expect, it } from "vitest";
import {
  DRAFT_STORAGE_KEY,
  emptyDraft,
  filterMcpServers,
  filterMemorySpaces,
  filterSkills,
  loadDraftPayload,
  parseDraftPayload,
  reconcileModelVendor,
  type DraftPayload,
} from "./draftPersistence";

const sample: DraftPayload = {
  v: 1,
  vendor: "velos",
  model: "sonnet",
  repos: { "owner/repo": "" },
  skills: ["bundle-a"],
  mcp: ["mcp-x"],
  memorySpaces: ["horsie"],
  thinkingEffort: "high",
};

beforeEach(() => localStorage.clear());

describe("parseDraftPayload", () => {
  it("accepts a well-formed payload", () => {
    expect(parseDraftPayload(sample)).toEqual(sample);
  });

  it("rejects a wrong version", () => {
    expect(parseDraftPayload({ ...sample, v: 2 })).toBeUndefined();
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
    expect(parseDraftPayload({ ...sample, repos: ["owner/repo"] })).toBeUndefined();
    expect(parseDraftPayload({ ...sample, vendor: 42 })).toBeUndefined();
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

describe("reconcileModelVendor", () => {
  const aliases = ["sonnet", "opus"];
  const vendors = ["local", "velos"];

  it("returns the same reference when both are still valid", () => {
    expect(reconcileModelVendor(sample, aliases, vendors, "local")).toBe(sample);
  });

  it("falls back to the first model when the stored one is gone", () => {
    const next = reconcileModelVendor({ ...sample, model: "gone" }, aliases, vendors, "local");
    expect(next.model).toBe("sonnet");
  });

  it("falls back to the default vendor when the stored one is gone or inactive", () => {
    const next = reconcileModelVendor({ ...sample, vendor: "gone" }, aliases, vendors, "local");
    expect(next.vendor).toBe("local");
  });

  it("clears the model when no models are configured", () => {
    const next = reconcileModelVendor(sample, [], vendors, "local");
    expect(next.model).toBe("");
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
  it("is all-empty with version 1", () => {
    expect(emptyDraft()).toEqual({
      v: 1,
      vendor: "",
      model: "",
      repos: {},
      skills: [],
      mcp: [],
      memorySpaces: [],
      thinkingEffort: "",
    });
  });
});
