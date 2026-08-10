import {
  Boxes,
  Brain,
  Cpu,
  Lightbulb,
  Plug,
  Server,
  Workflow,
  Check,
} from "lucide-react";
import type { ReactNode } from "react";
import { Link } from "react-router-dom";
import { useGithubRepos } from "../hooks/useGithub";
import { useMcpServers } from "../hooks/useMcp";
import { useMemorySpaces } from "../hooks/useMemory";
import { usePlugins } from "../hooks/usePlugins";
import { useSettings } from "../hooks/useSettings";
import type { SessionDetail } from "../api/types";
import { cn } from "../lib/cn";
import { basename } from "../lib/format";
import type {
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
 * The one Environment picker, as its own hook.
 *
 * Standalone because four surfaces need it and only one of them — the session
 * config bar — wants the rest of `useConfigPickers`. The routine form renders
 * exactly this spec as a labelled field, which is what makes the two lists
 * identical rather than merely similar.
 */
export function useEnvironmentPicker(d: EnvironmentChannel): PickerSpec {
  const { data: settings } = useSettings();
  const { data: repoList } = useGithubRepos(d.provisions && d.githubConnected);
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
    // actionable.
    warn: activeVendors.length === 0 && d.environments.length === 0,
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
        {activeVendors.length === 0 ? (
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
          <div className="border-t pt-1.5" data-testid="environment-summary">
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
          <div className="border-t pt-1.5" data-testid="environment-repos">
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
                empty: (
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
  const { data: settings } = useSettings();
  const { data: bundles } = usePlugins();
  const { data: mcpServers } = useMcpServers();
  const { data: memorySpaces } = useMemorySpaces();
  const env = hasEnvironment(draft) ? draft : undefined;
  // Called unconditionally with an inert channel when the draft has none: a
  // hook cannot be conditional, and an agent-preset form has no environment.
  const environmentPicker = useEnvironmentPicker(env ?? INERT_ENVIRONMENT);

  const models = settings?.models ?? [];
  const enabledMcp = (mcpServers ?? []).filter((s) => s.enabled);

  const pickers: PickerSpec[] = [];

  // Leftmost, and first read: it decides which of the others mean anything.
  const running = hasWorkflow(draft) ? draft.workflow : "";
  if (hasWorkflow(draft)) {
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
        empty: (
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
        empty: (
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
        empty: <EmptyLink to="/settings/memory">Create a memory space first</EmptyLink>,
      }),
  });

  pickers.push({
    key: "model",
    legend: "Model",
    icon: <Cpu size={15} />,
    label: models.find((m) => m.alias === draft.model)?.alias ?? "Select",
    marked: !!draft.model,
    width: "w-72",
    testId: "config-model",
    body: (close) =>
      models.length === 0 ? (
        <EmptyLink to="/settings/models">
          No models configured — add one in Settings
        </EmptyLink>
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
  // lifetime: changing effort mid-conversation invalidates the prompt cache.
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
export function useLockedChannels(detail: SessionDetail): PickerSpec[] {
  const { data: settings } = useSettings();
  // A model alias can be renamed or deleted out from under a live session, and
  // the next turn then fails `no provider registered for model '…'`. The row
  // used to show the dead alias exactly as it shows a live one, so the only
  // symptom was a turn that stopped working. It cannot be repaired here —
  // there is no API to repoint an existing session — but it can at least stop
  // being a surprise.
  const modelGone =
    !!settings && !settings.models.some((m) => m.alias === detail.model);

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

  const channels: PickerSpec[] = [
    environment,
    ...optional("skills", "Skills", <Boxes size={15} />, "w-80", detail.plugins),
    ...optional("mcp", "MCP", <Plug size={15} />, "w-72", detail.mcpServers),
    ...optional("memory", "Memory", <Brain size={15} />, "w-72", detail.memorySpaces),
    {
      key: "model",
      legend: "Model",
      icon: <Cpu size={15} />,
      label: modelGone ? `${detail.model} — missing` : detail.model,
      marked: true,
      warn: modelGone,
      width: "w-72",
      testId: "config-model",
      body: modelGone
        ? () => (
            <div className="space-y-1.5 px-1 py-0.5">
              <p className="font-mono text-[0.8125rem] break-words text-legend">
                {detail.model}
              </p>
              <p className="text-sm leading-relaxed text-red-ink">
                This model is no longer configured, so the next turn in this
                session will fail. Restore the alias in Settings → Models, or
                start a new session.
              </p>
            </div>
          )
        : readout([detail.model]),
    },
  ];

  if (detail.thinkingEffort) {
    channels.push({
      key: "thinking",
      legend: "Thinking",
      icon: <Lightbulb size={15} />,
      label: detail.thinkingEffort,
      marked: true,
      width: "w-52",
      testId: "config-thinking",
      body: readout([detail.thinkingEffort]),
    });
  }

  return channels;
}
