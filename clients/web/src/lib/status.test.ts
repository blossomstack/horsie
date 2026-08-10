import { describe, expect, it } from "vitest";
import { SessionStatusKind } from "../api/types";
import {
  progressionLabel,
  runStatusMeta,
  showsProgression,
  statusMeta,
  TONE_TEXT,
} from "./status";

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

describe("progression presentation", () => {
  // The stage keys are the server's, and there are two sources of them: a
  // `Preparing` entry names its own stage, while a `Runtime` entry is folded
  // into `runtime_<status>`. Both used to miss the map entirely — it was keyed
  // on `provisioning_runtime`, which nothing has ever emitted — so the whole
  // provisioning wait read as "runtime acquiring…".
  it("labels every stage the server actually emits", () => {
    expect(progressionLabel("runtime_acquiring")).toBe("Starting runtime…");
    expect(progressionLabel("acquiring_runtime")).toBe("Starting runtime…");
    expect(progressionLabel("scanning_workspace")).toBe("Scanning workspace…");
    expect(progressionLabel("connecting_tools")).toBe("Connecting tools…");
  });

  it("de-slugs a stage it has never heard of", () => {
    expect(progressionLabel("warming_caches")).toBe("warming caches…");
  });

  // Both ends of the wait, from both sources. `runtime_ready` used to sit above
  // the composer as a lamp and the words "runtime ready…" from the moment a
  // session finished provisioning until its next turn began.
  it("hides a wait that has settled, whichever source said so", () => {
    expect(showsProgression("ready")).toBe(false);
    expect(showsProgression("runtime_ready")).toBe(false);
    expect(showsProgression("runtime_acquiring")).toBe(true);
    expect(showsProgression("scanning_workspace")).toBe(true);
    expect(showsProgression(undefined)).toBe(false);
  });
});

describe("run presentation metadata", () => {
  /** The whole point of the vocabulary: the three outcomes a person scanning a
   * list of past runs is actually asking about have to be three different
   * lamps, not one. */
  it("gives success, failure and parked-on-a-question distinct lamps", () => {
    const finished = runStatusMeta({ type: "Finished", value: {} });
    const failed = runStatusMeta({ type: "Failed", value: {} });
    const parked = runStatusMeta({ type: "AwaitingInput", value: {} });

    expect(finished.tone).toBe("ready");
    expect(failed.tone).toBe("fault");
    expect(parked.tone).toBe("attention");
    expect(
      new Set([finished.label, failed.label, parked.label]).size,
    ).toBe(3);
    expect(
      new Set([
        TONE_TEXT[finished.tone],
        TONE_TEXT[failed.tone],
        TONE_TEXT[parked.tone],
      ]).size,
    ).toBe(3);
  });

  /** A run's lifecycle state is durable, so unlike a session's there is nothing
   * to be unknown about: no run may read as the em dash a session shows when it
   * is merely not loaded. */
  it("labels every run state rather than falling back to an em dash", () => {
    for (const type of [
      "Pending",
      "Running",
      "Suspended",
      "AwaitingInput",
      "Finished",
      "Failed",
    ] as const) {
      expect(runStatusMeta({ type, value: {} }).label).not.toBe("—");
    }
  });

  it("animates only a run with a step actually working", () => {
    expect(runStatusMeta({ type: "Running", value: {} }).busy).toBe(true);
    expect(runStatusMeta({ type: "AwaitingInput", value: {} }).busy).toBe(false);
    expect(runStatusMeta({ type: "Finished", value: {} }).busy).toBe(false);
  });
});
