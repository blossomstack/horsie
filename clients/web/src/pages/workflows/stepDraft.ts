import {
  StepFieldType,
  type OutcomeFilter,
  type StepField,
  type StepOutcome,
  type WorkflowStepDef,
  type WorkflowTransition,
} from "../../api/types";

/** The two values a step's `outcome` takes when its author names none. */
export const defaultOutcomes = (): StepOutcome[] => [
  { value: "success", description: "The step did what it was asked to do." },
  { value: "failure", description: "The step could not do what it was asked to do." },
];

export const emptyOutcome = (): StepOutcome => ({ value: "", description: "" });

/**
 * The label an edge carries — `outcome in [p0, p1]`.
 *
 * The same string the server renders into the run log, so the definition graph
 * and a run's graph label an edge identically. Two renderings of one filter
 * would drift, and nobody would see it.
 */
export function renderFilter(when: OutcomeFilter | undefined): string | undefined {
  if (when === undefined) return undefined;
  const op = when.op === "In" ? "in" : "not in";
  return `outcome ${op} [${when.value.values.join(", ")}]`;
}

/** The outcomes a filter names, whichever way round it is. */
export const filterValues = (when: OutcomeFilter | undefined): string[] =>
  when === undefined ? [] : when.value.values;

/** A filter of `op` naming `values`, in the wire's adjacently-tagged shape. */
export const makeFilter = (op: "In" | "NotIn", values: string[]): OutcomeFilter =>
  op === "In" ? { op: "In", value: { values } } : { op: "NotIn", value: { values } };

export const emptyField = (): StepField => ({
  name: "",
  kind: StepFieldType.String,
  description: "",
  required: false,
});

export interface StepDraft {
  /** Client-side identity, so selection and reordering survive a rename. */
  id: string;
  name: string;
  agent: string;
  prompt: string;
  /** The values this step's `outcome` may take. Always at least one: a step
   * with none could not be routed out of. */
  outcomes: StepOutcome[];
  /** Extra result fields, beyond `outcome` and `description`. */
  fields: StepField[];
  /** Whether the step may ask the person a question. */
  interactive: boolean;
  transitions: WorkflowTransition[];
  /** Per-step budgets, carried through rather than edited.
   *
   * The form has no control for either, and a save is a full replace — so
   * dropping them here destroyed whatever the API had set. Preserved the same
   * way `rawSchema` is, for the same reason. */
  maxIterations: number | undefined;
  maxRetries: number | undefined;
}

let counter = 0;
export const newStepId = () => `step-${++counter}`;

export function toDraft(step: WorkflowStepDef): StepDraft {
  const outcomes = step.outcomes ?? [];
  return {
    id: newStepId(),
    name: step.name,
    agent: step.agent,
    prompt: step.prompt,
    // A step that declared none runs on success/failure, so that is what the
    // form shows — editing it should start from what the step actually does,
    // not from an empty list that says nothing.
    outcomes: outcomes.length > 0 ? outcomes : defaultOutcomes(),
    fields: step.fields ?? [],
    interactive: step.interactive ?? false,
    transitions: step.transitions ?? [],
    maxIterations: step.maxIterations ?? undefined,
    maxRetries: step.maxRetries ?? undefined,
  };
}

export function fromDraft(d: StepDraft): WorkflowStepDef {
  const outcomes = d.outcomes.filter((o) => o.value.trim() !== "");
  const fields = d.fields.filter((f) => f.name.trim() !== "");
  return {
    name: d.name.trim(),
    agent: d.agent,
    prompt: d.prompt,
    outcomes: outcomes.length > 0 ? outcomes : undefined,
    fields: fields.length > 0 ? fields : undefined,
    interactive: d.interactive ? true : undefined,
    transitions: d.transitions.length > 0 ? d.transitions : undefined,
    maxIterations: d.maxIterations,
    maxRetries: d.maxRetries,
  };
}

export const emptyStep = (n: number): StepDraft => ({
  id: newStepId(),
  name: n === 0 ? "start" : `step-${n + 1}`,
  agent: "",
  prompt: "",
  outcomes: defaultOutcomes(),
  fields: [],
  interactive: false,
  transitions: [],
  maxIterations: undefined,
  maxRetries: undefined,
});

/**
 * Rename a step and carry every reference to it.
 *
 * Renaming used to change only the step's own `name`, leaving other steps'
 * `transitions[].to` — and the workflow's `start` — pointing at a name that no
 * longer existed. Save then failed naming a step that appeared nowhere in the
 * form, and since the seeded first step is called `start`, renaming it is the
 * first thing anyone does.
 *
 * A rewrite, not a refusal: the references are inside the object being edited,
 * so there is nothing to ask the user about. Returns the new list and the new
 * `start`.
 *
 * An empty or unchanged new name rewrites nothing — a half-typed name must not
 * repoint transitions on every keystroke.
 */
export function renameStep(
  steps: StepDraft[],
  id: string,
  nextName: string,
  start: string,
): { steps: StepDraft[]; start: string } {
  const target = steps.find((s) => s.id === id);
  const from = target?.name.trim() ?? "";
  const to = nextName.trim();
  const renaming = from !== "" && to !== "" && from !== to;

  const next = steps.map((s) => {
    const named = s.id === id ? { ...s, name: nextName } : s;
    if (!renaming) return named;
    return {
      ...named,
      transitions: named.transitions.map((t) =>
        t.to.trim() === from ? { ...t, to } : t,
      ),
    };
  });
  return { steps: next, start: renaming && start === from ? to : start };
}
