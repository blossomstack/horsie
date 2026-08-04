import { Boxes, Brain, Cpu, FolderGit2, Lightbulb, Plug, Server } from "lucide-react";
import type { ReactNode } from "react";
import { Link } from "react-router-dom";
import { useGithubRepos } from "../hooks/useGithub";
import { useMcpServers } from "../hooks/useMcp";
import { useMemorySpaces } from "../hooks/useMemory";
import { usePlugins } from "../hooks/usePlugins";
import { useSettings } from "../hooks/useSettings";
import type { SessionDetail } from "../api/types";
import { basename } from "../lib/format";
import type { ConfigDraft, RuntimeChannel } from "../hooks/useSessionDraft";

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

/** A session draft carries a runtime channel; an agent-preset draft does not.
 * The draft's own shape is the signal, so neither surface has to pass a flag
 * saying which one it is. */
function hasRuntime(draft: ConfigDraft): draft is ConfigDraft & RuntimeChannel {
  return "setVendor" in draft;
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
  const { data: repoList } = useGithubRepos(draft.provisions && draft.githubConnected);

  const models = settings?.models ?? [];
  const activeVendors = settings?.vendors ?? [];
  const enabledMcp = (mcpServers ?? []).filter((s) => s.enabled);

  const pickers: PickerSpec[] = [];

  if (hasRuntime(draft)) {
    const d = draft;
    pickers.push({
      key: "runtime",
      legend: "Runtime",
      icon: <Server size={15} />,
      label: d.vendor || "Select",
      marked: !!d.vendor,
      // The new-session page used to carry a whole roster panel answering "is my
      // laptop connected?". The answer is one bit, and this is where it is
      // actionable.
      warn: activeVendors.length === 0,
      width: "w-56",
      testId: "config-runtime",
      body: (close) =>
        activeVendors.length === 0 ? (
          <p className="px-2 py-1.5 text-sm leading-relaxed text-dim">
            No runtime is connected, so a session can’t run a turn yet. Run{" "}
            <code className="font-mono text-legend">horsie connect</code> on the
            machine holding your code.
          </p>
        ) : (
          activeVendors.map((v) => (
            <button
              key={v.name}
              type="button"
              className="flex w-full items-center gap-2 rounded-[var(--radius-chip)] px-2 py-1.5 text-left text-sm hover:bg-raised"
              data-testid="runtime-option"
              data-value={v.name}
              onClick={() => {
                d.setVendor(v.name);
                close();
              }}
            >
              <span className="font-mono">{v.name}</span>
              {v.isDefault && <span className="text-[0.6875rem] text-faint">default</span>}
            </button>
          ))
        ),
    });
  }

  // Repos are the one channel a non-provisioning vendor genuinely cannot
  // honour: it runs over a fixed, user-owned directory, so there is nothing to
  // check a repo out into. Skills and MCP are not workspace channels — MCP is
  // composed server-side and never reaches the runtime at all — so they are
  // offered unconditionally, below.
  if (draft.provisions) {
    pickers.push({
      key: "repos",
      legend: "Repos",
      icon: <FolderGit2 size={15} />,
      label: draft.repos.size ? `${draft.repos.size} selected` : "None",
      marked: draft.repos.size > 0,
      width: "w-80",
      testId: "config-repos",
      body: () =>
        !draft.githubConnected ? (
          <EmptyLink to="/settings/integrations">
            Connect GitHub in Settings to pick repos
          </EmptyLink>
        ) : (
          checkList({
            items: (repoList?.repos ?? []).map((r) => r.fullName),
            selected: draft.repos,
            onToggle: (name, checked) => {
              const next = new Map(draft.repos);
              if (checked) next.delete(name);
              else next.set(name, "");
              draft.setRepos(next);
            },
            empty: (
              <p className="px-2 py-1 text-sm text-dim">
                No repos visible to the app installation.
              </p>
            ),
          })
        ),
    });
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
            className="flex w-full flex-col rounded-[var(--radius-chip)] px-2 py-1.5 text-left hover:bg-raised"
            data-testid="model-option"
            data-value={m.alias}
            onClick={() => {
              draft.setModel(m.alias);
              close();
            }}
          >
            <span className="font-mono text-sm text-legend">{m.alias}</span>
            <span className="text-[0.6875rem] text-faint">{m.modelId}</span>
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
          <label className="flex cursor-pointer items-center gap-2 px-2 py-1 text-sm hover:bg-raised">
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
          </label>
          {draft.thinkingEfforts.map((e) => (
            <label
              key={e}
              className="flex cursor-pointer items-center gap-2 px-2 py-1 text-sm hover:bg-raised"
            >
              <input
                type="radio"
                name="thinking-effort"
                checked={draft.thinkingEffort === e}
                onChange={() => draft.setThinkingEffort(e)}
              />
              <span className="min-w-0 flex-1 truncate font-mono">{e}</span>
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
  const value = (items: string[]) =>
    items.length ? items.join(", ") : "None";

  const readout = (label: string, items: string[]) => () => (
    <div className="space-y-1.5 px-1 py-0.5">
      <p className="legend">{label}</p>
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
            body: readout(legend, items),
          },
        ];

  const channels: PickerSpec[] = [
    {
      key: "runtime",
      legend: "Runtime",
      icon: <Server size={15} />,
      label: detail.vendor,
      marked: true,
      width: "w-56",
      testId: "config-runtime",
      body: readout("Runtime", [detail.vendor]),
    },
    ...optional("repos", "Repos", <FolderGit2 size={15} />, "w-80", detail.repos.map(basename)),
    ...optional("skills", "Skills", <Boxes size={15} />, "w-80", detail.plugins),
    ...optional("mcp", "MCP", <Plug size={15} />, "w-72", detail.mcpServers),
    ...optional("memory", "Memory", <Brain size={15} />, "w-72", detail.memorySpaces),
    {
      key: "model",
      legend: "Model",
      icon: <Cpu size={15} />,
      label: detail.model,
      marked: true,
      width: "w-72",
      testId: "config-model",
      body: readout("Model", [detail.model]),
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
      body: readout("Thinking effort", [detail.thinkingEffort]),
    });
  }

  return channels;
}
