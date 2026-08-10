import { Plus, Trash2 } from "lucide-react";
import type { AgentView } from "../../api/types";
import type { OutputField, StepDraft } from "./stepDraft";

/**
 * One step's editor, as the right panel shows it.
 *
 * Everything a step has, unfolded — the sidebar already answers "which step",
 * so nothing here is behind a disclosure.
 */
export function StepForm({
  step,
  agents,
  stepNames,
  onChange,
}: {
  step: StepDraft;
  agents: AgentView[];
  /** Every named step, for the transition targets. */
  stepNames: string[];
  onChange: (patch: Partial<StepDraft>) => void;
}) {
  const missingAgent =
    step.agent !== "" && !agents.some((a) => a.name === step.agent);
  return (
    <div className="space-y-4" data-testid="step-form" data-step-name={step.name}>
      <section className="panel space-y-3 p-4">
        <label className="block">
          <span className="section-title">Name</span>
          <input
            className="field mt-1 w-full"
            value={step.name}
            placeholder="step name"
            onChange={(e) => onChange({ name: e.target.value })}
            data-testid="step-name"
          />
        </label>

        <label className="block">
          <span className="section-title">Agent</span>
          <select
            className="field mt-1 w-full"
            value={step.agent}
            onChange={(e) => onChange({ agent: e.target.value })}
            data-testid="step-agent"
          >
            <option value="">Choose an agent…</option>
            {agents.map((a) => (
              <option key={a.name} value={a.name}>
                {a.name}
              </option>
            ))}
            {/* A step keeps naming an agent that has been deleted — nothing
              checks workflows on delete, so this is reachable and only fails
              at run time. Without an option for it the select falls back to
              rendering the first one, so the form showed "Choose an agent…"
              while still holding the dead name, and a save carried it. */}
            {missingAgent && (
              <option value={step.agent}>{step.agent} — missing</option>
            )}
          </select>
          {missingAgent && (
            <span className="mt-1 block text-xs leading-relaxed text-red-ink">
              No agent named <span className="font-mono">{step.agent}</span>{" "}
              exists any more, so this step fails when the workflow runs. Pick
              another, or recreate it.
            </span>
          )}
        </label>

        <label className="block">
          <span className="section-title">Prompt</span>
          <textarea
            className="field mt-1 min-h-32 w-full"
            value={step.prompt}
            placeholder="What this step should do. Its input is appended below it."
            onChange={(e) => onChange({ prompt: e.target.value })}
            data-testid="step-prompt"
          />
        </label>
      </section>

      <section className="panel p-4">
        <h2 className="legend">Output fields</h2>
        <p className="mt-1 text-xs text-faint">
          A step needs these before a condition can read it. Leave empty and the
          step ends with plain text.
        </p>
        {step.rawSchema !== undefined ? (
          <p className="mt-2 text-xs text-orange-ink">
            This step’s schema was written elsewhere and is kept as it is.
          </p>
        ) : (
          <div className="mt-3 space-y-2">
            {step.fields.map((f, fi) => (
              <div key={fi} className="flex items-center gap-2">
                <input
                  className="field flex-1"
                  value={f.name}
                  placeholder="severity"
                  onChange={(e) =>
                    onChange({
                      fields: step.fields.map((x, j) =>
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
                    onChange({
                      fields: step.fields.map((x, j) =>
                        j === fi
                          ? { ...x, type: e.target.value as OutputField["type"] }
                          : x,
                      ),
                    })
                  }
                  data-testid="output-field-type"
                >
                  <option value="string">string</option>
                  <option value="number">number</option>
                  <option value="boolean">boolean</option>
                </select>
                <button
                  className="key key-danger !px-2 !py-1"
                  aria-label={`Remove field ${f.name || fi + 1}`}
                  onClick={() =>
                    onChange({ fields: step.fields.filter((_, j) => j !== fi) })
                  }
                >
                  <Trash2 size={13} />
                </button>
              </div>
            ))}
            <button
              className="key !px-2 !py-1 text-xs"
              onClick={() =>
                onChange({
                  fields: [...step.fields, { name: "", type: "string", description: "" }],
                })
              }
              data-testid="add-output-field"
            >
              <Plus size={13} />
              Add field
            </button>
          </div>
        )}
      </section>

      <section className="panel p-4">
        <h2 className="legend">Goes to</h2>
        <p className="mt-1 text-xs text-faint">
          Tried in order; the first match wins. A row with no condition is the
          catch-all. No match ends the run.
        </p>
        <div className="mt-3 space-y-2">
          {step.transitions.map((t, ti) => (
            <div key={ti} className="flex items-center gap-2">
              <input
                className="field field-mono flex-1"
                value={t.condition ?? ""}
                placeholder='output.severity == "p0"  (blank = always)'
                onChange={(e) =>
                  onChange({
                    transitions: step.transitions.map((x, j) =>
                      j === ti ? { ...x, condition: e.target.value || undefined } : x,
                    ),
                  })
                }
                data-testid="transition-condition"
              />
              <select
                className="field w-40"
                value={t.to}
                onChange={(e) =>
                  onChange({
                    transitions: step.transitions.map((x, j) =>
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
                aria-label={`Remove transition ${ti + 1}`}
                onClick={() =>
                  onChange({
                    transitions: step.transitions.filter((_, j) => j !== ti),
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
              onChange({
                transitions: [...step.transitions, { to: "", condition: undefined }],
              })
            }
            data-testid="add-transition"
          >
            <Plus size={13} />
            Add transition
          </button>
        </div>
      </section>

      {/* Limits, last: they are the only fields on this panel a step usually
          leaves alone, and they were previously carried through the editor
          without being shown at all — so a budget set through the API was
          invisible here while quietly surviving a save. */}
      <section className="panel space-y-3 p-4">
        <h3 className="legend">Limits</h3>
        <div className="grid grid-cols-2 gap-3">
          <label className="block">
            <span className="section-title">Max iterations</span>
            <input
              className="field mt-1 w-full"
              type="number"
              min={1}
              value={step.maxIterations ?? ""}
              placeholder="unlimited"
              onChange={(e) =>
                onChange({ maxIterations: numberOrUndefined(e.target.value) })
              }
              data-testid="step-max-iterations"
            />
          </label>
          <label className="block">
            <span className="section-title">Retries</span>
            <input
              className="field mt-1 w-full"
              type="number"
              min={0}
              value={step.maxRetries ?? ""}
              placeholder="0"
              onChange={(e) =>
                onChange({ maxRetries: numberOrUndefined(e.target.value) })
              }
              data-testid="step-max-retries"
            />
          </label>
        </div>
        <p className="text-xs text-faint">
          How many turns this step may take before it fails, and how many times a
          transient provider error is retried within it. Leave both blank for the
          defaults.
        </p>
      </section>
    </div>
  );
}

/** An empty field means "unset", which is not the same as zero — so it has to
 * become `undefined` rather than `Number("")`, which is `0`.
 *
 * Exported for its own test: `0` is a *meaningful* value for retries, so the
 * empty case cannot be folded into a falsy check. */
export function numberOrUndefined(raw: string): number | undefined {
  const trimmed = raw.trim();
  if (trimmed === "") return undefined;
  const n = Number(trimmed);
  return Number.isFinite(n) ? n : undefined;
}
