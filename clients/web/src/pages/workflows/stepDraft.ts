import type { WorkflowStepDef, WorkflowTransition } from "../../api/types";

/**
 * One output field, as the form holds it.
 *
 * The form edits a flat field list rather than raw JSON Schema: a condition
 * reads `output.severity`, which is exactly a flat object, and a schema editor
 * would be a far larger control for a case nobody has asked for yet. Anything
 * the form cannot express is preserved untouched — see `schemaFields`.
 */
export interface OutputField {
  name: string;
  type: "string" | "number" | "boolean";
  description: string;
}

export interface StepDraft {
  /** Client-side identity, so selection and reordering survive a rename. */
  id: string;
  name: string;
  agent: string;
  prompt: string;
  fields: OutputField[];
  /** A schema the field editor could not represent, kept verbatim. */
  rawSchema: unknown;
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

/** Read a flat object schema back into fields. Returns null when the schema is
 * something this form did not write, which is the signal to leave it alone. */
export function schemaFields(schema: unknown): OutputField[] | null {
  if (!schema || typeof schema !== "object") return null;
  const s = schema as Record<string, unknown>;
  if (s.type !== "object" || typeof s.properties !== "object" || !s.properties) {
    return null;
  }
  const out: OutputField[] = [];
  for (const [name, raw] of Object.entries(s.properties as Record<string, unknown>)) {
    if (!raw || typeof raw !== "object") return null;
    const p = raw as Record<string, unknown>;
    if (p.type !== "string" && p.type !== "number" && p.type !== "boolean") return null;
    out.push({
      name,
      type: p.type,
      description: typeof p.description === "string" ? p.description : "",
    });
  }
  return out;
}

export function fieldsToSchema(fields: OutputField[]): unknown {
  const named = fields.filter((f) => f.name.trim() !== "");
  if (named.length === 0) return undefined;
  const properties: Record<string, unknown> = {};
  for (const f of named) {
    properties[f.name.trim()] = f.description.trim()
      ? { type: f.type, description: f.description.trim() }
      : { type: f.type };
  }
  return { type: "object", properties };
}

export function toDraft(step: WorkflowStepDef): StepDraft {
  const fields = schemaFields(step.outputSchema);
  return {
    id: newStepId(),
    name: step.name,
    agent: step.agent,
    prompt: step.prompt,
    fields: fields ?? [],
    rawSchema: fields === null ? step.outputSchema : undefined,
    transitions: step.transitions ?? [],
    maxIterations: step.maxIterations ?? undefined,
    maxRetries: step.maxRetries ?? undefined,
  };
}

export function fromDraft(d: StepDraft): WorkflowStepDef {
  return {
    name: d.name.trim(),
    agent: d.agent,
    prompt: d.prompt,
    outputSchema: d.rawSchema !== undefined ? d.rawSchema : fieldsToSchema(d.fields),
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
  fields: [],
  rawSchema: undefined,
  transitions: [],
  maxIterations: undefined,
  maxRetries: undefined,
});
