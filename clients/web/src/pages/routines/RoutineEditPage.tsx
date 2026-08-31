import { RowLabel } from "../settings/fields";
import { useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { ApiRequestError } from "../../api/client";
import type { RoutineInput, RoutineSchedule, RoutineView } from "../../api/types";
import { Weekday } from "../../api/types";
import { PopoverMenu } from "../../components/PopoverMenu";
import { useEnvironmentPicker } from "../../components/configPickers";
import { useAgents } from "../../hooks/useAgents";
import { useWorkflows } from "../../hooks/useWorkflows";
import { useEnvironmentChannel } from "../../hooks/useEnvironmentChannel";
import {
  useCreateRoutine,
  useRoutine,
  useUpdateRoutine,
} from "../../hooks/useRoutines";
import {
  MIN_INTERVAL_SECS,
  browserTimezone,
  fromLocalInputValue,
  timezoneOptions,
  toLocalInputValue,
} from "../../lib/schedule";
import { useTranslation } from "react-i18next";
import { localeTag } from "../../lib/format";

/** Create (`/routines/new`) and edit (`/routines/:name/edit`) share one form,
 * mounted only once the routine has loaded — its fields seed from `initial`
 * with `useState`, which cannot pick up a value that arrives later. */
export function RoutineEditPage() {
  const { t } = useTranslation();
  const { name } = useParams<{ name: string }>();
  const { data: existing, isLoading, isError } = useRoutine(name);

  if (name && isLoading) {
    return <p className="px-6 py-4 text-sm text-faint">{t("common.loading")}</p>;
  }
  if (name && (isError || !existing)) {
    return (
      <p className="px-6 py-4 text-sm text-red-ink">
        {t("routines.noSuch", { name })}
      </p>
    );
  }
  return <RoutineForm key={name ?? "new"} initial={existing} />;
}

type ScheduleKind = RoutineSchedule["type"];

/** One hour, the sanest starting cadence for a recurring routine. */
const DEFAULT_INTERVAL_SECS = 3600;

/** The canonical Mon–Sun order the server requires. */
const WEEKDAY_ORDER: Weekday[] = [
  Weekday.Mon,
  Weekday.Tue,
  Weekday.Wed,
  Weekday.Thu,
  Weekday.Fri,
  Weekday.Sat,
  Weekday.Sun,
];

/** Weekday and month names come from `Intl`, which already has all nineteen
 * of them per language \u2014 a catalogue copy would be nineteen more strings to
 * keep in step with a translation the platform already ships. */
const fullWeekdayName = (day: Weekday): string =>
  new Date(Date.UTC(2024, 0, 1 + WEEKDAY_ORDER.indexOf(day))).toLocaleDateString(
    localeTag(),
    { weekday: "long", timeZone: "UTC" },
  );

const monthNames = (): string[] =>
  Array.from({ length: 12 }, (_, i) =>
    new Date(Date.UTC(2000, i, 1)).toLocaleDateString(localeTag(), {
      month: "long",
      timeZone: "UTC",
    }),
  );

function RoutineForm({ initial }: { initial?: RoutineView }) {
  const { t } = useTranslation();
  const editing = !!initial;
  const create = useCreateRoutine();
  const update = useUpdateRoutine();
  const navigate = useNavigate();
  // Cancel and save both land back on the panel this was opened from,
  // rather than on the roster with nothing selected.
  const back = () =>
    navigate(initial ? `/routines/${encodeURIComponent(initial.name)}` : "/routines");
  const { data: agents } = useAgents();
  const { data: workflows } = useWorkflows();

  const [routineName, setRoutineName] = useState(initial?.name ?? "");
  const [description, setDescription] = useState(initial?.description ?? "");
  // Which kind of thing this routine runs, and the slug of it. Two pieces of
  // state rather than the union itself: a form is edited a field at a time, and
  // switching kind must not throw away what was typed in the other — the union
  // is assembled at save, where "neither" cannot survive the check anyway.
  const [targetKind, setTargetKind] = useState<"Agent" | "Workflow">(
    initial?.target.type ?? "Agent",
  );
  const [agent, setAgent] = useState(
    initial?.target.type === "Agent" ? initial.target.value.agent : "",
  );
  const [workflow, setWorkflow] = useState(
    initial?.target.type === "Workflow" ? initial.target.value.workflow : "",
  );
  const target = targetKind === "Agent" ? agent : workflow;
  // The same channel the new-session bar uses, rendered as a field below.
  const environment = useEnvironmentChannel(initial?.environment);
  const environmentPicker = useEnvironmentPicker(environment);
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
  type CalendarValue = {
    timezone: string;
    hour: number;
    minute: number;
    weekdays?: Weekday[];
    dayOfMonth?: number;
    month?: number;
  };
  const calendarInitial: CalendarValue | null =
    initial?.schedule.type === "Daily" ||
    initial?.schedule.type === "Weekly" ||
    initial?.schedule.type === "Monthly" ||
    initial?.schedule.type === "Yearly"
      ? initial.schedule.value
      : null;
  const [timezone, setTimezone] = useState(
    calendarInitial?.timezone ?? browserTimezone(),
  );
  const localTimezone = browserTimezone();
  const [timezoneEditorOpen, setTimezoneEditorOpen] = useState(false);
  const [timeOfDay, setTimeOfDay] = useState(
    calendarInitial
      ? `${String(calendarInitial.hour).padStart(2, "0")}:${String(calendarInitial.minute).padStart(2, "0")}`
      : "09:00",
  );
  const [weekdays, setWeekdays] = useState<Set<Weekday>>(
    new Set(initial?.schedule.type === "Weekly" ? initial.schedule.value.weekdays : []),
  );
  const [dayOfMonth, setDayOfMonth] = useState<string>(
    calendarInitial?.dayOfMonth != null ? String(calendarInitial.dayOfMonth) : "1",
  );
  const [month, setMonth] = useState<number>(calendarInitial?.month ?? 1);
  const [error, setError] = useState<string | null>(null);

  const busy = create.isPending || update.isPending;
  const hourOf = (t: string) => Number(t.split(":")[0]);
  const minuteOf = (t: string) => Number(t.split(":")[1]);
  const timeIsValid = /^\d{2}:\d{2}$/.test(timeOfDay);
  const dayValid = Number(dayOfMonth) >= 1 && Number(dayOfMonth) <= 31;
  const calendarValid =
    timezone !== "" &&
    timeIsValid &&
    (kind !== "Weekly" || weekdays.size > 0) &&
    (kind !== "Monthly" || dayValid) &&
    (kind !== "Yearly" || (month >= 1 && month <= 12 && dayValid));
  // One arm per kind, not a chain of ORs. As a chain, an `Every` schedule with
  // a too-short interval failed its own arm and then fell through to
  // `calendarValid` — which is about a time-of-day and a timezone `Every` does
  // not use, so it was true and Save stayed enabled directly beneath the
  // client's own "the shortest interval is 1 minute".
  const scheduleValid =
    kind === "Manual"
      ? true
      : kind === "Every"
        ? intervalSecs >= MIN_INTERVAL_SECS
        : kind === "Once"
          ? !Number.isNaN(fromLocalInputValue(atLocal))
          : calendarValid;
  const canSave =
    !busy &&
    routineName.trim() !== "" &&
    target !== "" &&
    environment.chosen &&
    prompt.trim() !== "" &&
    scheduleValid;

  const buildSchedule = (): RoutineSchedule => {
    switch (kind) {
      case "Every":
        return { type: "Every", value: { intervalSecs } };
      case "Once":
        return { type: "Once", value: { atMs: fromLocalInputValue(atLocal) } };
      case "Daily":
        return {
          type: "Daily",
          value: { timezone, hour: hourOf(timeOfDay), minute: minuteOf(timeOfDay) },
        };
      case "Weekly":
        return {
          type: "Weekly",
          value: {
            timezone,
            hour: hourOf(timeOfDay),
            minute: minuteOf(timeOfDay),
            weekdays: WEEKDAY_ORDER.filter((d) => weekdays.has(d)),
          },
        };
      case "Monthly":
        return {
          type: "Monthly",
          value: {
            timezone,
            hour: hourOf(timeOfDay),
            minute: minuteOf(timeOfDay),
            dayOfMonth: Number(dayOfMonth),
          },
        };
      case "Yearly":
        return {
          type: "Yearly",
          value: {
            timezone,
            hour: hourOf(timeOfDay),
            minute: minuteOf(timeOfDay),
            month,
            dayOfMonth: Number(dayOfMonth),
          },
        };
      case "Manual":
        return { type: "Manual", value: {} };
    }
  };

  const handleSave = async () => {
    setError(null);
    const body: RoutineInput = {
      name: routineName.trim(),
      description: description.trim() || undefined,
      target:
        targetKind === "Agent"
          ? { type: "Agent", value: { agent } }
          : { type: "Workflow", value: { workflow } },
      environment: environment.spec,
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
        e instanceof ApiRequestError ? e.message : t("routineEdit.saveFailed"),
      );
    }
  };

  return (
    <div className="flex h-full flex-col" data-testid="routine-edit-page">
      <div className="flex h-[var(--header-h)] shrink-0 items-center gap-2 bar-scroll px-6">
        <h1 className="page-title min-w-0 flex-1 truncate">
          {editing
            ? t("agentEdit.editTitle", { name: initial.name })
            : t("routines.new")}
        </h1>
        <button
          className="key key-blank key-sm"
          onClick={back}
          data-testid="cancel-routine-button"
        >
          {t("common.cancel")}
        </button>
        <button
          className="key key-go key-sm"
          disabled={!canSave}
          onClick={handleSave}
          data-testid="save-routine-button"
        >
          {busy ? t("common.saving") : t("common.save")}
        </button>
      </div>
      <div className="flex-1 overflow-y-auto px-6 py-4">
        <div className="w-full space-y-4">
          <label className="block">
<RowLabel>{t("memoryPage.name")}</RowLabel>
            <input
              className="field w-full font-mono"
              placeholder={t("routineEdit.namePlaceholder")}
              value={routineName}
              disabled={editing}
              onChange={(e) => setRoutineName(e.target.value)}
              data-testid="routine-name-input"
            />
          </label>

          <label className="block">
<RowLabel>{t("memoryPage.description")}</RowLabel>
            <input
              className="field w-full"
              placeholder={t("routineEdit.descriptionPlaceholder")}
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              data-testid="routine-description-input"
            />
          </label>

          <div>
            <RowLabel>{t("routineEdit.runs")}</RowLabel>
            {/* Which kind first, then which one. A single list of everything
                would put two namespaces in one menu, where "release" could be
                either and the reader has no way to tell. */}
            <div
              className="segmented mb-2"
              role="radiogroup"
              aria-label={t("routineEdit.runs")}
            >
              {(["Agent", "Workflow"] as const).map((k) => (
                <button
                  key={k}
                  type="button"
                  role="radio"
                  aria-checked={targetKind === k}
                  data-testid={`routine-target-${k.toLowerCase()}`}
                  onClick={() => setTargetKind(k)}
                >
                  {t(k === "Agent" ? "routines.agent" : "channel.workflow")}
                </button>
              ))}
            </div>
            {targetKind === "Agent" ? (
              <label className="block">
                <select
                  className="field w-full"
                  value={agent}
                  onChange={(e) => setAgent(e.target.value)}
                  data-testid="routine-agent-select"
                >
                  <option value="">{t("routineEdit.chooseAgent")}</option>
                  {(agents ?? []).map((a) => (
                    <option key={a.name} value={a.name}>
                      {a.name} · {a.model}
                    </option>
                  ))}
                </select>
              </label>
            ) : (
              <label className="block">
                <select
                  className="field w-full"
                  value={workflow}
                  onChange={(e) => setWorkflow(e.target.value)}
                  data-testid="routine-workflow-select"
                >
                  <option value="">{t("routineEdit.chooseWorkflow")}</option>
                  {(workflows ?? []).map((w) => (
                    <option key={w.name} value={w.name}>
                      {w.name} · {t("newSession.stepCount", { count: w.steps.length })}
                    </option>
                  ))}
                </select>
              </label>
            )}
          </div>

          <div>
            <PopoverMenu
              variant="field"
              placement="down"
              testId={environmentPicker.testId}
              legend={environmentPicker.legend}
              icon={environmentPicker.icon}
              label={environmentPicker.label}
              width={environmentPicker.width}
            >
              {environmentPicker.body}
            </PopoverMenu>
          </div>

          <label className="block">
<RowLabel>{t("routines.prompt")}</RowLabel>
            <textarea
              className="field h-40 w-full resize-y font-mono text-sm"
              placeholder={t("routineEdit.promptPlaceholder")}
              value={prompt}
              onChange={(e) => setPrompt(e.target.value)}
              data-testid="routine-prompt-input"
            />
          </label>

          <fieldset className="space-y-2">
            <legend className="section-title mb-1.5">{t("routineEdit.trigger")}</legend>
            <div className="flex flex-wrap items-center gap-3">
              <select
                className="field"
                value={kind}
                onChange={(e) => setKind(e.target.value as ScheduleKind)}
                data-testid="routine-schedule-kind"
              >
                <option value="Manual">{t("routineEdit.kindManual")}</option>
                <option value="Every">{t("routineEdit.kindEvery")}</option>
                <option value="Once">{t("routineEdit.kindOnce")}</option>
                <option value="Daily">{t("routineEdit.kindDaily")}</option>
                <option value="Weekly">{t("routineEdit.kindWeekly")}</option>
                <option value="Monthly">{t("routineEdit.kindMonthly")}</option>
                <option value="Yearly">{t("routineEdit.kindYearly")}</option>
              </select>

              {kind === "Every" && (
                <label className="flex items-center gap-2 text-sm text-dim">
                  {t("routineEdit.everyLabel")}
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
                  {t("routineEdit.minutes")}
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

              {["Daily", "Weekly", "Monthly", "Yearly"].includes(kind) && (
                <>
                  <label className="flex items-center gap-2 text-sm text-dim">
                    {t("routineEdit.atLabel")}
                    <input
                      className="field"
                      type="time"
                      value={timeOfDay}
                      onChange={(e) => setTimeOfDay(e.target.value)}
                      data-testid="routine-time-input"
                    />
                  </label>
                  <div className="flex flex-wrap items-center gap-2 text-xs text-dim">
                    <span>
                      {timezone === localTimezone
                        ? t("routineEdit.browserTimezone")
                        : t("routineEdit.customTimezone")}
                      <span className="ml-1 font-mono text-faint">
                        · {timezone}
                      </span>
                    </span>
                    <button
                      type="button"
                      className="key key-flat"
                      aria-controls="routine-timezone-editor"
                      aria-expanded={timezoneEditorOpen}
                      data-testid="routine-timezone-toggle"
                      onClick={() =>
                        setTimezoneEditorOpen((open) => !open)
                      }
                    >
                      {timezoneEditorOpen ? t("routineEdit.done") : t("routineEdit.change")}
                    </button>
                  </div>
                  {timezoneEditorOpen && (
                    <div
                      id="routine-timezone-editor"
                      className="mt-2 w-full min-w-0"
                    >
                      <label className="flex min-w-0 flex-col items-start gap-1 text-xs text-dim">
                        <span>{t("routineEdit.timezone")}</span>
                        <select
                          className="field field-mono min-w-0 w-full"
                          value={timezone}
                          onChange={(e) => setTimezone(e.target.value)}
                          data-testid="routine-timezone-select"
                        >
                          {timezoneOptions().map((z) => (
                            <option key={z} value={z}>
                              {z}
                            </option>
                          ))}
                        </select>
                      </label>
                    </div>
                  )}

                  {kind === "Weekly" && (
                    <div className="flex flex-wrap items-center gap-1.5">
                      <div
                        className="flex flex-wrap items-center gap-1.5"
                        role="group"
                        aria-label={t("routineEdit.daysOfWeek")}
                        data-testid="routine-weekdays"
                      >
                        {WEEKDAY_ORDER.map((d) => (
                          <button
                            key={d}
                            type="button"
                            className="chip chip-toggle min-h-10 min-w-10 justify-center"
                            aria-label={fullWeekdayName(d)}
                            aria-pressed={weekdays.has(d)}
                            onClick={() =>
                              setWeekdays((prev) => {
                                const next = new Set(prev);
                                if (next.has(d)) next.delete(d);
                                else next.add(d);
                                return next;
                              })
                            }
                            data-testid={`weekday-${d.toLowerCase()}`}
                          >
                            {d}
                          </button>
                        ))}
                      </div>
                      <button
                        type="button"
                        className="key key-flat"
                        onClick={() =>
                          setWeekdays(new Set(WEEKDAY_ORDER.slice(0, 5)))
                        }
                        data-testid="routine-weekdays-weekdays"
                      >
                        {t("routineEdit.weekdays")}
                      </button>
                    </div>
                  )}

                  {kind === "Monthly" && (
                    <label className="flex items-center gap-2 text-sm text-dim">
                      {t("routineEdit.onTheLabel")}
                      <input
                        className="field w-20"
                        type="number"
                        min={1}
                        max={31}
                        value={dayOfMonth}
                        onChange={(e) => setDayOfMonth(e.target.value)}
                        data-testid="routine-day-of-month"
                      />
                      {t("routineEdit.dayLabel")}
                    </label>
                  )}

                  {kind === "Yearly" && (
                    <label className="flex items-center gap-2 text-sm text-dim">
                      {t("routineEdit.onLabel")}
                      <select
                        className="field"
                        value={month}
                        onChange={(e) => setMonth(Number(e.target.value))}
                        data-testid="routine-month"
                      >
                        {monthNames().map((m, i) => (
                          <option key={m} value={i + 1}>
                            {m}
                          </option>
                        ))}
                      </select>
                      <input
                        className="field w-20"
                        type="number"
                        min={1}
                        max={31}
                        value={dayOfMonth}
                        onChange={(e) => setDayOfMonth(e.target.value)}
                        data-testid="routine-day-of-month"
                      />
                    </label>
                  )}

                  {kind === "Weekly" && weekdays.size === 0 && (
                    <p className="text-xs text-red-ink">{t("routineEdit.pickADay")}</p>
                  )}
                </>
              )}
            </div>
            {kind === "Every" && intervalSecs < MIN_INTERVAL_SECS && (
              <p className="text-xs text-red-ink">
                {t("routineEdit.shortestInterval", {
                  count: MIN_INTERVAL_SECS / 60,
                })}
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
                {t("routineEdit.timerActive")}
              </label>
            )}
            <p className="text-[0.6875rem] text-faint">
{t("routineEdit.timerHint")}
            </p>
          </fieldset>

          {error && (
            <div
              className="notice notice-fault"
              data-testid="routine-error"
            >
              {error}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
