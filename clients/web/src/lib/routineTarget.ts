import type { RoutineTarget } from "../api/types";

/**
 * What a routine runs, in one shape both the list and the detail page read.
 *
 * A workflow and an agent preset are both slugs, so a bare name cannot say
 * which kind it is — and the kind is the first thing you need to guess what a
 * firing will produce. The label carries it; `to` is where that thing is
 * edited.
 */
export function targetOf(target: RoutineTarget): {
  kind: "agent" | "workflow";
  name: string;
  to: string;
} {
  return target.type === "Agent"
    ? {
        kind: "agent",
        name: target.value.agent,
        to: `/agents/${encodeURIComponent(target.value.agent)}/edit`,
      }
    : {
        kind: "workflow",
        name: target.value.workflow,
        to: `/workflows/${encodeURIComponent(target.value.workflow)}`,
      };
}
