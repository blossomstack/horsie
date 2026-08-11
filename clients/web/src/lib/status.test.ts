import { describe, expect, it } from "vitest";
import { SessionStatusKind } from "../api/types";
import {
  progressionLabel,
  showsProgression,
  statusMeta,
  TONE_TEXT,
} from "./status";

describe("status presentation metadata", () => {
  // A run that ran to completion and one that stopped part-way both rest, and
  // the list exists to tell them apart — so `Finished` has to read settled and
  // successful, not merely unlit.
  it("gives a finished run its own settled lamp", () => {
    const meta = statusMeta(SessionStatusKind.Finished);

    expect(meta.label).toBe("Finished");
    expect(meta.busy).toBe(false);
    expect(meta.tone).not.toBe("off");
    expect(meta.tone).not.toBe(statusMeta(SessionStatusKind.Idle).tone);
  });

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
