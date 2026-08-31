import { Plus, Trash2 } from "lucide-react";
import type { AgentView, StepFieldType, WorkflowTransition } from "../../api/types";
import {
  emptyField,
  emptyOutcome,
  filterValues,
  makeFilter,
  type StepDraft,
} from "./stepDraft";
import { Trans, useTranslation } from "react-i18next";

/**
 * One step's editor, as the right panel shows it.
 *
 * Everything a step has, unfolded — the sidebar already answers "which step",
 * so nothing here is behind a disclosure.
 */
/** Switch a transition between "always" and one of the two filters, keeping
 * whichever outcomes were already ticked. */
function setOp(step: StepDraft, ti: number, op: string): WorkflowTransition[] {
  return step.transitions.map((x, j) => {
    if (j !== ti) return x;
    if (op === "any") return { to: x.to, when: undefined };
    return { to: x.to, when: makeFilter(op as "In" | "NotIn", filterValues(x.when)) };
  });
}

function toggleValue(
  step: StepDraft,
  ti: number,
  value: string,
  on: boolean,
): WorkflowTransition[] {
  return step.transitions.map((x, j) => {
    if (j !== ti || x.when === undefined) return x;
    const values = on
      ? [...filterValues(x.when), value]
      : filterValues(x.when).filter((v) => v !== value);
    return { to: x.to, when: makeFilter(x.when.op, values) };
  });
}

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
  const { t } = useTranslation();
  const missingAgent =
    step.agent !== "" && !agents.some((a) => a.name === step.agent);
  return (
    <div className="space-y-4" data-testid="step-form" data-step-name={step.name}>
      <section className="section space-y-3">
        <label className="block">
          <span className="section-title">{t("memoryPage.name")}</span>
          <input
            className="field mt-1 w-full"
            value={step.name}
            placeholder={t("stepForm.namePlaceholder")}
            onChange={(e) => onChange({ name: e.target.value })}
            data-testid="step-name"
          />
        </label>

        <label className="block">
          <span className="section-title">{t("routines.agent")}</span>
          <select
            className="field mt-1 w-full"
            value={step.agent}
            onChange={(e) => onChange({ agent: e.target.value })}
            data-testid="step-agent"
          >
            <option value="">{t("routineEdit.chooseAgent")}</option>
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
              <Trans i18nKey="stepForm.missingAgent" values={{ agent: step.agent }} components={{ name: <span className="font-mono" /> }} />
            </span>
          )}
        </label>

        <label className="block">
          <span className="section-title">{t("routines.prompt")}</span>
          <textarea
            className="field mt-1 min-h-32 w-full"
            value={step.prompt}
            placeholder={t("stepForm.promptPlaceholder")}
            onChange={(e) => onChange({ prompt: e.target.value })}
            data-testid="step-prompt"
          />
        </label>
      </section>

      <section className="section">
        <h2 className="legend">{t("stepForm.outcomes")}</h2>
        <p className="mt-1 text-xs text-faint">
{t("stepForm.outcomesHint")}
        </p>
        <div className="mt-3 space-y-2">
          {step.outcomes.map((o, oi) => (
            <div key={oi} className="flex items-center gap-2">
              <input
                className="field field-mono w-40"
                value={o.value}
                placeholder={t("stepForm.outcomePlaceholder")}
                onChange={(e) =>
                  onChange({
                    outcomes: step.outcomes.map((x, j) =>
                      j === oi ? { ...x, value: e.target.value } : x,
                    ),
                  })
                }
                data-testid="outcome-value"
              />
              <input
                className="field flex-1"
                value={o.description}
                placeholder={t("stepForm.outcomeDescPlaceholder")}
                onChange={(e) =>
                  onChange({
                    outcomes: step.outcomes.map((x, j) =>
                      j === oi ? { ...x, description: e.target.value } : x,
                    ),
                  })
                }
                data-testid="outcome-description"
              />
              <button
                className="key key-danger key-sm"
                aria-label={t("stepForm.removeOutcome", { name: o.value || oi + 1 })}
                onClick={() =>
                  onChange({ outcomes: step.outcomes.filter((_, j) => j !== oi) })
                }
              >
                <Trash2 size={13} />
              </button>
            </div>
          ))}
          <button
            className="key key-sm"
            onClick={() => onChange({ outcomes: [...step.outcomes, emptyOutcome()] })}
            data-testid="add-outcome"
          >
            <Plus size={13} />
            {t("stepForm.addOutcome")}
          </button>
        </div>
      </section>

      <section className="section">
        <h2 className="legend">{t("stepForm.fields")}</h2>
        <div className="mt-3 space-y-2">
          {step.fields.map((f, fi) => (
            <div key={fi} className="flex items-center gap-2">
              <input
                className="field flex-1"
                value={f.name}
                placeholder={t("stepForm.fieldNamePlaceholder")}
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
                className="field w-32"
                value={f.kind}
                onChange={(e) =>
                  onChange({
                    fields: step.fields.map((x, j) =>
                      j === fi ? { ...x, kind: e.target.value as StepFieldType } : x,
                    ),
                  })
                }
                data-testid="output-field-type"
              >
                <option value="String">{t("stepForm.typeString")}</option>
                <option value="Number">{t("stepForm.typeNumber")}</option>
                <option value="Boolean">{t("stepForm.typeBoolean")}</option>
                <option value="StringList">{t("stepForm.typeStringList")}</option>
              </select>
              <input
                className="field flex-1"
                value={f.description}
                placeholder={t("stepForm.fieldDescPlaceholder")}
                onChange={(e) =>
                  onChange({
                    fields: step.fields.map((x, j) =>
                      j === fi ? { ...x, description: e.target.value } : x,
                    ),
                  })
                }
                data-testid="output-field-description"
              />
              <label className="flex items-center gap-1 text-xs text-faint">
                <input
                  type="checkbox"
                  checked={f.required ?? false}
                  onChange={(e) =>
                    onChange({
                      fields: step.fields.map((x, j) =>
                        j === fi ? { ...x, required: e.target.checked } : x,
                      ),
                    })
                  }
                  data-testid="output-field-required"
                />
                {t("stepForm.required")}
              </label>
              <button
                className="key key-danger key-sm"
                aria-label={t("stepForm.removeField", { name: f.name || fi + 1 })}
                onClick={() =>
                  onChange({ fields: step.fields.filter((_, j) => j !== fi) })
                }
              >
                <Trash2 size={13} />
              </button>
            </div>
          ))}
          <button
            className="key key-sm"
            onClick={() => onChange({ fields: [...step.fields, emptyField()] })}
            data-testid="add-output-field"
          >
            <Plus size={13} />
            {t("stepForm.addField")}
          </button>
        </div>
      </section>

      <section className="section">
        <label className="flex items-start gap-2">
          <input
            type="checkbox"
            className="mt-1"
            checked={step.interactive}
            onChange={(e) => onChange({ interactive: e.target.checked })}
            data-testid="step-interactive"
          />
          <span className="section-title">{t("stepForm.canAsk")}</span>
        </label>
      </section>

      <section className="section">
        <h2 className="legend">{t("stepForm.goesTo")}</h2>
        <p className="mt-1 text-xs text-faint">
{t("stepForm.goesToHint")}
        </p>
        <div className="mt-3 space-y-2">
          {step.transitions.map((transition, ti) => (
            <div key={ti} className="flex items-center gap-2">
              <select
                className="field w-24"
                value={transition.when?.op ?? "any"}
                onChange={(e) => onChange({ transitions: setOp(step, ti, e.target.value) })}
                data-testid="transition-op"
              >
                <option value="any">{t("stepForm.opAlways")}</option>
                <option value="In">{t("stepForm.opIn")}</option>
                <option value="NotIn">{t("stepForm.opNotIn")}</option>
              </select>
              {/* Checkboxes over the step's own outcomes, not free text: a
                  filter may only name outcomes this step reports, and offering
                  the list is what makes that unmissable rather than a save
                  error. */}
              <div
                className="flex flex-1 flex-wrap items-center gap-2"
                data-testid="transition-outcomes"
              >
                {transition.when === undefined
                  ? null
                  : step.outcomes
                      .filter((o) => o.value.trim() !== "")
                      .map((o) => (
                        <label
                          key={o.value}
                          className="flex items-center gap-1 text-xs text-faint"
                        >
                          <input
                            type="checkbox"
                            checked={filterValues(transition.when).includes(o.value)}
                            onChange={(e) =>
                              onChange({
                                transitions: toggleValue(step, ti, o.value, e.target.checked),
                              })
                            }
                          />
                          <span className="font-mono">{o.value}</span>
                        </label>
                      ))}
              </div>
              <select
                className="field w-40"
                value={transition.to}
                onChange={(e) =>
                  onChange({
                    transitions: step.transitions.map((x, j) =>
                      j === ti ? { ...x, to: e.target.value } : x,
                    ),
                  })
                }
                data-testid="transition-target"
              >
                <option value="">{t("stepForm.chooseStep")}</option>
                {stepNames.map((n) => (
                  <option key={n} value={n}>
                    {n}
                  </option>
                ))}
              </select>
              <button
                className="key key-danger key-sm"
                aria-label={t("stepForm.removeTransition", { n: ti + 1 })}
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
            className="key key-sm"
            onClick={() =>
              onChange({
                transitions: [...step.transitions, { to: "", when: undefined }],
              })
            }
            data-testid="add-transition"
          >
            <Plus size={13} />
            {t("stepForm.addTransition")}
          </button>
        </div>
      </section>

      {/* Limits, last: they are the only fields on this panel a step usually
          leaves alone, and they were previously carried through the editor
          without being shown at all — so a budget set through the API was
          invisible here while quietly surviving a save. */}
      <section className="section space-y-3">
        <h3 className="legend">{t("stepForm.limits")}</h3>
        <div className="grid grid-cols-2 gap-3">
          <label className="block">
            <span className="section-title">{t("stepForm.maxIterations")}</span>
            <input
              className="field mt-1 w-full"
              type="number"
              min={1}
              value={step.maxIterations ?? ""}
              placeholder={t("stepForm.unlimited")}
              onChange={(e) =>
                onChange({ maxIterations: numberOrUndefined(e.target.value) })
              }
              data-testid="step-max-iterations"
            />
          </label>
          <label className="block">
            <span className="section-title">{t("stepForm.retries")}</span>
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
