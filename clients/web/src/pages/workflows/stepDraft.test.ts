import { describe, expect, it } from "vitest";
import type { WorkflowStepDef } from "../../api/types";
import { fromDraft, schemaFields, toDraft } from "./stepDraft";

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
