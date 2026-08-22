import { useState } from "react";
import type { EnvironmentSpec } from "../api/types";
import {
  fromEnvironmentSpec,
  toEnvironmentSpec,
  type EnvironmentDraft,
} from "./draftPersistence";
import { useEnvironments } from "./useEnvironments";
import { useGithubStatus } from "./useGithub";
import { useSettings } from "./useSettings";
import type { EnvironmentChannel } from "./useSessionDraft";

/**
 * A standalone environment channel for a form that has one field, not a whole
 * session draft — today the routine editor.
 *
 * `useSessionDraft` keeps its own copy because its state is persisted to
 * localStorage alongside every other channel. What the two share is the pure
 * mapping in `draftPersistence`, which is where the rules that could drift
 * actually live.
 */
export interface StandaloneEnvironment extends EnvironmentChannel {
  /** The wire value to save. */
  spec: EnvironmentSpec;
  /** Something has been chosen — what a form gates its save on. */
  chosen: boolean;
}

export function useEnvironmentChannel(
  initial?: EnvironmentSpec,
): StandaloneEnvironment {
  const { data: settings } = useSettings();
  const { data: environments } = useEnvironments();
  const { data: ghStatus } = useGithubStatus();
  const [environment, setEnvironment] = useState<EnvironmentDraft>(() =>
    initial ? fromEnvironmentSpec(initial) : { kind: "runtime", vendor: "", repos: {} },
  );

  // Which vendor this selection resolves to — its own, or the predefined
  // environment's. One lookup, so every downstream answer agrees.
  const resolvedVendor =
    environment.kind === "named"
      ? (environments ?? []).find((e) => e.name === environment.name)?.vendor
      : environment.kind === "none"
        ? undefined
        : environment.vendor;
  const provisions = !!(settings?.vendors ?? []).find(
    (v) => v.name === resolvedVendor,
  )?.capabilities?.supportsProvisioning;

  return {
    environment,
    setEnvironment,
    environments: environments ?? [],
    provisions,
    githubConnected: !!ghStatus?.connected,
    spec: toEnvironmentSpec(environment, provisions),
    // A runtime-less session has chosen: "nowhere" is an answer, not a
    // blank.
    chosen:
      environment.kind === "named"
        ? !!environment.name
        : environment.kind === "none" || !!environment.vendor.trim(),
  };
}
