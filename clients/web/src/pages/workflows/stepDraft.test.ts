import { describe, expect, it } from "vitest";
import type { WorkflowStepDef } from "../../api/types";
import { fromDraft, renameStep, schemaFields, toDraft } from "./stepDraft";

/**
 * The editor holds a step as a draft and writes it back whole — a save is a
 * full replace — so anything the draft drops is destroyed.
 */
describe("stepDraft", () => {
  const step = (over: Partial<WorkflowStepDef> = {}): WorkflowStepDef => ({
    name: "triage",
    agent: "triager",
    prompt: "Triage it.",
    outputSchema: { type: "object", properties: { severity: { type: "string" } } },
    transitions: [{ to: "fix", condition: 'output.severity == "p0"' }],
    maxIterations: undefined,
    maxRetries: undefined,
    ...over,
  });

  it("round-trips a step the form can fully represent", () => {
    expect(fromDraft(toDraft(step()))).toEqual(step());
  });

  /** The form has no control for either budget, and it used to write both back
   * as `undefined` — so opening a workflow in the browser and saving it wiped
   * whatever the API had set. */
  it("preserves per-step budgets it cannot edit", () => {
    const withBudgets = step({ maxIterations: 12, maxRetries: 3 });
    const saved = fromDraft(toDraft(withBudgets));
    expect(saved.maxIterations).toBe(12);
    expect(saved.maxRetries).toBe(3);
  });

  /** Same rule for a schema the flat field editor cannot express: kept verbatim
   * rather than flattened into something else. */
  it("preserves an output schema the field editor cannot represent", () => {
    const nested = {
      type: "object",
      properties: { detail: { type: "object", properties: { n: { type: "number" } } } },
    };
    expect(schemaFields(nested)).toBeNull();
    expect(fromDraft(toDraft(step({ outputSchema: nested }))).outputSchema).toEqual(
      nested,
    );
  });
});

/**
 * Renaming a step used to change only its own `name`, leaving other steps'
 * transition targets and the workflow's `start` pointing at a name that no
 * longer existed. The save then failed naming a step absent from the form —
 * and since the seeded first step is called `start`, renaming it is the first
 * thing anyone does.
 */
describe("renameStep", () => {
  const draft = (name: string, to: string[] = []) => ({
    id: `id-${name}`,
    name,
    agent: "",
    prompt: "",
    fields: [],
    rawSchema: undefined,
    transitions: to.map((t) => ({ to: t, condition: "" })),
    maxIterations: undefined,
    maxRetries: undefined,
  });

  it("carries transitions that pointed at the old name", () => {
    const steps = [draft("start", ["review"]), draft("review", ["start"])];
    const out = renameStep(steps, "id-start", "triage", "start");
    expect(out.steps[0].name).toBe("triage");
    expect(out.steps[1].transitions[0].to).toBe("triage");
    // The renamed step's own outgoing edge is untouched.
    expect(out.steps[0].transitions[0].to).toBe("review");
  });

  it("carries the workflow's start", () => {
    const steps = [draft("start"), draft("review")];
    expect(renameStep(steps, "id-start", "triage", "start").start).toBe("triage");
    // ...and leaves it alone when it named a different step.
    expect(renameStep(steps, "id-review", "audit", "start").start).toBe("start");
  });

  it("rewrites nothing while a name is half-typed", () => {
    const steps = [draft("start", []), draft("review", ["start"])];
    // Clearing the field must not repoint every transition at "".
    const cleared = renameStep(steps, "id-start", "", "start");
    expect(cleared.steps[1].transitions[0].to).toBe("start");
    expect(cleared.start).toBe("start");
    // An unchanged name is not a rename either.
    const same = renameStep(steps, "id-start", "start", "start");
    expect(same.steps[1].transitions[0].to).toBe("start");
  });

  it("leaves transitions that named something else", () => {
    const steps = [draft("start"), draft("review", ["publish"])];
    const out = renameStep(steps, "id-start", "triage", "start");
    expect(out.steps[1].transitions[0].to).toBe("publish");
  });
});
