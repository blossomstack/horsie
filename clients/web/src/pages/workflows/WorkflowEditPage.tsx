import { ChevronDown, ChevronRight, Plus, Trash2 } from "lucide-react";
import { useMemo, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import type { WorkflowStepDef, WorkflowTransition } from "../../api/types";
import { WorkflowGraph } from "../../components/WorkflowGraph";
import { useAgents } from "../../hooks/useAgents";
import {
  useCreateWorkflow,
  useUpdateWorkflow,
  useWorkflow,
} from "../../hooks/useWorkflows";

/**
 * One output field, as the form holds it.
 *
 * The form edits a flat field list rather than raw JSON Schema: a condition
 * reads `output.severity`, which is exactly a flat object, and a schema editor
 * would be a far larger control for a case nobody has asked for yet. Anything
 * the form cannot express is preserved untouched — see `schemaFields`.
 */
interface OutputField {
  name: string;
  type: "string" | "number" | "boolean";
  description: string;
}

/** Read a flat object schema back into fields. Returns null when the schema is
 * something this form did not write, which is the signal to leave it alone. */
function schemaFields(schema: unknown): OutputField[] | null {
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

function fieldsToSchema(fields: OutputField[]): unknown {
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

interface StepDraft {
  name: string;
  agent: string;
  prompt: string;
  fields: OutputField[];
  /** A schema the field editor could not represent, kept verbatim. */
  rawSchema: unknown;
  transitions: WorkflowTransition[];
}

function toDraft(step: WorkflowStepDef): StepDraft {
  const fields = schemaFields(step.outputSchema);
  return {
    name: step.name,
    agent: step.agent,
    prompt: step.prompt,
    fields: fields ?? [],
    rawSchema: fields === null ? step.outputSchema : undefined,
    transitions: step.transitions ?? [],
  };
}

function fromDraft(d: StepDraft): WorkflowStepDef {
  return {
    name: d.name.trim(),
    agent: d.agent,
    prompt: d.prompt,
    outputSchema: d.rawSchema !== undefined ? d.rawSchema : fieldsToSchema(d.fields),
    transitions: d.transitions.length > 0 ? d.transitions : undefined,
    maxIterations: undefined,
    maxRetries: undefined,
  };
}

const emptyStep = (n: number): StepDraft => ({
  name: n === 0 ? "start" : `step-${n + 1}`,
  agent: "",
  prompt: "",
  fields: [],
  rawSchema: undefined,
  transitions: [],
});

export function WorkflowEditPage() {
  const { name } = useParams<{ name: string }>();
  const editing = !!name;
  const navigate = useNavigate();
  const { data: existing } = useWorkflow(name);
  const { data: agents } = useAgents();
  const create = useCreateWorkflow();
  const update = useUpdateWorkflow();

  const [slug, setSlug] = useState("");
  const [description, setDescription] = useState("");
  const [steps, setSteps] = useState<StepDraft[]>([emptyStep(0)]);
  const [start, setStart] = useState("start");
  const [open, setOpen] = useState<Set<number>>(new Set([0]));
  const [error, setError] = useState<string | null>(null);
  const [loaded, setLoaded] = useState(false);

  // Seed once, when the definition arrives.
  if (editing && existing && !loaded) {
    setSlug(existing.name);
    setDescription(existing.description);
    setSteps(existing.steps.map(toDraft));
    setStart(existing.start);
    setLoaded(true);
  }

  const graph = useMemo(() => {
    const nodes = steps
      .filter((s) => s.name.trim() !== "")
      .map((s) => ({ step: s.name.trim(), detail: s.agent || undefined }));
    const edges = steps.flatMap((s) =>
      s.transitions
        .filter((t) => t.to.trim() !== "")
        .map((t) => ({
          from: s.name.trim(),
          to: t.to.trim(),
          condition: t.condition,
        })),
    );
    return { nodes, edges };
  }, [steps]);

  const setStep = (i: number, patch: Partial<StepDraft>) =>
    setSteps((prev) => prev.map((s, j) => (j === i ? { ...s, ...patch } : s)));

  const save = () => {
    setError(null);
    const body = {
      name: slug.trim(),
      description: description.trim() || undefined,
      start: start.trim(),
      steps: steps.map(fromDraft),
    };
    const done = () => navigate(`/workflows/${encodeURIComponent(body.name)}`);
    const fail = (e: unknown) => setError(e instanceof Error ? e.message : String(e));
    if (editing) {
      update.mutate({ name: name as string, body }, { onSuccess: done, onError: fail });
    } else {
      create.mutate(body, { onSuccess: done, onError: fail });
    }
  };

  const stepNames = steps.map((s) => s.name.trim()).filter(Boolean);

  return (
    <div className="flex h-full flex-col" data-testid="workflow-edit-page">
      <div className="flex items-center gap-3 border-b px-6 py-4">
        <h1 className="page-title">{editing ? `Edit ${name}` : "New workflow"}</h1>
        <button
          className="key key-go ml-auto !px-2.5 !py-1.5 text-xs"
          onClick={save}
          data-testid="save-workflow"
          disabled={!slug.trim() || stepNames.length === 0}
        >
          Save
        </button>
      </div>

      <div className="flex flex-1 gap-6 overflow-hidden px-6 py-4">
        <div className="flex-1 space-y-4 overflow-y-auto pr-1">
          {error && (
            <p className="rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2 text-sm text-red-ink">
              {error}
            </p>
          )}

          <section className="panel space-y-3 p-4">
            <h2 className="legend">Definition</h2>
            <label className="block">
              <span className="section-title">Name</span>
              <input
                className="field mt-1 w-full"
                value={slug}
                disabled={editing}
                placeholder="fix-bug"
                onChange={(e) => setSlug(e.target.value)}
                data-testid="workflow-name"
              />
            </label>
            <label className="block">
              <span className="section-title">Description</span>
              <input
                className="field mt-1 w-full"
                value={description}
                onChange={(e) => setDescription(e.target.value)}
              />
            </label>
            <label className="block">
              <span className="section-title">Starts at</span>
              <select
                className="field mt-1 w-full"
                value={start}
                onChange={(e) => setStart(e.target.value)}
                data-testid="workflow-start"
              >
                {stepNames.map((n) => (
                  <option key={n} value={n}>
                    {n}
                  </option>
                ))}
              </select>
            </label>
          </section>

          <section className="space-y-2">
            <div className="flex items-center gap-2">
              <h2 className="legend">Steps</h2>
              <button
                className="key ml-auto !px-2 !py-1 text-xs"
                onClick={() => {
                  setSteps((p) => [...p, emptyStep(p.length)]);
                  setOpen((p) => new Set(p).add(steps.length));
                }}
                data-testid="add-step"
              >
                <Plus size={14} />
                Add step
              </button>
            </div>

            {steps.map((s, i) => {
              const expanded = open.has(i);
              return (
                <div
                  key={i}
                  className="panel p-3"
                  data-testid="step-card"
                  data-step-name={s.name}
                >
                  <div className="flex items-center gap-2">
                    <button
                      className="key key-flat !px-1.5 !py-1"
                      aria-label={expanded ? "Collapse step" : "Expand step"}
                      onClick={() =>
                        setOpen((p) => {
                          const next = new Set(p);
                          if (next.has(i)) next.delete(i);
                          else next.add(i);
                          return next;
                        })
                      }
                    >
                      {expanded ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
                    </button>
                    <input
                      className="field flex-1"
                      value={s.name}
                      placeholder="step name"
                      onChange={(e) => setStep(i, { name: e.target.value })}
                      data-testid="step-name"
                    />
                    <button
                      className="key key-danger !px-2 !py-1"
                      aria-label={`Remove step ${s.name}`}
                      onClick={() => setSteps((p) => p.filter((_, j) => j !== i))}
                      data-testid="remove-step"
                    >
                      <Trash2 size={14} />
                    </button>
                  </div>

                  {expanded && (
                    <div className="mt-3 space-y-3">
                      <label className="block">
                        <span className="section-title">Agent</span>
                        <select
                          className="field mt-1 w-full"
                          value={s.agent}
                          onChange={(e) => setStep(i, { agent: e.target.value })}
                          data-testid="step-agent"
                        >
                          <option value="">Choose an agent…</option>
                          {(agents ?? []).map((a) => (
                            <option key={a.name} value={a.name}>
                              {a.name}
                            </option>
                          ))}
                        </select>
                      </label>

                      <label className="block">
                        <span className="section-title">Prompt</span>
                        <textarea
                          className="field mt-1 min-h-20 w-full"
                          value={s.prompt}
                          placeholder="What this step should do. Its input is appended below it."
                          onChange={(e) => setStep(i, { prompt: e.target.value })}
                          data-testid="step-prompt"
                        />
                      </label>

                      <div>
                        <span className="section-title">Output fields</span>
                        <p className="mt-1 text-xs text-faint">
                          A step needs these before a condition can read it. Leave
                          empty and the step ends with plain text.
                        </p>
                        {s.rawSchema !== undefined ? (
                          <p className="mt-2 text-xs text-orange-ink">
                            This step’s schema was written elsewhere and is kept as
                            it is.
                          </p>
                        ) : (
                          <div className="mt-2 space-y-2">
                            {s.fields.map((f, fi) => (
                              <div key={fi} className="flex items-center gap-2">
                                <input
                                  className="field flex-1"
                                  value={f.name}
                                  placeholder="severity"
                                  onChange={(e) =>
                                    setStep(i, {
                                      fields: s.fields.map((x, j) =>
                                        j === fi ? { ...x, name: e.target.value } : x,
                                      ),
                                    })
                                  }
                                  data-testid="output-field-name"
                                />
                                <select
                                  className="field w-28"
                                  value={f.type}
                                  onChange={(e) =>
                                    setStep(i, {
                                      fields: s.fields.map((x, j) =>
                                        j === fi
                                          ? {
                                              ...x,
                                              type: e.target
                                                .value as OutputField["type"],
                                            }
                                          : x,
                                      ),
                                    })
                                  }
                                >
                                  <option value="string">string</option>
                                  <option value="number">number</option>
                                  <option value="boolean">boolean</option>
                                </select>
                                <button
                                  className="key key-danger !px-2 !py-1"
                                  aria-label="Remove field"
                                  onClick={() =>
                                    setStep(i, {
                                      fields: s.fields.filter((_, j) => j !== fi),
                                    })
                                  }
                                >
                                  <Trash2 size={13} />
                                </button>
                              </div>
                            ))}
                            <button
                              className="key !px-2 !py-1 text-xs"
                              onClick={() =>
                                setStep(i, {
                                  fields: [
                                    ...s.fields,
                                    { name: "", type: "string", description: "" },
                                  ],
                                })
                              }
                              data-testid="add-output-field"
                            >
                              <Plus size={13} />
                              Add field
                            </button>
                          </div>
                        )}
                      </div>

                      <div>
                        <span className="section-title">Goes to</span>
                        <p className="mt-1 text-xs text-faint">
                          Tried in order; the first match wins. A row with no
                          condition is the catch-all. No match ends the run.
                        </p>
                        <div className="mt-2 space-y-2">
                          {s.transitions.map((t, ti) => (
                            <div key={ti} className="flex items-center gap-2">
                              <input
                                className="field field-mono flex-1"
                                value={t.condition ?? ""}
                                placeholder='output.severity == "p0"  (blank = always)'
                                onChange={(e) =>
                                  setStep(i, {
                                    transitions: s.transitions.map((x, j) =>
                                      j === ti
                                        ? {
                                            ...x,
                                            condition: e.target.value || undefined,
                                          }
                                        : x,
                                    ),
                                  })
                                }
                                data-testid="transition-condition"
                              />
                              <select
                                className="field w-40"
                                value={t.to}
                                onChange={(e) =>
                                  setStep(i, {
                                    transitions: s.transitions.map((x, j) =>
                                      j === ti ? { ...x, to: e.target.value } : x,
                                    ),
                                  })
                                }
                                data-testid="transition-target"
                              >
                                <option value="">Choose a step…</option>
                                {stepNames.map((n) => (
                                  <option key={n} value={n}>
                                    {n}
                                  </option>
                                ))}
                              </select>
                              <button
                                className="key key-danger !px-2 !py-1"
                                aria-label="Remove transition"
                                onClick={() =>
                                  setStep(i, {
                                    transitions: s.transitions.filter(
                                      (_, j) => j !== ti,
                                    ),
                                  })
                                }
                              >
                                <Trash2 size={13} />
                              </button>
                            </div>
                          ))}
                          <button
                            className="key !px-2 !py-1 text-xs"
                            onClick={() =>
                              setStep(i, {
                                transitions: [
                                  ...s.transitions,
                                  { to: "", condition: undefined },
                                ],
                              })
                            }
                            data-testid="add-transition"
                          >
                            <Plus size={13} />
                            Add transition
                          </button>
                        </div>
                      </div>
                    </div>
                  )}
                </div>
              );
            })}
          </section>
        </div>

        <div className="w-[26rem] shrink-0 overflow-auto">
          <div className="panel p-4">
            <h2 className="legend">Graph</h2>
            <div className="mt-3">
              <WorkflowGraph
                nodes={graph.nodes}
                edges={graph.edges}
                start={start}
              />
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
