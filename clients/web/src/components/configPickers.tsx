import {
  Boxes,
  Brain,
  ChevronDown,
  Cpu,
  Lightbulb,
  Plug,
  Server,
  Workflow,
  Wrench,
  Check,
} from "lucide-react";
import { useState, type ReactNode } from "react";
import { Link } from "react-router-dom";
import { useGithubRepos } from "../hooks/useGithub";
import { useMcpServers } from "../hooks/useMcp";
import { useMemorySpaces } from "../hooks/useMemory";
import { usePlugins } from "../hooks/usePlugins";
import { useSettings } from "../hooks/useSettings";
import { allTools, defaultSelection, useTools } from "../hooks/useTools";
import type {
  AgentDocument,
  SessionDetail,
  ToolCatalog,
  ToolGroupView,
} from "../api/types";
import { ToolAccess } from "../api/types";
import { cn } from "../lib/cn";
import { basename } from "../lib/format";
import { ReadError } from "./ReadError";
import type {
  AgentChannel,
  ConfigDraft,
  EnvironmentChannel,
  WorkflowChannel,
} from "../hooks/useSessionDraft";

/**
 * One configurable channel of a session or agent preset.
 *
 * The two surfaces that render these — the session action row and the agent
 * form — disagree about everything except *what the options are*, so that is
 * the only thing shared. The row wants a bare key with a dot; the form wants a
 * labelled row. Neither should own the option lists, which is what forced the
 * agent form to borrow the action row wholesale and inherit its
 * bottom-anchored layout.
 */
export interface PickerSpec {
  key: string;
  /** The channel name — visible in the form, the accessible name in the row. */
  legend: string;
  icon: ReactNode;
  /** The current value, rendered in the form and spoken in the row. */
  label: string;
  /** Something other than the default is chosen. */
  marked: boolean;
  /** The control works, but what it configures needs attention. */
  warn?: boolean;
  width: string;
  /** Tailwind max-height for the popover; omitted leaves `PopoverMenu`'s
   * default, which suits a list of bare names but not one of described
   * options. */
  height?: string;
  testId: string;
  body: (close: () => void) => ReactNode;
}

/** Keep selected picker choices legible without changing the compact menu
 * layout. Every button that wears this also carries `data-popover-option`, which
 * is what tells `PopoverMenu` to give the list arrow keys and one tab stop —
 * the checklists and the radio group get that from their native controls. */
function optionClass(selected: boolean): string {
  return cn(
    "flex w-full items-center gap-2 rounded-[var(--radius-chip)] px-2 py-1.5 text-left text-sm",
    selected ? "bg-raised text-legend" : "hover:bg-raised",
  );
}

function SelectedMark() {
  return <Check size={14} className="ml-auto shrink-0" aria-hidden />;
}

/** A list of tickable names — repos, skills, MCP servers, memory spaces all
 * present the same way, so they are one function rather than four copies. */
function checkList<T extends string>({
  items,
  selected,
  onToggle,
  empty,
}: {
  items: T[];
  selected: Set<T> | Map<T, string>;
  onToggle: (name: T, checked: boolean) => void;
  empty: ReactNode;
}): ReactNode {
  if (items.length === 0) return empty;
  return (
    <div className="space-y-0.5">
      {items.map((name) => {
        const checked = selected.has(name);
        return (
          <label
            key={name}
            className="flex cursor-pointer items-center gap-2 px-2 py-1 text-sm hover:bg-raised"
          >
            <input
              type="checkbox"
              checked={checked}
              onChange={() => onToggle(name, checked)}
            />
            <span className="min-w-0 flex-1 truncate font-mono">{name}</span>
          </label>
        );
      })}
    </div>
  );
}

/**
 * The read/write badge.
 *
 * The distinction is about the tool, not the call — `bash` is a write because
 * it can be one — so the badge answers "could this change something", which is
 * the question someone narrowing a selection is actually asking.
 */
function AccessBadge({ access }: { access: ToolAccess }) {
  const read = access === ToolAccess.Read;
  return (
    <span
      className={cn(
        "shrink-0 rounded-[var(--radius-chip)] px-1 py-px text-[0.625rem] tracking-wide uppercase",
        read ? "bg-raised text-dim" : "bg-raised text-legend",
      )}
      data-testid="tool-access"
      data-access={read ? "read" : "write"}
    >
      {read ? "read" : "write"}
    </span>
  );
}

/**
 * One group, as a collapsed row that expands.
 *
 * The row is the control: its checkbox selects or clears the whole group
 * without opening anything, which is how most selections are actually made —
 * "no shell access", "no server admin" are group-shaped thoughts, not
 * tool-shaped ones. Opening it is for the rarer case of wanting three of the
 * ten.
 *
 * Two controls, deliberately not nested: a checkbox that selects, and a
 * separate button that expands. One control doing both means every attempt to
 * look inside a group also changes what is selected.
 */
function ToolGroup({
  group,
  names,
  selected,
  expanded,
  onToggleExpanded,
  onSet,
  children,
}: {
  group: ToolGroupView;
  /** The group's tools *as currently filtered* — what the row summarises and
   * what its checkbox acts on, so the two can never disagree with the list. */
  names: string[];
  selected: Set<string>;
  expanded: boolean;
  onToggleExpanded: () => void;
  onSet: (names: string[], checked: boolean) => void;
  children: ReactNode;
}) {
  const chosen = names.filter((n) => selected.has(n)).length;
  const all = chosen === names.length;
  return (
    <div data-testid={`tool-group-${group.key}`} data-expanded={expanded}>
      <div className="flex items-center gap-2 px-2 py-1 hover:bg-raised">
        <input
          type="checkbox"
          className="shrink-0"
          checked={all}
          // A group with some of its tools chosen is neither ticked nor empty,
          // and `indeterminate` is a DOM property with no HTML attribute — it
          // can only be set through the element.
          ref={(el) => {
            if (el) el.indeterminate = chosen > 0 && !all;
          }}
          aria-label={`Select all ${group.label} tools`}
          data-testid={`tool-group-all-${group.key}`}
          onChange={() => onSet(names, !all)}
        />
        <button
          type="button"
          className="flex min-w-0 flex-1 items-center gap-2 text-left"
          aria-expanded={expanded}
          data-testid={`tool-group-expand-${group.key}`}
          onClick={onToggleExpanded}
        >
          <span className="min-w-0 flex-1">
            <span className="block text-[0.6875rem] leading-tight tracking-wide text-faint uppercase">
              {group.label}
            </span>
            <span className="block truncate text-[0.6875rem] leading-tight text-faint">
              {group.description}
            </span>
          </span>
          <span className="shrink-0 font-mono text-[0.6875rem] text-faint">
            {chosen}/{names.length}
          </span>
          <ChevronDown
            size={12}
            className={cn("shrink-0 text-faint transition-transform", expanded && "rotate-180")}
            aria-hidden
          />
        </button>
      </div>
      {expanded && <div className="pb-1">{children}</div>}
    </div>
  );
}

function EmptyLink({ to, children }: { to: string; children: ReactNode }) {
  return (
    <Link to={to} className="block px-2 py-1.5 text-sm text-dim hover:text-legend">
      {children}
    </Link>
  );
}

/** A session draft carries an environment channel; an agent-preset draft does
 * not. The draft's own shape is the signal, so neither surface has to pass a
 * flag saying which one it is. */
function hasEnvironment(
  draft: ConfigDraft,
): draft is ConfigDraft & EnvironmentChannel {
  return "setEnvironment" in draft;
}

/** Likewise for the workflow channel: only the new-session draft starts one. */
function hasWorkflow(draft: ConfigDraft): draft is ConfigDraft & WorkflowChannel {
  return "setWorkflow" in draft;
}

function hasAgent(draft: ConfigDraft): draft is ConfigDraft & AgentChannel {
  return "setAgent" in draft;
}

/** Stands in when a draft has no environment channel, so the picker hook can
 * be called unconditionally. Its spec is built and discarded. */
const INERT_ENVIRONMENT: EnvironmentChannel = {
  environment: { kind: "runtime", vendor: "", repos: {} },
  setEnvironment: () => {},
  environments: [],
  provisions: false,
  githubConnected: false,
};

/**
 * The Tools picker: which built-in tools the agent may call.
 *
 * `draft.tools === null` is *not* the same as every box ticked. It means the
 * question was left to the server, so the selection follows a later horsie's
 * idea of a sensible default instead of freezing today's list — and it is what
 * keeps the control plane out, since a grant that could be inherited from an
 * unset field would not be a grant. Ticking or unticking anything answers the
 * question, and from then on the stored list is exactly what was chosen.
 *
 * MCP servers, skills and memory spaces are absent on purpose: each has its own
 * picker on this same row, and their tool names are not fixed at build time. A
 * selection here never removes what one of those turned on — see
 * `crate::tools` on the server for the same rule from the other side.
 */
function toolsPicker(
  draft: ConfigDraft,
  catalog: ToolCatalog | undefined,
  failed: boolean,
  error: unknown,
): PickerSpec {
  const groups = catalog?.groups ?? [];
  const every = allTools(catalog);
  const fallback = defaultSelection(catalog);
  // What is *ticked*. An unanswered selection shows the default set, so the
  // boxes always agree with what would actually run.
  const selected = draft.tools ?? fallback;
  const answered = draft.tools !== null;

  const label = !answered
    ? "Default"
    : selected.size === 0
      ? "None"
      : selected.size === every.length
        ? "All"
        : `${selected.size} selected`;

  // Selection actions only. The read/write control beside them is a *filter*
  // and never appears here: narrowing what you are looking at must not change
  // what you have chosen, or a glance at the read tools would silently throw
  // away every write tool you had picked.
  const select = (names: string[], checked: boolean) => {
    const next = new Set(selected);
    for (const n of names) {
      if (checked) next.add(n);
      else next.delete(n);
    }
    draft.setTools(next);
  };

  return {
    key: "tools",
    legend: "Tools",
    icon: <Wrench size={15} />,
    label,
    // Only an explicit narrowing is worth marking. "Default" is the resting
    // state, and a row of permanently-lit chips tells nobody anything.
    marked: answered,
    warn: failed,
    width: "w-96",
    // Two lines and a badge per option across eight groups: the default 18rem
    // shows four tools and hides the Horsie group behind a scrollbar nobody
    // has a reason to look for.
    height: "max-h-[32rem]",
    testId: "config-tools",
    body: () =>
      failed ? (
        <ReadError
          what="the tool catalogue"
          error={error}
          testId="tools-read-error"
          className="mx-1 my-0.5"
        />
      ) : (
        <ToolsBody
          groups={groups}
          selected={selected}
          onSet={select}
          onDefault={() => draft.setTools(null)}
        />
      ),
  };
}

/**
 * The Tools popover's contents.
 *
 * A component rather than inline JSX because which groups are open is state,
 * and `PickerSpec.body` is a plain render function called from a hook — there
 * is nowhere in `toolsPicker` for a `useState` to live.
 *
 * Deliberately *not* lifted into the draft: which groups you have open is not
 * part of the session you are configuring. It should not persist, travel to the
 * server, or make a preset look edited.
 */
/** What the read/write control is showing. Not a selection — see `ToolsBody`. */
type AccessFilter = "all" | "read" | "write";

/**
 * The Tools popover's contents.
 *
 * A component rather than inline JSX because which groups are open, and which
 * access the list is filtered to, are both state — and `PickerSpec.body` is a
 * plain render function called from a hook, so there is nowhere in
 * `toolsPicker` for a `useState` to live.
 *
 * Deliberately *not* lifted into the draft: neither belongs to the session you
 * are configuring. They should not persist, travel to the server, or make a
 * preset look edited.
 *
 * **The read/write control filters, it does not select.** Switching to Read
 * hides the write tools; anything already chosen among them stays chosen and
 * comes back into view under All or Write. The controls that *do* select — the
 * group boxes and Select all / Clear — act on what is currently listed, so
 * they never reach behind the filter to change something you cannot see.
 */
function ToolsBody({
  groups,
  selected,
  onSet,
  onDefault,
}: {
  groups: ToolGroupView[];
  selected: Set<string>;
  onSet: (names: string[], checked: boolean) => void;
  onDefault: () => void;
}) {
  const [filter, setFilter] = useState<AccessFilter>("all");
  const shown = (g: ToolGroupView) =>
    g.tools.filter(
      (t) =>
        filter === "all" ||
        (filter === "read") === (t.access === ToolAccess.Read),
    );
  // A group with nothing matching is not an empty group, it is a group this
  // filter has nothing to say about — so it goes rather than reading "0/0".
  const visibleGroups = groups.filter((g) => shown(g).length > 0);
  const visibleNames = visibleGroups.flatMap((g) => shown(g).map((t) => t.name));

  // Open only what the row cannot summarise. A group entirely on or entirely
  // off is fully described by its checkbox and its count; a partly chosen one
  // is the only case where the answer is inside — so a narrowed preset shows
  // what it narrowed to without a click.
  const [expanded, setExpanded] = useState<Set<string>>(
    () =>
      new Set(
        groups
          .filter((g) => {
            const chosen = g.tools.filter((t) => selected.has(t.name)).length;
            return chosen > 0 && chosen < g.tools.length;
          })
          .map((g) => g.key),
      ),
  );
  const toggle = (key: string) =>
    setExpanded((prev) => {
      const next = new Set(prev);
      if (!next.delete(key)) next.add(key);
      return next;
    });

  const filters: { key: AccessFilter; label: string }[] = [
    { key: "all", label: "All" },
    { key: "read", label: "Read" },
    { key: "write", label: "Write" },
  ];

  return (
    <div className="space-y-0.5" data-testid="tools-body" data-filter={filter}>
      <div className="flex flex-wrap items-center gap-1 px-2 pt-0.5 pb-1">
        <span className="flex overflow-hidden rounded-[var(--radius-chip)] bg-raised">
          {filters.map((f) => (
            <button
              key={f.key}
              type="button"
              className={cn(
                "px-1.5 py-0.5 text-[0.6875rem]",
                filter === f.key ? "bg-legend/10 text-legend" : "text-dim hover:text-legend",
              )}
              aria-pressed={filter === f.key}
              data-testid={`tool-filter-${f.key}`}
              onClick={() => setFilter(f.key)}
            >
              {f.label}
            </button>
          ))}
        </span>
        <span className="ml-auto flex gap-1">
          <button
            type="button"
            className="rounded-[var(--radius-chip)] bg-raised px-1.5 py-0.5 text-[0.6875rem] text-dim hover:text-legend"
            data-testid="tool-quick-default"
            onClick={onDefault}
          >
            Default
          </button>
          <button
            type="button"
            className="rounded-[var(--radius-chip)] bg-raised px-1.5 py-0.5 text-[0.6875rem] text-dim hover:text-legend"
            data-testid="tool-quick-all"
            onClick={() => onSet(visibleNames, true)}
          >
            Select all
          </button>
          <button
            type="button"
            className="rounded-[var(--radius-chip)] bg-raised px-1.5 py-0.5 text-[0.6875rem] text-dim hover:text-legend"
            data-testid="tool-quick-none"
            onClick={() => onSet(visibleNames, false)}
          >
            Clear
          </button>
        </span>
      </div>
      {visibleGroups.map((group) => (
        <ToolGroup
          key={group.key}
          group={group}
          names={shown(group).map((t) => t.name)}
          selected={selected}
          expanded={expanded.has(group.key)}
          onToggleExpanded={() => toggle(group.key)}
          onSet={onSet}
        >
          {shown(group).map((tool) => {
            const checked = selected.has(tool.name);
            return (
              <label
                key={tool.name}
                className="flex cursor-pointer items-center gap-2 py-1 pr-2 pl-8 text-sm hover:bg-raised"
                data-testid="tool-option"
                data-value={tool.name}
                data-selected={checked}
              >
                <input
                  type="checkbox"
                  checked={checked}
                  onChange={() => onSet([tool.name], !checked)}
                />
                <span className="min-w-0 flex-1">
                  <span className="block truncate font-mono text-sm text-legend">
                    {tool.name}
                  </span>
                  <span className="block text-[0.6875rem] leading-snug text-faint">
                    {tool.description}
                  </span>
                </span>
                <AccessBadge access={tool.access} />
              </label>
            );
          })}
        </ToolGroup>
      ))}
    </div>
  );
}

/**
 * The one Environment picker, as its own hook.
 *
 * Standalone because four surfaces need it and only one of them — the session
 * config bar — wants the rest of `useConfigPickers`. The routine form renders
 * exactly this spec as a labelled field, which is what makes the two lists
 * identical rather than merely similar.
 */
export function useEnvironmentPicker(d: EnvironmentChannel): PickerSpec {
  const { data: settings, isError: settingsFailed, error: settingsError } =
    useSettings();
  const { data: repoList, isError: reposFailed, error: reposError } =
    useGithubRepos(d.provisions && d.githubConnected);
  const activeVendors = settings?.vendors ?? [];
  const chosen =
    d.environment.kind === "named"
      ? d.environment.name
      : d.environment.vendor;
  // What a predefined selection resolves to, for the read-only summary the
  // picker shows under it.
  const namedName = d.environment.kind === "named" ? d.environment.name : undefined;
  const named =
    namedName === undefined
      ? undefined
      : d.environments.find((e) => e.name === namedName);
  // The ad-hoc half, bound once: the repo checklist below only renders for it,
  // and reading `d.environment` again inside those callbacks loses the
  // narrowing and invites a fallback that can never fire.
  const adhoc = d.environment.kind === "runtime" ? d.environment : undefined;
  const repos = new Map(Object.entries(adhoc?.repos ?? {}));
  return {
    key: "environment",
    legend: "Environment",
    icon: <Server size={15} />,
    label: chosen || "Select",
    marked: !!chosen,
    // The new-session page used to carry a whole roster panel answering "is
    // my laptop connected?". The answer is one bit, and this is where it is
    // actionable. A failed read is not that answer, but it is still something
    // to look at — the popover says which of the two it is.
    warn:
      settingsFailed ||
      (activeVendors.length === 0 && d.environments.length === 0),
    width: "w-80",
    testId: "config-environment",
    body: (close) => (
      <div className="space-y-1">
        {d.environments.length > 0 && (
          <>
            <p className="px-2 pt-0.5 text-[0.6875rem] tracking-wide text-faint uppercase">
              Predefined
            </p>
            {d.environments.map((e) => {
              const selected =
                d.environment.kind === "named" && d.environment.name === e.name;
              return (
                <button
                  key={e.name}
                  type="button"
                  className={optionClass(selected)}
                  data-popover-option
                  data-testid="environment-option"
                  data-value={e.name}
                  data-kind="named"
                  data-selected={selected}
                  aria-pressed={selected}
                  onClick={() => {
                    d.setEnvironment({ kind: "named", name: e.name });
                    close();
                  }}
                >
                  <span className="min-w-0 flex-1 truncate font-mono">{e.name}</span>
                  <span className="text-[0.6875rem] text-faint">
                    {e.vendor}
                    {e.repos.length > 0 &&
                      ` · ${e.repos.length} repo${e.repos.length === 1 ? "" : "s"}`}
                  </span>
                  {selected && <SelectedMark />}
                </button>
              );
            })}
          </>
        )}
        <p className="px-2 pt-0.5 text-[0.6875rem] tracking-wide text-faint uppercase">
          Runtimes
        </p>
        {settingsFailed ? (
          // Without the config read there is no roster to be empty: saying "no
          // runtime is connected" here would send someone to re-run `horsie
          // connect` for a runtime that is probably already there.
          <ReadError
            what="runtimes"
            error={settingsError}
            testId="environment-read-error"
            className="mx-1 my-0.5"
          />
        ) : activeVendors.length === 0 ? (
          <p className="px-2 py-1.5 text-sm leading-relaxed text-dim">
            No runtime is connected, so a session can’t run a turn yet. Run{" "}
            <code className="font-mono text-legend">horsie connect</code> on the
            machine holding your code.
          </p>
        ) : (
          activeVendors.map((v) => {
            const selected =
              d.environment.kind === "runtime" && d.environment.vendor === v.name;
            return (
              <button
                key={v.name}
                type="button"
                className={optionClass(selected)}
                data-popover-option
                data-testid="environment-option"
                data-value={v.name}
                data-kind="runtime"
                data-selected={selected}
                aria-pressed={selected}
                onClick={() =>
                  d.setEnvironment({
                    kind: "runtime",
                    vendor: v.name,
                    // Keep the repos already ticked when swapping between
                    // two provisioning vendors: the selection is about
                    // where, not about what.
                    repos: adhoc?.repos ?? {},
                  })
                }
              >
                <span className="min-w-0 flex-1 truncate font-mono">{v.name}</span>
                {v.isDefault && (
                  <span className="text-[0.6875rem] text-faint">default</span>
                )}
                {selected && <SelectedMark />}
              </button>
            );
          })
        )}

        {/*
          What follows the divider depends on the selection: a predefined
          environment's repos are part of its definition and shown read-only,
          an ad-hoc one's are picked here, and a runtime that cannot
          provision has nowhere to check anything out.
        */}
        {named && (
          <div className="pt-1.5" data-testid="environment-summary">
            <p className="px-2 pb-0.5 text-[0.6875rem] tracking-wide text-faint uppercase">
              Repos
            </p>
            {named.repos.length === 0 ? (
              <p className="px-2 py-1 text-sm text-faint">None</p>
            ) : (
              <ul className="space-y-0.5 px-2 py-0.5">
                {named.repos.map((r) => (
                  <li
                    key={r.url}
                    className="truncate font-mono text-[0.8125rem] text-legend"
                  >
                    {basename(r.url)}
                    {r.gitRef && <span className="text-faint"> @ {r.gitRef}</span>}
                  </li>
                ))}
              </ul>
            )}
          </div>
        )}
        {adhoc && d.provisions && (
          <div className="pt-1.5" data-testid="environment-repos">
            <p className="px-2 pb-0.5 text-[0.6875rem] tracking-wide text-faint uppercase">
              Repos
            </p>
            {!d.githubConnected ? (
              <EmptyLink to="/settings/integrations">
                Connect GitHub in Settings to pick repos
              </EmptyLink>
            ) : (
              checkList({
                items: (repoList?.repos ?? []).map((r) => r.fullName),
                selected: repos,
                onToggle: (name, checked) => {
                  const next = new Map(repos);
                  if (checked) next.delete(name);
                  else next.set(name, "");
                  d.setEnvironment({
                    kind: "runtime",
                    vendor: adhoc.vendor,
                    repos: Object.fromEntries(next),
                  });
                },
                empty: reposFailed ? (
                  <ReadError
                    what="repos"
                    error={reposError}
                    testId="environment-repos-read-error"
                    className="mx-1 my-0.5"
                  />
                ) : (
                  <p className="px-2 py-1 text-sm text-dim">
                    No repos visible to the app installation.
                  </p>
                ),
              })
            )}
          </div>
        )}
      </div>
    ),
  };
}

/**
 * Every picker for a draft, in the order both surfaces render them.
 *
 * Model and Thinking come last and adjacent: thinking effort is a property of
 * the model, so putting them side by side is the one ordering that reads as a
 * single decision rather than two unrelated switches.
 */
export function useConfigPickers(draft: ConfigDraft): PickerSpec[] {
  const { data: settings, isError: settingsFailed, error: settingsError } =
    useSettings();
  const { data: bundles, isError: bundlesFailed, error: bundlesError } =
    usePlugins();
  const { data: mcpServers, isError: mcpFailed, error: mcpError } =
    useMcpServers();
  const { data: memorySpaces, isError: memoryFailed, error: memoryError } =
    useMemorySpaces();
  const { data: toolCatalog, isError: toolsFailed, error: toolsError } = useTools();
  const env = hasEnvironment(draft) ? draft : undefined;
  // Called unconditionally with an inert channel when the draft has none: a
  // hook cannot be conditional, and an agent-preset form has no environment.
  const environmentPicker = useEnvironmentPicker(env ?? INERT_ENVIRONMENT);

  const models = settings?.models ?? [];
  const enabledMcp = (mcpServers ?? []).filter((s) => s.enabled);

  const pickers: PickerSpec[] = [];

  // Leftmost, and first read: it decides which of the others mean anything.
  const running = hasWorkflow(draft) ? draft.workflow : "";
  const agentChannel = hasAgent(draft) ? draft : undefined;
  const selectedAgent = agentChannel?.agent ?? "";
  if (hasWorkflow(draft) && !selectedAgent) {
    const d = draft;
    pickers.push({
      key: "workflow",
      legend: "Workflow",
      icon: <Workflow size={15} />,
      label: d.workflow || "None",
      marked: !!d.workflow,
      width: "w-64",
      testId: "config-workflow",
      body: (close) => (
        <>
          <button
            type="button"
            className={optionClass(d.workflow === "")}
            data-popover-option
            data-testid="workflow-option"
            data-value=""
            data-selected={d.workflow === ""}
            aria-pressed={d.workflow === ""}
            onClick={() => {
              d.setWorkflow("");
              close();
            }}
          >
            None
            <span className="ml-auto text-[0.6875rem] text-faint">one agent</span>
            {d.workflow === "" && <SelectedMark />}
          </button>
          {d.workflows.length === 0 ? (
            <EmptyLink to="/workflows">Define a workflow to run one</EmptyLink>
          ) : (
            d.workflows.map((w) => (
              <button
                key={w}
                type="button"
                className={optionClass(d.workflow === w)}
                data-popover-option
                data-testid="workflow-option"
                data-value={w}
                data-selected={d.workflow === w}
                aria-pressed={d.workflow === w}
                onClick={() => {
                  d.setWorkflow(w);
                  close();
                }}
              >
                <span className="font-mono">{w}</span>
                {d.workflow === w && <SelectedMark />}
              </button>
            ))
          )}
        </>
      ),
    });
  }

  if (env) {
    pickers.push(environmentPicker);
  }

  // A run's model, thinking effort, skills, MCP and memory come from each
  // step's own agent preset, and `WorkflowRunRequest` carries none of them —
  // so while a workflow is selected these controls would configure nothing.
  if (running) return pickers;

  // An agent preset supplies every agent channel itself. Keep it in the Model
  // menu as a mutually-exclusive alternative to configuring a model directly.
  if (selectedAgent && agentChannel) {
    pickers.push({
      key: "model",
      legend: "Model",
      icon: <Cpu size={15} />,
      label: selectedAgent,
      marked: true,
      width: "w-72",
      testId: "config-model",
      warn: settingsFailed,
      body: (close) => (
        <>
          <p className="px-2 pt-0.5 text-[0.6875rem] tracking-wide text-faint uppercase">
            Models
          </p>
          {models.map((m) => (
            <button
              key={m.alias}
              type="button"
              className={optionClass(false)}
              data-popover-option
              data-testid="model-option"
              data-value={m.alias}
              data-selected={false}
              aria-pressed={false}
              onClick={() => {
                draft.setModel(m.alias);
                agentChannel.setAgent("");
                close();
              }}
            >
              <span className="min-w-0 flex-1">
                <span className="block font-mono text-sm text-legend">{m.alias}</span>
                <span className="block text-[0.6875rem] text-faint">{m.modelId}</span>
              </span>
            </button>
          ))}
          <p className="px-2 pt-1.5 text-[0.6875rem] tracking-wide text-faint uppercase">
            Agents
          </p>
          {agentChannel.agents.map((agent) => (
            <button
              key={agent}
              type="button"
              className={optionClass(agent === selectedAgent)}
              data-popover-option
              data-testid="agent-option"
              data-value={agent}
              data-selected={agent === selectedAgent}
              aria-pressed={agent === selectedAgent}
              onClick={() => {
                agentChannel.setAgent(agent);
                close();
              }}
            >
              <span className="min-w-0 flex-1 font-mono text-sm text-legend">{agent}</span>
              {agent === selectedAgent && <SelectedMark />}
            </button>
          ))}
        </>
      ),
    });
    return pickers;
  }

  // Skills and MCP are not workspace channels, so they are offered on every
  // vendor. A runtime fetches its selected bundles over its own outbound
  // connection into its own plugins dir — nothing it has to have built — and
  // an MCP toolbox is composed server-side and never reaches the runtime at
  // all. Gating these on `provisions` kept both off `horsie connect`, which
  // is the most common self-hosted vendor.
  pickers.push({
    key: "skills",
    legend: "Skills",
    icon: <Boxes size={15} />,
    label: draft.skills.size ? `${draft.skills.size} selected` : "None",
    marked: draft.skills.size > 0,
    width: "w-80",
    testId: "config-skills",
    body: () =>
      checkList({
        items: (bundles ?? []).map((b) => b.name),
        selected: draft.skills,
        onToggle: (name, checked) => {
          const next = new Set(draft.skills);
          if (checked) next.delete(name);
          else next.add(name);
          draft.setSkills(next);
        },
        empty: bundlesFailed ? (
          <ReadError
            what="skill bundles"
            error={bundlesError}
            testId="skills-read-error"
            className="mx-1 my-0.5"
          />
        ) : (
          <EmptyLink to="/settings/skills">
            Install skill bundles in Settings
          </EmptyLink>
        ),
      }),
  });

  pickers.push({
    key: "mcp",
    legend: "MCP",
    icon: <Plug size={15} />,
    label: draft.mcp.size ? `${draft.mcp.size} selected` : "None",
    marked: draft.mcp.size > 0,
    width: "w-72",
    testId: "config-mcp",
    body: () =>
      checkList({
        items: enabledMcp.map((s) => s.name),
        selected: draft.mcp,
        onToggle: (name, checked) => {
          const next = new Set(draft.mcp);
          if (checked) next.delete(name);
          else next.add(name);
          draft.setMcp(next);
        },
        empty: mcpFailed ? (
          <ReadError
            what="MCP servers"
            error={mcpError}
            testId="mcp-read-error"
            className="mx-1 my-0.5"
          />
        ) : (
          <EmptyLink to="/settings/integrations">
            Add MCP servers in Settings
          </EmptyLink>
        ),
      }),
  });

  // Not gated on `provisions`: the server owns memory, so it works on vendors
  // that cannot provision a workspace too.
  pickers.push({
    key: "memory",
    legend: "Memory",
    icon: <Brain size={15} />,
    label: draft.memorySpaces.size ? `${draft.memorySpaces.size} selected` : "None",
    marked: draft.memorySpaces.size > 0,
    width: "w-72",
    testId: "config-memory",
    body: () =>
      checkList({
        items: (memorySpaces ?? []).map((sp) => sp.name),
        selected: draft.memorySpaces,
        onToggle: (name, checked) => {
          const next = new Set(draft.memorySpaces);
          if (checked) next.delete(name);
          else next.add(name);
          draft.setMemorySpaces(next);
        },
        empty: memoryFailed ? (
          <ReadError
            what="memory spaces"
            error={memoryError}
            testId="memory-read-error"
            className="mx-1 my-0.5"
          />
        ) : (
          <EmptyLink to="/settings/memory">Create a memory space first</EmptyLink>
        ),
      }),
  });

  // Between the toolbox channels and the model: it is the widest of them, and
  // the one whose answer changes what the others are for.
  pickers.push(toolsPicker(draft, toolCatalog, toolsFailed, toolsError));

  pickers.push({
    key: "model",
    legend: "Model",
    icon: <Cpu size={15} />,
    label: models.find((m) => m.alias === draft.model)?.alias ?? "Select",
    marked: !!draft.model,
    width: "w-72",
    testId: "config-model",
    warn: settingsFailed,
    body: (close) =>
      settingsFailed ? (
        <ReadError
          what="models"
          error={settingsError}
          testId="model-read-error"
          className="mx-1 my-0.5"
        />
      ) : models.length === 0 ? (
        <EmptyLink to="/settings/models">
          No models configured — add one in Settings
        </EmptyLink>
      ) : agentChannel ? (
        <>
          <p className="px-2 pt-0.5 text-[0.6875rem] tracking-wide text-faint uppercase">
            Models
          </p>
          {models.map((m) => (
            <button
              key={m.alias}
              type="button"
              className={optionClass(draft.model === m.alias)}
              data-popover-option
              data-testid="model-option"
              data-value={m.alias}
              data-selected={draft.model === m.alias}
              aria-pressed={draft.model === m.alias}
              onClick={() => {
                draft.setModel(m.alias);
                close();
              }}
            >
              <span className="min-w-0 flex-1">
                <span className="block font-mono text-sm text-legend">{m.alias}</span>
                <span className="block text-[0.6875rem] text-faint">{m.modelId}</span>
              </span>
              {draft.model === m.alias && <SelectedMark />}
            </button>
          ))}
          <p className="px-2 pt-1.5 text-[0.6875rem] tracking-wide text-faint uppercase">
            Agents
          </p>
          {agentChannel.agents.map((agent) => (
            <button
              key={agent}
              type="button"
              className={optionClass(false)}
              data-popover-option
              data-testid="agent-option"
              data-value={agent}
              data-selected={false}
              aria-pressed={false}
              onClick={() => {
                agentChannel.setAgent(agent);
                close();
              }}
            >
              <span className="min-w-0 flex-1 font-mono text-sm text-legend">{agent}</span>
            </button>
          ))}
        </>
      ) : (
        models.map((m) => (
          <button
            key={m.alias}
            type="button"
            className={optionClass(draft.model === m.alias)}
            data-popover-option
            data-testid="model-option"
            data-value={m.alias}
            data-selected={draft.model === m.alias}
            aria-pressed={draft.model === m.alias}
            onClick={() => {
              draft.setModel(m.alias);
              close();
            }}
          >
            <span className="min-w-0 flex-1">
              <span className="block font-mono text-sm text-legend">{m.alias}</span>
              <span className="block text-[0.6875rem] text-faint">{m.modelId}</span>
            </span>
            {draft.model === m.alias && <SelectedMark />}
          </button>
        ))
      ),
  });

  // Only for models that offer a menu. The value is fixed for the session's
  // lifetime: changing effort mid-session invalidates the prompt cache.
  if (draft.thinkingEfforts.length > 0) {
    pickers.push({
      key: "thinking",
      legend: "Thinking",
      icon: <Lightbulb size={15} />,
      label: draft.thinkingEffort || "Default",
      marked: !!draft.thinkingEffort,
      width: "w-52",
      testId: "config-thinking",
      body: () => (
        <div className="space-y-0.5">
          <label
            className={cn(optionClass(draft.thinkingEffort === ""), "cursor-pointer")}
            data-selected={draft.thinkingEffort === ""}
          >
            <input
              type="radio"
              name="thinking-effort"
              checked={draft.thinkingEffort === ""}
              onChange={() => draft.setThinkingEffort("")}
            />
            <span className="min-w-0 flex-1 truncate">
              {draft.modelDefaultThinkingEffort
                ? `default (${draft.modelDefaultThinkingEffort})`
                : "default"}
            </span>
            {draft.thinkingEffort === "" && <SelectedMark />}
          </label>
          {draft.thinkingEfforts.map((e) => (
            <label
              key={e}
              className={cn(optionClass(draft.thinkingEffort === e), "cursor-pointer")}
              data-selected={draft.thinkingEffort === e}
            >
              <input
                type="radio"
                name="thinking-effort"
                checked={draft.thinkingEffort === e}
                onChange={() => draft.setThinkingEffort(e)}
              />
              <span className="min-w-0 flex-1 truncate font-mono">{e}</span>
              {draft.thinkingEffort === e && <SelectedMark />}
            </label>
          ))}
        </div>
      ),
    });
  }

  // Auto-compaction had a key here and no longer does. It is the one setting
  // on this row with a single sensible answer — the alternative is a session
  // that stops working once its context fills — so it sat beside the model and
  // the skills asking for a decision that nobody wants to make. It stays a real
  // field on the wire for an API caller that means it; the UI just assumes it.

  return pickers;
}

/**
 * The same channels for a session that already exists.
 *
 * A created session's configuration is frozen, but it is still the thing you
 * check when something surprises you — so the row keeps its shape and its
 * position, and each key opens a readout instead of a picker. Nothing here is
 * editable; `marked` means "this session has one", not "you chose one".
 */
export function useLockedChannels(
  detail: SessionDetail,
  agent: AgentDocument,
): PickerSpec[] {
  const { data: settings } = useSettings();
  // A model alias can be renamed or deleted out from under a live session, and
  // the next turn then fails `no provider registered for model '…'`. The row
  // used to show the dead alias exactly as it shows a live one, so the only
  // symptom was a turn that stopped working. It cannot be repaired here —
  // there is no API to repoint an existing session — but it can at least stop
  // being a surprise.
  const modelGone =
    !!settings && !settings.models.some((m) => m.alias === agent.model);

  const value = (items: string[]) =>
    items.length ? items.join(", ") : "None";

  const readout = (items: string[]) => () => (
    <div className="space-y-1.5 px-1 py-0.5">
      {items.length === 0 ? (
        <p className="text-sm text-faint">None</p>
      ) : (
        <ul className="space-y-0.5">
          {items.map((v) => (
            <li key={v} className="font-mono text-[0.8125rem] break-words text-legend">
              {v}
            </li>
          ))}
        </ul>
      )}
    </div>
  );

  // Only the channels this session actually has. The draft row hides the
  // workspace channels on a vendor that cannot provision one, and `SessionDetail`
  // does not carry that capability — but an empty list means the same thing
  // here, and five keys that all read "None" is a row that says nothing.
  const optional = (
    key: string,
    legend: string,
    icon: ReactNode,
    width: string,
    items: string[],
  ): PickerSpec[] =>
    items.length === 0
      ? []
      : [
          {
            key,
            legend,
            icon,
            label: value(items),
            marked: true,
            width,
            testId: `config-${key}`,
            body: readout(items),
          },
        ];

  // One key, matching the draft row: the environment is where this session
  // runs, and the vendor and repos are what it resolved to. A predefined one
  // leads with its name, because that is what was chosen.
  const environment: PickerSpec = {
    key: "environment",
    legend: "Environment",
    icon: <Server size={15} />,
    label: detail.environment ?? detail.vendor,
    marked: true,
    width: "w-80",
    testId: "config-environment",
    body: readout([
      ...(detail.environment ? [detail.environment] : []),
      detail.vendor,
      ...detail.repos.map(basename),
    ]),
  };

  // Model, MCP, memory and thinking read the *selected agent's* document:
  // a workflow step's configuration is its own preset's, and the session
  // document deliberately carries no session-wide model. Skills remain
  // session-wide — the bundle union is provisioned for the whole run.
  const channels: PickerSpec[] = [
    environment,
    ...optional("skills", "Skills", <Boxes size={15} />, "w-80", detail.plugins),
    ...optional("mcp", "MCP", <Plug size={15} />, "w-72", agent.mcpServers),
    ...optional("memory", "Memory", <Brain size={15} />, "w-72", agent.memorySpaces),
    // Unlike the others, absent is a real answer here rather than "nothing to
    // show": a session on the default set has no list to read out, and saying
    // so is what tells you the tools were *not* the reason a call was refused.
    {
      key: "tools",
      legend: "Tools",
      icon: <Wrench size={15} />,
      label: agent.allowedTools ? `${agent.allowedTools.length} selected` : "Default",
      marked: !!agent.allowedTools,
      width: "w-80",
      testId: "config-tools",
      body: agent.allowedTools
        ? readout(agent.allowedTools)
        : () => (
            <p className="px-1 py-0.5 text-sm text-faint">
              Every built-in tool except the control plane — this server's
              default set.
            </p>
          ),
    },
    {
      key: "model",
      legend: "Model",
      icon: <Cpu size={15} />,
      label: modelGone ? `${agent.model} — missing` : agent.model,
      marked: true,
      warn: modelGone,
      width: "w-72",
      testId: "config-model",
      body: modelGone
        ? () => (
            <div className="space-y-1.5 px-1 py-0.5">
              <p className="font-mono text-[0.8125rem] break-words text-legend">
                {agent.model}
              </p>
              <p className="text-sm leading-relaxed text-red-ink">
                This model is no longer configured, so the next turn in this
                session will fail. Restore the alias in Settings → Models, or
                start a new session.
              </p>
            </div>
          )
        : readout([agent.model]),
    },
  ];

  if (agent.thinkingEffort) {
    channels.push({
      key: "thinking",
      legend: "Thinking",
      icon: <Lightbulb size={15} />,
      label: agent.thinkingEffort,
      marked: true,
      width: "w-52",
      testId: "config-thinking",
      body: readout([agent.thinkingEffort]),
    });
  }

  return channels;
}
