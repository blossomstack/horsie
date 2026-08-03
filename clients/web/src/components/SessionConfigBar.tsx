import { Boxes, Brain, Cpu, FolderGit2, Lightbulb, Plug, Server } from "lucide-react";
import type { ReactNode } from "react";
import { Link } from "react-router-dom";
import type { SessionDetail } from "../api/types";
import { useGithubRepos } from "../hooks/useGithub";
import { useMcpServers } from "../hooks/useMcp";
import { useMemorySpaces } from "../hooks/useMemory";
import { usePlugins } from "../hooks/usePlugins";
import { useSettings } from "../hooks/useSettings";
import type { ConfigDraft } from "../hooks/useSessionDraft";
import { basename } from "../lib/format";
import { PopoverMenu } from "./PopoverMenu";

type Props =
  | { mode: "draft"; draft: ConfigDraft }
  | { mode: "locked"; detail: SessionDetail };

/** A settled channel on the header strip: engraved legend over its value.
 * Locked config is a description of the session, not a control, so it reads
 * as an instrument label rather than a button you might be able to press. */
function Readout({
  legend,
  children,
  testId,
}: {
  legend: string;
  children: ReactNode;
  testId?: string;
}) {
  return (
    <span className="flex min-w-0 flex-col gap-0.5" data-testid={testId}>
      <span className="legend leading-none">{legend}</span>
      <span className="font-mono text-[11px] leading-snug break-words text-legend">
        {children}
      </span>
    </span>
  );
}

export function SessionConfigBar(props: Props) {
  if (props.mode === "locked") {
    // A stacked list, not a strip: locked config now lives inside a narrow
    // info popover rather than spanning the header.
    return (
      <div
        className="space-y-2.5"
        data-testid="session-config-bar"
        data-mode="locked"
      >
        <LockedControls detail={props.detail} />
      </div>
    );
  }
  return (
    <div className="mx-auto w-full max-w-[54rem] px-4 sm:px-6">
      <div
        className="flex flex-wrap items-center gap-1.5 pb-2"
        data-testid="session-config-bar"
        data-mode="draft"
      >
        <DraftControls draft={props.draft} />
      </div>
    </div>
  );
}

function DraftControls({ draft }: { draft: ConfigDraft }) {
  const { data: settings } = useSettings();
  const { data: bundles } = usePlugins();
  const { data: mcpServers } = useMcpServers();
  const { data: memorySpaces } = useMemorySpaces();
  const models = settings?.models ?? [];
  const activeVendors = (settings?.vendors ?? []);
  const enabledMcp = (mcpServers ?? []).filter((s) => s.enabled);
  const { data: repoList } = useGithubRepos(draft.provisions && draft.githubConnected);

  const modelLabel =
    models.find((m) => m.alias === draft.model)?.alias ?? "Select";

  return (
    <>
      {/* Runtime */}
      <PopoverMenu
        testId="config-runtime"
        legend="Runtime"
        icon={<Server size={13} />}
        label={draft.vendor || "Select"}
        width="w-56"
      >
        {(close) =>
          activeVendors.map((v) => (
            <button
              key={v.name}
              type="button"
              className="flex w-full items-center gap-2 rounded-[var(--radius-chip)] px-2 py-1.5 text-left text-sm hover:bg-raised"
              data-testid="runtime-option"
              data-value={v.name}
              onClick={() => {
                draft.setVendor(v.name);
                close();
              }}
            >
              <span className="font-mono">{v.name}</span>
              {v.isDefault && (
                <span className="text-[11px] text-faint">default</span>
              )}
            </button>
          ))
        }
      </PopoverMenu>

      {/* Remote-only workspace controls */}
      {draft.provisions && (
        <>
          <PopoverMenu
            testId="config-repos"
            legend="Repos"
            icon={<FolderGit2 size={13} />}
            label={draft.repos.size ? `${draft.repos.size} selected` : "None"}
            width="w-80"
          >
            {() =>
              !draft.githubConnected ? (
                <Link
                  to="/settings"
                  className="block px-2 py-1.5 text-sm text-dim hover:text-legend"
                >
                  Connect GitHub in Settings to pick repos
                </Link>
              ) : (
                <div className="space-y-0.5">
                  {(repoList?.repos ?? []).map((r) => {
                    const checked = draft.repos.has(r.fullName);
                    return (
                      <label
                        key={r.fullName}
                        className="flex cursor-pointer items-center gap-2 px-2 py-1 text-sm hover:bg-raised"
                      >
                        <input
                          type="checkbox"
                          checked={checked}
                          onChange={() => {
                            const next = new Map(draft.repos);
                            if (checked) next.delete(r.fullName);
                            else next.set(r.fullName, "");
                            draft.setRepos(next);
                          }}
                        />
                        <span className="min-w-0 flex-1 truncate font-mono">
                          {r.fullName}
                        </span>
                      </label>
                    );
                  })}
                  {repoList && repoList.repos.length === 0 && (
                    <p className="px-2 py-1 text-sm text-dim">
                      No repos visible to the app installation.
                    </p>
                  )}
                </div>
              )
            }
          </PopoverMenu>

          <PopoverMenu
            testId="config-skills"
            legend="Skills"
            icon={<Boxes size={13} />}
            label={draft.skills.size ? `${draft.skills.size} selected` : "None"}
            width="w-80"
          >
            {() =>
              (bundles ?? []).length === 0 ? (
                <Link
                  to="/skills"
                  className="block px-2 py-1.5 text-sm text-dim hover:text-legend"
                >
                  Install skill bundles in the Skills page
                </Link>
              ) : (
                <div className="space-y-0.5">
                  {(bundles ?? []).map((b) => {
                    const checked = draft.skills.has(b.name);
                    return (
                      <label
                        key={b.name}
                        className="flex cursor-pointer items-center gap-2 px-2 py-1 text-sm hover:bg-raised"
                      >
                        <input
                          type="checkbox"
                          checked={checked}
                          onChange={() => {
                            const next = new Set(draft.skills);
                            if (checked) next.delete(b.name);
                            else next.add(b.name);
                            draft.setSkills(next);
                          }}
                        />
                        <span className="min-w-0 flex-1 truncate font-mono">
                          {b.name}
                        </span>
                      </label>
                    );
                  })}
                </div>
              )
            }
          </PopoverMenu>

          <PopoverMenu
            testId="config-mcp"
            legend="MCP"
            icon={<Plug size={13} />}
            label={draft.mcp.size ? `${draft.mcp.size} selected` : "None"}
            width="w-72"
          >
            {() =>
              enabledMcp.length === 0 ? (
                <Link
                  to="/settings"
                  className="block px-2 py-1.5 text-sm text-dim hover:text-legend"
                >
                  Add MCP servers in Settings
                </Link>
              ) : (
                <div className="space-y-0.5">
                  {enabledMcp.map((s) => {
                    const checked = draft.mcp.has(s.name);
                    return (
                      <label
                        key={s.name}
                        className="flex cursor-pointer items-center gap-2 px-2 py-1 text-sm hover:bg-raised"
                      >
                        <input
                          type="checkbox"
                          checked={checked}
                          onChange={() => {
                            const next = new Set(draft.mcp);
                            if (checked) next.delete(s.name);
                            else next.add(s.name);
                            draft.setMcp(next);
                          }}
                        />
                        <span className="min-w-0 flex-1 truncate font-mono">
                          {s.name}
                        </span>
                      </label>
                    );
                  })}
                </div>
              )
            }
          </PopoverMenu>
        </>
      )}

      {/* Memory — not gated on `provisions`: the server owns the data, so this
          works on vendors that can't provision a workspace too. */}
      <PopoverMenu
        testId="config-memory"
        legend="Memory"
        icon={<Brain size={13} />}
        label={draft.memorySpaces.size ? `${draft.memorySpaces.size} selected` : "None"}
        width="w-72"
      >
        {() =>
          (memorySpaces ?? []).length === 0 ? (
            <Link
              to="/memory"
              className="block px-2 py-1.5 text-sm text-dim hover:text-legend"
            >
              Create a memory space first
            </Link>
          ) : (
            <div className="space-y-0.5">
              {(memorySpaces ?? []).map((sp) => {
                const checked = draft.memorySpaces.has(sp.name);
                return (
                  <label
                    key={sp.name}
                    className="flex cursor-pointer items-center gap-2 px-2 py-1 text-sm hover:bg-raised"
                  >
                    <input
                      type="checkbox"
                      checked={checked}
                      onChange={() => {
                        const next = new Set(draft.memorySpaces);
                        if (checked) next.delete(sp.name);
                        else next.add(sp.name);
                        draft.setMemorySpaces(next);
                      }}
                    />
                    <span className="min-w-0 flex-1 truncate font-mono">
                      {sp.name}
                    </span>
                  </label>
                );
              })}
            </div>
          )
        }
      </PopoverMenu>

      {/* Thinking effort — only for models that offer a menu. The value is
          fixed for the session's lifetime: switching effort mid-conversation
          invalidates the provider's prompt cache. */}
      {draft.thinkingEfforts.length > 0 && (
        <PopoverMenu
          testId="config-thinking"
          legend="Thinking"
          icon={<Lightbulb size={13} />}
          label={draft.thinkingEffort || "Default"}
          width="w-52"
        >
          {() => (
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
          )}
        </PopoverMenu>
      )}

      {/* Model — right-aligned; editable now, structured to unlock on existing
          sessions later. */}
      <div className="ml-auto">
        <PopoverMenu
          testId="config-model"
          legend="Model"
          icon={<Cpu size={13} />}
          label={modelLabel}
          width="w-72"
        >
          {(close) =>
            models.length === 0 ? (
              <Link
                to="/settings"
                className="block px-2 py-1.5 text-sm text-dim hover:text-legend"
              >
                No models configured — add one in Settings
              </Link>
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
                  <span className="text-[11px] text-faint">{m.modelId}</span>
                </button>
              ))
            )
          }
        </PopoverMenu>
      </div>
    </>
  );
}

function LockedControls({ detail }: { detail: SessionDetail }) {
  return (
    <>
      <Readout legend="Model" testId="config-model">
        {detail.model}
      </Readout>
      <Readout legend="Runtime" testId="config-runtime">
        {detail.vendor}
      </Readout>
      {detail.repos.length > 0 && (
        <Readout legend="Repos" testId="config-repos">
          {detail.repos.map((r) => basename(r)).join(", ")}
        </Readout>
      )}
      {detail.plugins.length > 0 && (
        <Readout legend="Skills" testId="config-skills">
          {detail.plugins.join(", ")}
        </Readout>
      )}
      {detail.mcpServers.length > 0 && (
        <Readout legend="MCP" testId="config-mcp">
          {detail.mcpServers.join(", ")}
        </Readout>
      )}
      {detail.memorySpaces.length > 0 && (
        <Readout legend="Memory" testId="config-memory">
          {detail.memorySpaces.join(", ")}
        </Readout>
      )}
      {detail.thinkingEffort && (
        <Readout legend="Thinking" testId="config-thinking">
          {detail.thinkingEffort}
        </Readout>
      )}
    </>
  );
}
