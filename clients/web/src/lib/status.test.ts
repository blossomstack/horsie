import { describe, expect, it } from "vitest";
import { SessionStatusKind } from "../api/types";
import { statusMeta, TONE_TEXT } from "./status";

describe("status presentation metadata", () => {
  it("keeps Running prominent, amber, and animated", () => {
    const meta = statusMeta(SessionStatusKind.Running);

    expect(meta.tone).toBe("live");
    expect(meta.busy).toBe(true);
    expect(TONE_TEXT[meta.tone]).toBe("text-amber-ink");
  });

  it("renders Idle with a subdued neutral tone and no animation", () => {
    const meta = statusMeta(SessionStatusKind.Idle);

    expect(meta.tone).toBe("idle");
    expect(meta.busy).toBe(false);
    expect(TONE_TEXT[meta.tone]).toBe("text-dim");
  });

  it("keeps an unknown status separate from Idle", () => {
    expect(statusMeta(undefined).tone).toBe("off");
    expect(statusMeta(null).tone).toBe("off");
  });
});
