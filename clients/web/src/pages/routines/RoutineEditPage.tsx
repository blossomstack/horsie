import { useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { ApiRequestError } from "../../api/client";
import type { RoutineInput, RoutineSchedule, RoutineView } from "../../api/types";
import { useAgents } from "../../hooks/useAgents";
import {
  useCreateRoutine,
  useRoutine,
  useUpdateRoutine,
} from "../../hooks/useRoutines";
import {
  MIN_INTERVAL_SECS,
  fromLocalInputValue,
  toLocalInputValue,
} from "../../lib/schedule";

/** Create (`/routines/new`) and edit (`/routines/:name/edit`) share one form,
 * mounted only once the routine has loaded — its fields seed from `initial`
 * with `useState`, which cannot pick up a value that arrives later. */
export function RoutineEditPage() {
  const { name } = useParams<{ name: string }>();
  const { data: existing, isLoading, isError } = useRoutine(name);

  if (name && isLoading) {
    return <p className="px-6 py-4 text-sm text-faint">Loading…</p>;
  }
  if (name && (isError || !existing)) {
    return (
      <p className="px-6 py-4 text-sm text-red-ink">No such routine: {name}.</p>
    );
  }
  return <RoutineForm key={name ?? "new"} initial={existing} />;
}

type ScheduleKind = RoutineSchedule["type"];

/** One hour, the sanest starting cadence for a recurring routine. */
const DEFAULT_INTERVAL_SECS = 3600;

function RoutineForm({ initial }: { initial?: RoutineView }) {
  const editing = !!initial;
  const create = useCreateRoutine();
  const update = useUpdateRoutine();
  const navigate = useNavigate();
  const { data: agents } = useAgents();

  const [routineName, setRoutineName] = useState(initial?.name ?? "");
  const [description, setDescription] = useState(initial?.description ?? "");
  const [agent, setAgent] = useState(initial?.agent ?? "");
  const [prompt, setPrompt] = useState(initial?.prompt ?? "");
  const [enabled, setEnabled] = useState(initial?.enabled ?? true);
  const [kind, setKind] = useState<ScheduleKind>(
    initial?.schedule.type ?? "Manual",
  );
  const [intervalSecs, setIntervalSecs] = useState(
    initial?.schedule.type === "Every"
      ? initial.schedule.value.intervalSecs
      : DEFAULT_INTERVAL_SECS,
  );
  const [atLocal, setAtLocal] = useState(
    initial?.schedule.type === "Once"
      ? toLocalInputValue(initial.schedule.value.atMs)
      : "",
  );
  const [error, setError] = useState<string | null>(null);

  const busy = create.isPending || update.isPending;
  const scheduleValid =
    kind === "Manual" ||
    (kind === "Every" && intervalSecs >= MIN_INTERVAL_SECS) ||
    (kind === "Once" && !Number.isNaN(fromLocalInputValue(atLocal)));
  const canSave =
    !busy &&
    routineName.trim() !== "" &&
    agent !== "" &&
    prompt.trim() !== "" &&
    scheduleValid;

  const buildSchedule = (): RoutineSchedule => {
    switch (kind) {
      case "Every":
        return { type: "Every", value: { intervalSecs } };
      case "Once":
        return { type: "Once", value: { atMs: fromLocalInputValue(atLocal) } };
      case "Manual":
        return { type: "Manual", value: {} };
    }
  };

  const handleSave = async () => {
    setError(null);
    const body: RoutineInput = {
      name: routineName.trim(),
      description: description.trim() || undefined,
      agent,
      prompt: prompt.trim(),
      schedule: buildSchedule(),
      enabled,
    };
    try {
      if (editing) await update.mutateAsync({ name: body.name, body });
      else await create.mutateAsync(body);
      navigate(`/routines/${encodeURIComponent(body.name)}`);
    } catch (e) {
      setError(
        e instanceof ApiRequestError ? e.message : "Failed to save routine.",
      );
    }
  };

  return (
    <div className="flex h-full flex-col" data-testid="routine-edit-page">
      <div className="border-b px-6 py-4">
        <h1 className="page-title">
          {editing ? `Edit ${initial.name}` : "New routine"}
        </h1>
      </div>
      <div className="flex-1 overflow-y-auto px-6 py-4">
        <div className="mx-auto w-full max-w-3xl space-y-4">
          <label className="block">
            <span className="mb-1 block text-xs font-medium text-dim">
              Name
            </span>
            <input
              className="field w-full font-mono"
              placeholder="nightly-triage"
              value={routineName}
              disabled={editing}
              onChange={(e) => setRoutineName(e.target.value)}
              data-testid="routine-name-input"
            />
          </label>

          <label className="block">
            <span className="mb-1 block text-xs font-medium text-dim">
              Description
            </span>
            <input
              className="field w-full"
              placeholder="What this routine is for"
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              data-testid="routine-description-input"
            />
          </label>

          <label className="block">
            <span className="mb-1 block text-xs font-medium text-dim">
              Agent
            </span>
            <select
              className="field w-full"
              value={agent}
              onChange={(e) => setAgent(e.target.value)}
              data-testid="routine-agent-select"
            >
              <option value="">Choose an agent…</option>
              {(agents ?? []).map((a) => (
                <option key={a.name} value={a.name}>
                  {a.name} · {a.model}
                </option>
              ))}
            </select>
            <span className="mt-1 block text-[11px] text-faint">
              The routine runs with this agent’s runtime, model, repos, skills
              and memory. Edit those on the Agents page.
            </span>
          </label>

          <label className="block">
            <span className="mb-1 block text-xs font-medium text-dim">
              Prompt
            </span>
            <textarea
              className="field h-40 w-full resize-y font-mono text-sm"
              placeholder="Everything the run gets told. It cannot ask you a question, so say what to do when a choice comes up."
              value={prompt}
              onChange={(e) => setPrompt(e.target.value)}
              data-testid="routine-prompt-input"
            />
          </label>

          <fieldset className="space-y-2">
            <legend className="mb-1 text-xs font-medium text-dim">
              Trigger
            </legend>
            <div className="flex flex-wrap items-center gap-3">
              <select
                className="field"
                value={kind}
                onChange={(e) => setKind(e.target.value as ScheduleKind)}
                data-testid="routine-schedule-kind"
              >
                <option value="Manual">Only when I run it</option>
                <option value="Every">Repeatedly</option>
                <option value="Once">Once, at a time</option>
              </select>

              {kind === "Every" && (
                <label className="flex items-center gap-2 text-sm text-dim">
                  every
                  <input
                    className="field w-24"
                    type="number"
                    min={MIN_INTERVAL_SECS / 60}
                    step={1}
                    value={Math.round(intervalSecs / 60)}
                    onChange={(e) =>
                      setIntervalSecs(Math.round(Number(e.target.value) * 60))
                    }
                    data-testid="routine-interval-minutes"
                  />
                  minutes
                </label>
              )}

              {kind === "Once" && (
                <input
                  className="field"
                  type="datetime-local"
                  value={atLocal}
                  onChange={(e) => setAtLocal(e.target.value)}
                  data-testid="routine-at-input"
                />
              )}
            </div>
            {kind === "Every" && intervalSecs < MIN_INTERVAL_SECS && (
              <p className="text-xs text-red-ink">
                The shortest interval is {MIN_INTERVAL_SECS / 60} minute.
              </p>
            )}
            {kind !== "Manual" && (
              <label className="flex items-center gap-2 text-sm text-dim">
                <input
                  type="checkbox"
                  checked={enabled}
                  onChange={(e) => setEnabled(e.target.checked)}
                  data-testid="routine-enabled-toggle"
                />
                Timer active
              </label>
            )}
            <p className="text-[11px] text-faint">
              The run button and the API work either way — pausing only stops
              the timer. Runs are not prevented from overlapping, so leave the
              interval room to finish.
            </p>
          </fieldset>

          {error && (
            <div
              className="rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2 text-sm text-red-ink"
              data-testid="routine-error"
            >
              {error}
            </div>
          )}
        </div>
      </div>
      <div className="mx-auto flex w-full max-w-3xl gap-2 px-4 pb-4">
        <button
          className="key key-go"
          disabled={!canSave}
          onClick={handleSave}
          data-testid="save-routine-button"
        >
          {busy ? "Saving…" : "Save routine"}
        </button>
        <button className="key" onClick={() => navigate("/routines")}>
          Cancel
        </button>
      </div>
    </div>
  );
}
