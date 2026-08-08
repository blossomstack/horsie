import { GitBranch, GripVertical, Plus, Sliders, Trash2 } from "lucide-react";
import { useMemo, useState, type ReactNode } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { WorkflowGraph } from "../../components/WorkflowGraph";
import { useAgents } from "../../hooks/useAgents";
import {
  useCreateWorkflow,
  useUpdateWorkflow,
  useWorkflow,
} from "../../hooks/useWorkflows";
import { cn } from "../../lib/cn";
import { StepForm } from "./StepForm";
import { emptyStep, fromDraft, toDraft, type StepDraft } from "./stepDraft";
import {
  afterRemoval,
  DEFINITION,
  isSelected,
  moveItem,
  type Selection,
} from "./stepList";

/**
 * The workflow editor: a sidebar of what the definition holds, and one panel
 * showing whichever piece is selected.
 *
 * The steps used to be a column of accordions with the graph pinned beside
 * them, which made a five-step workflow a page you scrolled to read and left
 * the graph too narrow to follow. Here the list is always in view and never
 * grows, and the graph takes the whole panel when asked for — where clicking a
 * node can select that step, which a preview pinned in a gutter could not do.
 */
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
  // Held as text, not a number: an empty field has to mean "use the server's
  // default", and a number input cannot express that without a sentinel.
  const [maxSteps, setMaxSteps] = useState("");
  const [steps, setSteps] = useState<StepDraft[]>([emptyStep(0)]);
  const [start, setStart] = useState("start");
  const [selected, setSelected] = useState<Selection>(DEFINITION);
  const [visualizing, setVisualizing] = useState(false);
  const [dragging, setDragging] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loaded, setLoaded] = useState(false);

  // Seed once, when the definition arrives.
  if (editing && existing && !loaded) {
    setSlug(existing.name);
    setDescription(existing.description);
    setMaxSteps(existing.maxSteps === undefined ? "" : String(existing.maxSteps));
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

  const setStep = (id: string, patch: Partial<StepDraft>) =>
    setSteps((prev) => prev.map((s) => (s.id === id ? { ...s, ...patch } : s)));

  const openStep = (id: string) => {
    setSelected({ kind: "step", id });
    setVisualizing(false);
  };

  const addStep = () => {
    const step = emptyStep(steps.length);
    setSteps((p) => [...p, step]);
    openStep(step.id);
  };

  const removeStep = (index: number) => {
    setSelected(afterRemoval(steps.map((s) => s.id), index, selected));
    setSteps((p) => p.filter((_, j) => j !== index));
  };

  const move = (from: number, to: number) => setSteps((p) => moveItem(p, from, to));

  const save = () => {
    setError(null);
    const body = {
      name: slug.trim(),
      description: description.trim() || undefined,
      start: start.trim(),
      steps: steps.map(fromDraft),
      maxSteps: maxSteps.trim() === "" ? undefined : Number(maxSteps),
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
  const current =
    selected.kind === "step" ? steps.find((s) => s.id === selected.id) : undefined;

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

      <div className="flex min-h-0 flex-1 flex-col md:flex-row">
        {/* A column beside the panel on a desk, a scrolling strip of keys above
            it on a phone — the same move the settings nav makes, because a
            15rem column takes two thirds of a 390px viewport. */}
        <nav
          className="flex shrink-0 border-b md:h-full md:w-60 md:flex-col md:border-b-0 md:border-r"
          aria-label="Workflow contents"
          data-testid="workflow-sidebar"
        >
          <div className="flex min-h-0 flex-1 items-center gap-1 overflow-x-auto p-2 [mask-image:linear-gradient(to_right,black_calc(100%-2rem),transparent)] md:mask-none md:block md:gap-0 md:overflow-x-visible md:overflow-y-auto">
            <SidebarRow
              icon={<Sliders size={14} />}
              label="Definition"
              hint={slug.trim() || "unnamed"}
              active={selected.kind === "definition" && !visualizing}
              onClick={() => {
                setSelected(DEFINITION);
                setVisualizing(false);
              }}
              testId="definition-row"
            />

            <p className="section-title hidden px-2 pb-1 md:mt-3 md:block">Steps</p>
            <ul className="flex items-center gap-1 md:block md:space-y-0.5">
              {steps.map((s, i) => (
                <li
                  key={s.id}
                  // Native drag: the handle starts it, any row accepts it. No
                  // library for a list that never outgrows the panel.
                  onDragOver={(e) => {
                    if (dragging !== null) e.preventDefault();
                  }}
                  onDrop={(e) => {
                    e.preventDefault();
                    if (dragging === null) return;
                    move(dragging, i);
                    setDragging(null);
                  }}
                  className={cn(
                    "group flex shrink-0 items-center gap-1 rounded-[var(--radius-control)] pr-1 transition-colors md:shrink",
                    dragging === i && "opacity-50",
                    isSelected(selected, s.id) && !visualizing
                      ? "bg-raised shadow-[inset_0_0_0_1px_var(--rule-strong)]"
                      : "hover:bg-raised",
                  )}
                  data-testid="step-row"
                  data-step-name={s.name}
                >
                  <button
                    className="key key-flat hidden cursor-grab !px-1 !py-1 text-faint md:inline-flex"
                    draggable
                    onDragStart={() => setDragging(i)}
                    onDragEnd={() => setDragging(null)}
                    // Native drag is mouse-only, and this list is the only
                    // place order can be changed — so the handle also moves
                    // the step from the keyboard.
                    onKeyDown={(e) => {
                      if (e.key !== "ArrowUp" && e.key !== "ArrowDown") return;
                      e.preventDefault();
                      move(i, e.key === "ArrowUp" ? i - 1 : i + 1);
                    }}
                    aria-label={`Reorder ${s.name || "step"} with the arrow keys`}
                    data-testid="step-handle"
                  >
                    <GripVertical size={13} />
                  </button>
                  <button
                    className="min-w-0 flex-1 truncate py-1.5 text-left text-sm"
                    onClick={() => openStep(s.id)}
                    data-testid="select-step"
                  >
                    <span
                      className={cn(
                        isSelected(selected, s.id) ? "text-legend" : "text-dim",
                      )}
                    >
                      {s.name || "unnamed"}
                    </span>
                    {s.name.trim() !== "" && s.name.trim() === start.trim() && (
                      <span className="ml-1.5 text-[0.625rem] uppercase tracking-wide text-faint">
                        start
                      </span>
                    )}
                  </button>
                  <button
                    className="key key-danger !px-1.5 !py-1 md:opacity-0 md:group-hover:opacity-100 md:focus:opacity-100"
                    aria-label={`Remove step ${s.name || i + 1}`}
                    onClick={() => removeStep(i)}
                    data-testid="remove-step"
                  >
                    <Trash2 size={13} />
                  </button>
                </li>
              ))}
            </ul>

            <button
              className="key shrink-0 !px-2 !py-1.5 text-xs md:mt-2 md:w-full"
              onClick={addStep}
              data-testid="add-step"
            >
              <Plus size={14} />
              Add step
            </button>
          </div>

          <div className="flex shrink-0 items-center border-l p-2 md:border-l-0 md:border-t">
            <button
              // A toggle, not a command: a blank key that lights the same way
              // a config key holding a value does. Amber would be wrong — it
              // means a live measured value, not a control that is on.
              className={cn(
                "key key-blank !px-2 !py-1.5 text-xs md:w-full",
                visualizing && "bg-raised !text-legend",
              )}
              onClick={() => setVisualizing((v) => !v)}
              aria-pressed={visualizing}
              data-testid="visualize-workflow"
            >
              <GitBranch size={14} />
              Visualize
            </button>
          </div>
        </nav>

        <div className="min-w-0 flex-1 space-y-4 overflow-y-auto px-6 py-4">
          {error && (
            <p className="rounded-[var(--radius-control)] border border-red bg-red-quiet px-3 py-2 text-sm text-red-ink">
              {error}
            </p>
          )}

          {visualizing ? (
            <section className="panel p-4" data-testid="workflow-visual">
              <h2 className="legend">Graph</h2>
              <p className="mt-1 text-xs text-faint">Choose a step to edit it.</p>
              <div className="mt-3 overflow-auto">
                <WorkflowGraph
                  nodes={graph.nodes}
                  edges={graph.edges}
                  start={start}
                  selected={current?.name.trim()}
                  onSelect={(step) => {
                    const match = steps.find((s) => s.name.trim() === step);
                    if (match) openStep(match.id);
                  }}
                />
              </div>
            </section>
          ) : current ? (
            // Capped like every other form surface in the build: a name field
            // stretched across a 1440px pane is a field nobody can scan.
            <div className="max-w-3xl">
              <StepForm
                step={current}
                agents={agents ?? []}
                stepNames={stepNames}
                onChange={(patch) => setStep(current.id, patch)}
              />
            </div>
          ) : (
            <section
              className="panel max-w-3xl space-y-3 p-4"
              data-testid="definition-form"
            >
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
                  data-testid="workflow-description"
                />
              </label>
              <label className="block">
                <span className="section-title">Step budget</span>
                <input
                  className="field mt-1 w-full"
                  type="number"
                  min={1}
                  value={maxSteps}
                  placeholder="100 (default)"
                  onChange={(e) => setMaxSteps(e.target.value)}
                  data-testid="workflow-max-steps"
                />
                <span className="mt-1 block text-xs text-faint">
                  Most steps one run may execute. This is what stops a loop whose
                  condition never flips; raise it for a graph that legitimately
                  loops far.
                </span>
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
          )}
        </div>
      </div>
    </div>
  );
}

function SidebarRow({
  icon,
  label,
  hint,
  active,
  onClick,
  testId,
}: {
  icon: ReactNode;
  label: string;
  hint?: string;
  active: boolean;
  onClick: () => void;
  testId: string;
}) {
  return (
    <button
      className={cn(
        "flex shrink-0 items-center gap-2 rounded-[var(--radius-control)] px-2 py-1.5 text-left transition-colors md:w-full",
        active
          ? "bg-raised text-legend shadow-[inset_0_0_0_1px_var(--rule-strong)]"
          : "text-dim hover:bg-raised hover:text-legend",
      )}
      onClick={onClick}
      data-testid={testId}
    >
      <span className="text-faint">{icon}</span>
      <span className="min-w-0 flex-1">
        <span className="block text-sm">{label}</span>
        {hint && <span className="block truncate text-xs text-faint">{hint}</span>}
      </span>
    </button>
  );
}
