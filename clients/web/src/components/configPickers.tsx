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
import { Trans, useTranslation } from "react-i18next";
import { Link } from "react-router-dom";
import { useGithubRepos } from "../hooks/useGithub";
import { useMcpServer, useMcpServers } from "../hooks/useMcp";
import { useMemorySpaces } from "../hooks/useMemory";
import { usePlugins } from "../hooks/usePlugins";
import { useSettings } from "../hooks/useSettings";
import { allTools, defaultSelection, useTools } from "../hooks/useTools";
import type { McpServerView, ToolCatalog, ToolGroupView } from "../api/types";
import { ToolAccess } from "../api/types";
import { i18n } from "../i18n";
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

/**
 * Whether this row configures something or reports it.
 *
 * One flag rather than two hooks. A created session's configuration is frozen,
 * but it is still the thing you check when something surprises you — so it must
 * be the *same* row, with the same keys in the same order showing the same
 * lists, and only the controls turned off. The two used to be built by separate
 * functions and drifted into two different pictures of one fact: the frozen one
 * flattened every list to a comma-joined string and dropped any channel that
 * held nothing, so a session narrowed to no skills looked exactly like a
 * session nobody had narrowed.
 */
export type ConfigMode = "edit" | "frozen";

/** Keep selected picker choices legible without changing the compact menu
 * layout. Every button that wears this also carries `data-popover-option`, which
 * is what tells `PopoverMenu` to give the list arrow keys and one tab stop —
 * the checklists and the radio group get that from their native controls.
 *
 * A frozen option loses its hover: nothing happens when you press it, and a row
 * that lights up under the pointer promises otherwise. */
function optionClass(selected: boolean, frozen = false): string {
  return cn(
    "flex w-full items-center gap-2 rounded-[var(--radius-chip)] px-2 py-1.5 text-left text-sm",
    selected ? "is-selected" : frozen ? "" : "hover:bg-raised",
  );
}

function SelectedMark() {
  return <Check size={14} className="ml-auto shrink-0" aria-hidden />;
}

/** A list of tickable names — repos, skills, MCP servers, memory spaces all
 * present the same way, so they are one function rather than four copies.
 *
 * Frozen, the list is what the session actually got: anything it selected is
 * shown ticked even when the catalogue no longer offers it. A bundle
 * uninstalled since the session started is still what that session is running,
 * and reading the live catalogue alone would quietly drop it. */
function checkList<T extends string>({
  items,
  selected,
  onToggle,
  empty,
  frozen = false,
}: {
  items: T[];
  selected: Set<T> | Map<T, string>;
  onToggle: (name: T, checked: boolean) => void;
  empty: ReactNode;
  frozen?: boolean;
}): ReactNode {
  // The catalogue first, then anything selected that is no longer in it. Not
  // filtered down to the selection: the whole point of keeping the control is
  // that it still answers "what else could this session have had".
  const shown = frozen
    ? Array.from(new Set<T>([...items, ...selected.keys()]))
    : items;
  // Frozen and empty is an answer — "this session selected none" — not a
  // catalogue to go and fill, so it never offers the draft row's install link.
  if (shown.length === 0) {
    return frozen ? (
      <p className="px-2 py-1 text-sm text-faint">{i18n.t("common.none")}</p>
    ) : (
      empty
    );
  }
  return (
    <div className="space-y-0.5">
      {shown.map((name) => {
        const checked = selected.has(name);
        return (
          <label
            key={name}
            className={cn(
              "flex items-center gap-2 px-2 py-1 text-sm",
              frozen ? "" : "cursor-pointer hover:bg-raised",
            )}
          >
            <input
              type="checkbox"
              checked={checked}
              disabled={frozen}
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
      {read ? i18n.t("tools.read") : i18n.t("tools.write")}
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
  frozen,
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
  frozen: boolean;
  children: ReactNode;
}) {
  const chosen = names.filter((n) => selected.has(n)).length;
  const all = chosen === names.length;
  return (
    <div data-testid={`tool-group-${group.key}`} data-expanded={expanded}>
      <div
        className={cn(
          "flex items-center gap-2 px-2 py-1",
          frozen ? "" : "hover:bg-raised",
        )}
      >
        <input
          type="checkbox"
          className="shrink-0"
          checked={all}
          disabled={frozen}
          // A group with some of its tools chosen is neither ticked nor empty,
          // and `indeterminate` is a DOM property with no HTML attribute — it
          // can only be set through the element.
          ref={(el) => {
            if (el) el.indeterminate = chosen > 0 && !all;
          }}
          aria-label={i18n.t("tools.selectAllIn", { group: group.label })}
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
  mode: ConfigMode,
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
    legend: i18n.t("channel.tools"),
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
          what={i18n.t("channel.toolCatalogue")}
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
          frozen={mode === "frozen"}
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
  frozen,
}: {
  groups: ToolGroupView[];
  selected: Set<string>;
  onSet: (names: string[], checked: boolean) => void;
  onDefault: () => void;
  frozen: boolean;
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
    { key: "all", label: i18n.t("tools.filterAll") },
    { key: "read", label: i18n.t("tools.filterRead") },
    { key: "write", label: i18n.t("tools.filterWrite") },
  ];

  return (
    <div className="space-y-0.5" data-testid="tools-body" data-filter={filter}>
      <div className="flex flex-wrap items-center gap-1 px-2 pt-0.5 pb-1">
        <span className="segmented">
          {filters.map((f) => (
            <button
              key={f.key}
              type="button"
              className="px-1.5 py-0.5 text-[0.6875rem]"
              // A view switcher, so it says which segment is shown rather than
              // which button is held: `aria-pressed` on three mutually
              // exclusive buttons claims three independent toggles.
              aria-selected={filter === f.key}
              data-testid={`tool-filter-${f.key}`}
              onClick={() => setFilter(f.key)}
            >
              {f.label}
            </button>
          ))}
        </span>
        {/* Selection shortcuts, so they go when nothing can be selected. The
            read/write control beside them stays: it changes what you are
            looking at rather than what is chosen, which is exactly what
            reading a frozen selection wants. */}
        {!frozen && (
          <span className="ml-auto flex gap-1">
            <button
              type="button"
              className="rounded-[var(--radius-chip)] bg-raised px-1.5 py-0.5 text-[0.6875rem] text-dim hover:text-legend"
              data-testid="tool-quick-default"
              onClick={onDefault}
            >
              {i18n.t("common.default")}
            </button>
            <button
              type="button"
              className="rounded-[var(--radius-chip)] bg-raised px-1.5 py-0.5 text-[0.6875rem] text-dim hover:text-legend"
              data-testid="tool-quick-all"
              onClick={() => onSet(visibleNames, true)}
            >
              {i18n.t("tools.selectAll")}
            </button>
            <button
              type="button"
              className="rounded-[var(--radius-chip)] bg-raised px-1.5 py-0.5 text-[0.6875rem] text-dim hover:text-legend"
              data-testid="tool-quick-none"
              onClick={() => onSet(visibleNames, false)}
            >
              {i18n.t("tagFilter.clear")}
            </button>
          </span>
        )}
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
          frozen={frozen}
        >
          {shown(group).map((tool) => {
            const checked = selected.has(tool.name);
            return (
              <label
                key={tool.name}
                className={cn(
                  "flex items-center gap-2 py-1 pr-2 pl-8 text-sm",
                  frozen ? "" : "cursor-pointer hover:bg-raised",
                )}
                data-testid="tool-option"
                data-value={tool.name}
                data-selected={checked}
              >
                <input
                  type="checkbox"
                  checked={checked}
                  disabled={frozen}
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

/** One selected server as a read-only line: `linear`, or `linear (2 tools)`. */
/** One selected server, as a phrase: its name, and how many of its tools are
 * in scope when it was narrowed to some of them. */
function mcpReadout(name: string, tools: string[] | null): string {
  return tools
    ? `${name} (${i18n.t("mcpChannel.toolCount", { count: tools.length })})`
    : name;
}

/** What one server's row reads as: `all`, `2/7`, or nothing when unselected. */
function selectionLabel(
  chosen: string[] | null | undefined,
  total: number | undefined,
): string {
  if (chosen === undefined) return "";
  if (chosen === null) return i18n.t("mcpChannel.allTools");
  return total === undefined
    ? `${chosen.length}`
    : `${chosen.length}/${total}`;
}

/**
 * The MCP popover's contents: a server per row, each opening into its tools.
 *
 * A component rather than inline JSX for the same reason `ToolsBody` is one —
 * which rows are open is state, and `PickerSpec.body` is a plain render
 * function called from a hook.
 */
function McpBody({
  servers,
  selected,
  onSet,
  frozen,
}: {
  servers: McpServerView[];
  selected: Map<string, string[] | null>;
  onSet: (next: Map<string, string[] | null>) => void;
  frozen: boolean;
}) {
  // Open what the row cannot summarise: a narrowed server is the only case
  // where the answer is inside. Same rule as the Tools groups.
  const [expanded, setExpanded] = useState<Set<string>>(
    () =>
      new Set(
        servers
          .filter((s) => (selected.get(s.name) ?? null) !== null)
          .map((s) => s.name),
      ),
  );

  const set = (name: string, tools: string[] | null | undefined) => {
    const next = new Map(selected);
    if (tools === undefined) next.delete(name);
    else next.set(name, tools);
    onSet(next);
  };

  return (
    <div className="space-y-0.5" data-testid="mcp-body">
      {servers.map((server) => (
        <McpServerRow
          key={server.name}
          server={server}
          chosen={selected.has(server.name) ? (selected.get(server.name) ?? null) : undefined}
          expanded={expanded.has(server.name)}
          onToggleExpanded={() =>
            setExpanded((prev) => {
              const next = new Set(prev);
              if (!next.delete(server.name)) next.add(server.name);
              return next;
            })
          }
          onSet={(tools) => set(server.name, tools)}
          frozen={frozen}
        />
      ))}
    </div>
  );
}

/**
 * One server, and its tools when opened.
 *
 * `chosen` is three answers, not two: `undefined` — not selected at all;
 * `null` — the whole server, **including tools it gains later**; a list —
 * only those. Collapsing `null` into "every name I can see today" would
 * silently freeze the selection, so a server that added a tool would never
 * offer it to a preset that asked for all of it.
 *
 * The tools come from what horsie remembered at the last connect, fetched only
 * when this row is opened. Nothing here dials the server: this popover is on
 * the new-session screen, which has no MCP connection at all.
 */
function McpServerRow({
  server,
  chosen,
  expanded,
  onToggleExpanded,
  onSet,
  frozen,
}: {
  server: McpServerView;
  chosen: string[] | null | undefined;
  expanded: boolean;
  onToggleExpanded: () => void;
  onSet: (tools: string[] | null | undefined) => void;
  frozen: boolean;
}) {
  const { data: detail, isPending, isError } = useMcpServer(server.name, expanded);
  const every = (detail?.tools ?? []).map((t) => t.name);
  const selectedAll = chosen === null;

  // Toggling one tool needs the full list, because "all" is stored as `null`:
  // unticking one of an unnarrowed server means "the others", which can only
  // be written out once the others are known.
  const toggleTool = (name: string, checked: boolean) => {
    const current = chosen === null ? every : (chosen ?? []);
    const next = checked
      ? current.filter((t) => t !== name)
      : [...current, name];
    if (next.length === 0) return onSet(undefined);
    // Back to the whole server rather than a list of every name — see above.
    if (every.length > 0 && next.length === every.length) return onSet(null);
    onSet(next);
  };

  return (
    <div data-testid={`mcp-server-${server.name}`} data-expanded={expanded}>
      <div
        className={cn(
          "flex items-center gap-2 px-2 py-1",
          frozen ? "" : "hover:bg-raised",
        )}
      >
        <input
          type="checkbox"
          className="shrink-0"
          checked={chosen !== undefined}
          disabled={frozen}
          // Narrowed is neither on nor off, and `indeterminate` is a DOM
          // property with no HTML attribute.
          ref={(el) => {
            if (el) el.indeterminate = Array.isArray(chosen);
          }}
          aria-label={server.name}
          data-testid={`mcp-server-check-${server.name}`}
          onChange={() => onSet(chosen === undefined ? null : undefined)}
        />
        <button
          type="button"
          className="flex min-w-0 flex-1 items-center gap-2 text-left"
          aria-expanded={expanded}
          data-testid={`mcp-server-expand-${server.name}`}
          onClick={onToggleExpanded}
        >
          <span className="min-w-0 flex-1">
            <span className="block truncate font-mono text-sm text-legend">
              {server.name}
            </span>
            {server.description && (
              <span className="block truncate text-[0.6875rem] leading-tight text-faint">
                {server.description}
              </span>
            )}
          </span>
          <span className="shrink-0 font-mono text-[0.6875rem] text-faint">
            {selectionLabel(chosen, server.toolCount)}
          </span>
          <ChevronDown
            size={13}
            className={cn("shrink-0 text-faint", expanded && "rotate-180")}
            aria-hidden
          />
        </button>
      </div>
      {expanded && isPending && (
        <p className="py-1 pr-2 pl-8 text-[0.6875rem] text-faint">
          {i18n.t("common.loading")}
        </p>
      )}
      {expanded && isError && (
        <p className="py-1 pr-2 pl-8 text-[0.6875rem] text-faint">
          {i18n.t("mcpChannel.toolsUnreadable")}
        </p>
      )}
      {expanded &&
        !isPending &&
        !isError &&
        (every.length === 0 ? (
          <p className="py-1 pr-2 pl-8 text-[0.6875rem] text-faint">
            {i18n.t("mcpChannel.noTools")}
          </p>
        ) : (
          (detail?.tools ?? []).map((tool) => {
            const checked = selectedAll || (chosen ?? []).includes(tool.name);
            return (
              <label
                key={tool.name}
                className={cn(
                  "flex items-center gap-2 py-1 pr-2 pl-8 text-sm",
                  frozen ? "" : "cursor-pointer hover:bg-raised",
                )}
                data-testid="mcp-tool-option"
                data-value={tool.name}
                data-selected={checked}
              >
                <input
                  type="checkbox"
                  checked={checked}
                  disabled={frozen}
                  onChange={() => toggleTool(tool.name, checked)}
                />
                <span className="min-w-0 flex-1">
                  <span className="block truncate font-mono text-sm text-legend">
                    {tool.name}
                  </span>
                  {tool.description && (
                    <span className="block text-[0.6875rem] leading-snug text-faint">
                      {tool.description}
                    </span>
                  )}
                </span>
              </label>
            );
          })
        ))}
    </div>
  );
}

/**
 * What a preset resolved to, for the frozen row's one Agent key.
 *
 * The collapse into a single key is what item 2 asks for — a session created
 * by choosing a preset was configured by one decision — but a collapse that
 * hid the settings would swap one wrong answer for another. So everything the
 * exploded row used to show is here, under the name that was actually chosen.
 *
 * Read off the draft rather than the preset's current definition: a preset is
 * flattened at creation, so an edit since then does not describe this session.
 */
function ResolvedPreset({ draft }: { draft: ConfigDraft }) {
  const names = (s: Set<string>) =>
    s.size === 0 ? i18n.t("common.none") : Array.from(s).join(", ");
  const rows: { key: string; label: string; value: string }[] = [
    { key: "model", label: i18n.t("channel.model"), value: draft.model },
    { key: "skills", label: i18n.t("channel.skills"), value: names(draft.skills) },
    {
      key: "mcp",
      label: i18n.t("channel.mcp"),
      // Not just the server names: a session narrowed to two of a server's
      // seven tools is running something different from one that took all
      // seven, and the collapsed key is the only place that now shows.
      value:
        draft.mcp.size === 0
          ? i18n.t("common.none")
          : Array.from(draft.mcp)
              .map(([name, tools]) => mcpReadout(name, tools))
              .join(", "),
    },
    {
      key: "memory",
      label: i18n.t("channel.memory"),
      value: names(draft.memorySpaces),
    },
    {
      key: "tools",
      label: i18n.t("channel.tools"),
      value:
        draft.tools === null
          ? i18n.t("common.default")
          : draft.tools.size === 0
            ? i18n.t("common.none")
            : i18n.t("channel.selectedCount", { count: draft.tools.size }),
    },
    {
      key: "thinking",
      label: i18n.t("channel.thinking"),
      value: draft.thinkingEffort || i18n.t("common.default"),
    },
  ];
  return (
    <dl className="space-y-1 px-2 py-0.5" data-testid="resolved-preset">
      {rows.map((r) => (
        <div key={r.key} className="flex items-baseline gap-2">
          <dt className="shrink-0 text-[0.6875rem] text-faint">{r.label}</dt>
          <dd
            className="min-w-0 flex-1 text-right font-mono text-[0.8125rem] break-words text-legend"
            data-testid={`resolved-${r.key}`}
          >
            {r.value}
          </dd>
        </div>
      ))}
    </dl>
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
export function useEnvironmentPicker(
  d: EnvironmentChannel,
  mode: ConfigMode = "edit",
): PickerSpec {
  const frozen = mode === "frozen";
  // These specs read the catalogue through the global `t` rather than a
  // per-string hook, so nothing in them would re-render on a language change.
  // Subscribing once here moves every picker built below.
  useTranslation();
  const { data: settings, isError: settingsFailed, error: settingsError } =
    useSettings();
  const { data: repoList, isError: reposFailed, error: reposError } =
    useGithubRepos(d.provisions && d.githubConnected);
  const listed = settings?.vendors ?? [];
  // A frozen session's vendor may have disconnected since it started. It is
  // still where the session runs, so it stays in the list rather than
  // vanishing and leaving the key pointing at nothing.
  const chosenVendor =
    d.environment.kind === "runtime" ? d.environment.vendor : "";
  const activeVendors =
    frozen && chosenVendor && !listed.some((v) => v.name === chosenVendor)
      ? [
          ...listed,
          {
            name: chosenVendor,
            isDefault: false,
            capabilities: { supportsProvisioning: d.provisions },
          },
        ]
      : listed;
  const chosen =
    d.environment.kind === "named"
      ? d.environment.name
      : d.environment.kind === "none"
        ? ""
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
    legend: i18n.t("channel.environment"),
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
              {i18n.t("environment.predefined")}
            </p>
            {d.environments.map((e) => {
              const selected =
                d.environment.kind === "named" && d.environment.name === e.name;
              return (
                <button
                  key={e.name}
                  type="button"
                  className={optionClass(selected, frozen)}
                  data-popover-option
                  data-testid="environment-option"
                  data-value={e.name}
                  data-kind="named"
                  data-selected={selected}
                  aria-pressed={selected}
                  disabled={frozen}
                  onClick={() => {
                    d.setEnvironment({ kind: "named", name: e.name });
                    close();
                  }}
                >
                  <span className="min-w-0 flex-1 truncate font-mono">{e.name}</span>
                  <span className="text-[0.6875rem] text-faint">
                    {e.vendor}
                    {e.repos.length > 0 &&
                      ` · ${i18n.t("environment.repoCount", { count: e.repos.length })}`}
                  </span>
                  {selected && <SelectedMark />}
                </button>
              );
            })}
          </>
        )}
        <p className="px-2 pt-0.5 text-[0.6875rem] tracking-wide text-faint uppercase">
          {i18n.t("environment.runtimes")}
        </p>
        {frozen && activeVendors.length === 0 ? null : settingsFailed ? (
          // Without the config read there is no roster to be empty: saying "no
          // runtime is connected" here would send someone to re-run `horsie
          // connect` for a runtime that is probably already there.
          <ReadError
            what={i18n.t("environment.runtimesLower")}
            error={settingsError}
            testId="environment-read-error"
            className="mx-1 my-0.5"
          />
        ) : activeVendors.length === 0 ? (
          <p className="px-2 py-1.5 text-sm leading-relaxed text-dim">
            <Trans
              i18nKey="environment.noRuntime"
              components={{ cmd: <code className="font-mono text-legend" /> }}
            />
          </p>
        ) : (
          activeVendors.map((v) => {
            const selected =
              d.environment.kind === "runtime" && d.environment.vendor === v.name;
            return (
              <button
                key={v.name}
                type="button"
                className={optionClass(selected, frozen)}
                data-popover-option
                data-testid="environment-option"
                data-value={v.name}
                data-kind="runtime"
                data-selected={selected}
                aria-pressed={selected}
                disabled={frozen}
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
                  <span className="text-[0.6875rem] text-faint">
                    {i18n.t("environment.defaultVendor")}
                  </span>
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
              {i18n.t("environment.repos")}
            </p>
            {named.repos.length === 0 ? (
              <p className="px-2 py-1 text-sm text-faint">
                {i18n.t("common.none")}
              </p>
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
              {i18n.t("environment.repos")}
            </p>
            {!frozen && !d.githubConnected ? (
              <EmptyLink to="/settings/integrations">
                {i18n.t("environment.connectGithub")}
              </EmptyLink>
            ) : (
              checkList({
                frozen,
                // Frozen, the catalogue is left out entirely: what a running
                // session was checked out with is a fact about that session,
                // and padding it with today's GitHub listing would invite the
                // reading that any of them could still be added.
                items: frozen
                  ? []
                  : (repoList?.repos ?? []).map((r) => r.fullName),
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
                    what={i18n.t("environment.reposLower")}
                    error={reposError}
                    testId="environment-repos-read-error"
                    className="mx-1 my-0.5"
                  />
                ) : (
                  <p className="px-2 py-1 text-sm text-dim">
                    {i18n.t("environment.noRepos")}
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
export function useConfigPickers(
  draft: ConfigDraft,
  mode: ConfigMode = "edit",
): PickerSpec[] {
  useTranslation();
  const frozen = mode === "frozen";
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
  const environmentPicker = useEnvironmentPicker(env ?? INERT_ENVIRONMENT, mode);

  const models = settings?.models ?? [];
  const listedMcp = (mcpServers ?? []).filter((s) => s.enabled);
  // A frozen session keeps the servers it actually runs, whether or not they
  // are still enabled — or still configured at all. Without this the row went
  // quiet on exactly the session whose MCP selection you had come to check:
  // the picker lists the live catalogue, and a selection is not in it.
  const missingMcp = frozen
    ? Array.from(draft.mcp.keys())
        .filter((name) => !listedMcp.some((s) => s.name === name))
        .map((name) => ({ name }) as McpServerView)
    : [];
  const enabledMcp = [...listedMcp, ...missingMcp];

  const pickers: PickerSpec[] = [];

  // Leftmost, and first read: it decides which of the others mean anything.
  const running = hasWorkflow(draft) ? draft.workflow : "";
  const agentChannel = hasAgent(draft) ? draft : undefined;
  const selectedAgent = agentChannel?.agent ?? "";
  // Choosing a workflow and choosing a preset are alternatives, so the draft
  // row offers the key only while no preset is picked. Frozen they are not
  // alternatives but two facts about one agent: a workflow step *is* a step of
  // a run and *is* an instance of its own preset, and the row says both.
  if (hasWorkflow(draft) && (frozen ? !!draft.workflow : !selectedAgent)) {
    const d = draft;
    const names =
      frozen && d.workflow && !d.workflows.includes(d.workflow)
        ? [...d.workflows, d.workflow]
        : d.workflows;
    pickers.push({
      key: "workflow",
      legend: i18n.t("channel.workflow"),
      icon: <Workflow size={15} />,
      label: d.workflow || i18n.t("common.none"),
      marked: !!d.workflow,
      width: "w-64",
      testId: "config-workflow",
      body: (close) => (
        <>
          {/* "None" is an alternative to running a workflow, so it belongs to
              choosing one. A run already chose. */}
          {!frozen && (
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
              {i18n.t("common.none")}
              <span className="ml-auto text-[0.6875rem] text-faint">
                {i18n.t("workflowChannel.oneAgent")}
              </span>
              {d.workflow === "" && <SelectedMark />}
            </button>
          )}
          {names.length === 0 ? (
            <EmptyLink to="/workflows">{i18n.t("workflowChannel.define")}</EmptyLink>
          ) : (
            names.map((w) => (
              <button
                key={w}
                type="button"
                className={optionClass(d.workflow === w, frozen)}
                data-popover-option
                data-testid="workflow-option"
                data-value={w}
                data-selected={d.workflow === w}
                aria-pressed={d.workflow === w}
                disabled={frozen}
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
  // so while a workflow is *being chosen* these controls would configure
  // nothing. Once the run exists the reasoning inverts: the agent this row is
  // reading is one step, and that step's resolved settings are precisely what
  // someone opening it wants. Returning early here is what used to make a run
  // report its start step's model as the whole session's.
  if (running && !frozen) return pickers;

  // An agent preset supplies every agent channel itself. Keep it in the Model
  // menu as a mutually-exclusive alternative to configuring a model directly.
  //
  // The same collapse holds once the session exists, which is the whole point:
  // a session created by choosing "reviewer" was configured by one decision,
  // and redrawing it afterwards as six independent channels reports a
  // configuration nobody performed. The preset is what was chosen, so the
  // preset is what the row shows — with everything it resolved to inside it,
  // so nothing is lost to the collapse.
  if (selectedAgent && agentChannel) {
    // A preset can be deleted out from under a running session. Its settings
    // were flattened at creation and still apply, so this is not an error —
    // but a name that no longer resolves should say so rather than read like
    // a link to somewhere.
    const presetGone =
      frozen && !agentChannel.agents.includes(selectedAgent);
    pickers.push({
      key: "model",
      legend: i18n.t("channel.model"),
      icon: <Cpu size={15} />,
      label: presetGone
        ? i18n.t("modelChannel.presetGone", { agent: selectedAgent })
        : selectedAgent,
      marked: true,
      width: "w-72",
      height: frozen ? "max-h-[32rem]" : undefined,
      testId: "config-model",
      warn: settingsFailed || presetGone,
      body: (close) =>
        frozen ? (
          <>
            <p className="px-2 pt-0.5 text-[0.6875rem] tracking-wide text-faint uppercase">
              {i18n.t("modelChannel.agents")}
            </p>
            <button
              type="button"
              className={optionClass(true, true)}
              data-testid="agent-option"
              data-value={selectedAgent}
              data-selected
              aria-pressed
              disabled
            >
              <span className="min-w-0 flex-1 font-mono text-sm text-legend">
                {selectedAgent}
              </span>
              <SelectedMark />
            </button>
            {presetGone && (
              <p className="px-2 py-1 text-sm leading-relaxed text-red-ink">
                {i18n.t("modelChannel.presetGoneHint")}
              </p>
            )}
            <p className="px-2 pt-1.5 text-[0.6875rem] tracking-wide text-faint uppercase">
              {i18n.t("modelChannel.resolved")}
            </p>
            <ResolvedPreset draft={draft} />
          </>
        ) : (
          <>
            <p className="px-2 pt-0.5 text-[0.6875rem] tracking-wide text-faint uppercase">
              {i18n.t("modelChannel.models")}
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
              {i18n.t("modelChannel.agents")}
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
    legend: i18n.t("channel.skills"),
    icon: <Boxes size={15} />,
    label: draft.skills.size
      ? i18n.t("channel.selectedCount", { count: draft.skills.size })
      : i18n.t("common.none"),
    marked: draft.skills.size > 0,
    width: "w-80",
    testId: "config-skills",
    body: () =>
      checkList({
        frozen,
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
            what={i18n.t("channel.skillBundles")}
            error={bundlesError}
            testId="skills-read-error"
            className="mx-1 my-0.5"
          />
        ) : (
          <EmptyLink to="/settings/skills">
            {i18n.t("channel.installSkills")}
          </EmptyLink>
        ),
      }),
  });

  pickers.push({
    key: "mcp",
    legend: i18n.t("channel.mcp"),
    icon: <Plug size={15} />,
    label: draft.mcp.size
      ? i18n.t("channel.selectedCount", { count: draft.mcp.size })
      : i18n.t("common.none"),
    marked: draft.mcp.size > 0,
    width: "w-96",
    // Two lines per server and its tools underneath. The 18rem default shows
    // one open server and hides the rest behind a scrollbar.
    height: "max-h-[32rem]",
    testId: "config-mcp",
    body: () =>
      mcpFailed ? (
        <ReadError
          what={i18n.t("channel.mcpServers")}
          error={mcpError}
          testId="mcp-read-error"
          className="mx-1 my-0.5"
        />
      ) : enabledMcp.length === 0 ? (
        // Frozen and empty is an answer — this session selected none — not a
        // catalogue to go and fill.
        frozen ? (
          <p className="px-2 py-1 text-sm text-faint">{i18n.t("common.none")}</p>
        ) : (
          <EmptyLink to="/settings/integrations">
            {i18n.t("channel.addMcp")}
          </EmptyLink>
        )
      ) : (
        <McpBody
          servers={enabledMcp}
          selected={draft.mcp}
          onSet={draft.setMcp}
          frozen={frozen}
        />
      ),
  });

  // Not gated on `provisions`: the server owns memory, so it works on vendors
  // that cannot provision a workspace too.
  pickers.push({
    key: "memory",
    legend: i18n.t("channel.memory"),
    icon: <Brain size={15} />,
    label: draft.memorySpaces.size
      ? i18n.t("channel.selectedCount", { count: draft.memorySpaces.size })
      : i18n.t("common.none"),
    marked: draft.memorySpaces.size > 0,
    width: "w-72",
    testId: "config-memory",
    body: () =>
      checkList({
        frozen,
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
            what={i18n.t("channel.memorySpaces")}
            error={memoryError}
            testId="memory-read-error"
            className="mx-1 my-0.5"
          />
        ) : (
          <EmptyLink to="/settings/memory">
            {i18n.t("channel.createMemory")}
          </EmptyLink>
        ),
      }),
  });

  // Between the toolbox channels and the model: it is the widest of them, and
  // the one whose answer changes what the others are for.
  pickers.push(toolsPicker(draft, toolCatalog, toolsFailed, toolsError, mode));

  // A model alias can be renamed or deleted out from under a live session, and
  // the next turn then fails `no provider registered for model '…'`. The row
  // used to show the dead alias exactly as it shows a live one, so the only
  // symptom was a turn that stopped working. It cannot be repaired here —
  // there is no API to repoint an existing session — but it can at least stop
  // being a surprise. Only once the settings have actually loaded: an unknown
  // answer must not be reported as "missing".
  const modelGone =
    frozen && !!settings && !models.some((m) => m.alias === draft.model);
  pickers.push({
    key: "model",
    legend: i18n.t("channel.model"),
    icon: <Cpu size={15} />,
    label: modelGone
      ? i18n.t("channel.modelMissing", { model: draft.model })
      : (models.find((m) => m.alias === draft.model)?.alias ??
        (frozen ? draft.model : i18n.t("channel.select"))),
    marked: !!draft.model,
    width: "w-72",
    testId: "config-model",
    warn: settingsFailed || modelGone,
    body: (close) =>
      settingsFailed ? (
        <ReadError
          what={i18n.t("channel.models")}
          error={settingsError}
          testId="model-read-error"
          className="mx-1 my-0.5"
        />
      ) : modelGone ? (
        <div className="space-y-1.5 px-1 py-0.5">
          <p className="font-mono text-[0.8125rem] break-words text-legend">
            {draft.model}
          </p>
          <p className="text-sm leading-relaxed text-red-ink">
            {i18n.t("channel.modelGoneHint")}
          </p>
        </div>
      ) : models.length === 0 ? (
        <EmptyLink to="/settings/models">
          {i18n.t("channel.noModels")}
        </EmptyLink>
      ) : agentChannel && !frozen ? (
        <>
          <p className="px-2 pt-0.5 text-[0.6875rem] tracking-wide text-faint uppercase">
            {i18n.t("modelChannel.models")}
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
            {i18n.t("modelChannel.agents")}
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
            className={optionClass(draft.model === m.alias, frozen)}
            data-popover-option
            data-testid="model-option"
            data-value={m.alias}
            data-selected={draft.model === m.alias}
            aria-pressed={draft.model === m.alias}
            disabled={frozen}
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
  // A frozen session's effort may name a model whose card offers no menu — or
  // whose card is no longer readable at all. The value is still what the
  // session runs under, so it keeps its key; only a session that genuinely has
  // no effort *and* no menu has nothing to say.
  const efforts =
    frozen && draft.thinkingEffort && !draft.thinkingEfforts.includes(draft.thinkingEffort)
      ? [...draft.thinkingEfforts, draft.thinkingEffort]
      : draft.thinkingEfforts;
  if (efforts.length > 0) {
    pickers.push({
      key: "thinking",
      legend: i18n.t("channel.thinking"),
      icon: <Lightbulb size={15} />,
      label: draft.thinkingEffort || i18n.t("common.default"),
      marked: !!draft.thinkingEffort,
      width: "w-52",
      testId: "config-thinking",
      body: () => (
        <div className="space-y-0.5">
          <label
            className={cn(
              optionClass(draft.thinkingEffort === "", frozen),
              frozen ? "" : "cursor-pointer",
            )}
            data-selected={draft.thinkingEffort === ""}
          >
            <input
              type="radio"
              name="thinking-effort"
              checked={draft.thinkingEffort === ""}
              disabled={frozen}
              onChange={() => draft.setThinkingEffort("")}
            />
            <span className="min-w-0 flex-1 truncate">
              {draft.modelDefaultThinkingEffort
                ? i18n.t("channel.defaultEffort", {
                    effort: draft.modelDefaultThinkingEffort,
                  })
                : i18n.t("channel.defaultLower")}
            </span>
            {draft.thinkingEffort === "" && <SelectedMark />}
          </label>
          {efforts.map((e) => (
            <label
              key={e}
              className={cn(
                optionClass(draft.thinkingEffort === e, frozen),
                frozen ? "" : "cursor-pointer",
              )}
              data-selected={draft.thinkingEffort === e}
            >
              <input
                type="radio"
                name="thinking-effort"
                checked={draft.thinkingEffort === e}
                disabled={frozen}
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
