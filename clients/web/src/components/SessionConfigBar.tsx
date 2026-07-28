import { Boxes, Brain, Cpu, FolderGit2, Lightbulb, Plug, Server } from "lucide-react";
import type { ReactNode } from "react";
import { Link } from "react-router-dom";
import type { SessionDetail } from "../api/types";
import { useGithubRepos } from "../hooks/useGithub";
import { useMcpServers } from "../hooks/useMcp";
import { useMemorySpaces } from "../hooks/useMemory";
import { usePlugins } from "../hooks/usePlugins";
import { useSettings } from "../hooks/useSettings";
import type { SessionDraft } from "../hooks/useSessionDraft";
import { basename } from "../lib/format";
import { PopoverMenu } from "./PopoverMenu";

type Props =
  | { mode: "draft"; draft: SessionDraft }
  | { mode: "locked"; detail: SessionDetail };

/** A non-interactive labelled chip used in locked mode. */
function LockedChip({
  icon,
  children,
  testId,
}: {
  icon: ReactNode;
  children: ReactNode;
  testId?: string;
}) {
  return (
    <span
      className="flex items-center gap-1.5 rounded-[var(--radius)] border px-2.5 py-1.5 text-xs font-medium text-muted"
      data-testid={testId}
    >
      {icon}
      <span className="max-w-[14rem] truncate">{children}</span>
    </span>
  );
}

export function SessionConfigBar(props: Props) {
  return (
    <div className="mx-auto w-full max-w-3xl px-4">
      <div
        className="flex flex-wrap items-center gap-2 pb-2"
        data-testid="session-config-bar"
        data-mode={props.mode}
      >
        {props.mode === "draft" ? (
          <DraftControls draft={props.draft} />
        ) : (
          <LockedControls detail={props.detail} />
        )}
      </div>
    </div>
  );
}

function DraftControls({ draft }: { draft: SessionDraft }) {
  const { data: settings } = useSettings();
  const { data: bundles } = usePlugins();
  const { data: mcpServers } = useMcpServers();
  const { data: memorySpaces } = useMemorySpaces();
  const models = settings?.models ?? [];
  const activeVendors = (settings?.vendors ?? []).filter((v) => v.active);
  const enabledMcp = (mcpServers ?? []).filter((s) => s.enabled);
  const { data: repoList } = useGithubRepos(draft.provisions && draft.githubConnected);

  const modelLabel =
    models.find((m) => m.alias === draft.model)?.alias ?? "Select model";

  return (
    <>
      {/* Runtime */}
      <PopoverMenu
        testId="config-runtime"
        icon={<Server size={13} />}
        label={draft.vendor || "Runtime"}
        width="w-56"
      >
        {(close) =>
          activeVendors.map((v) => (
            <button
              key={v.name}
              type="button"
              className="flex w-full items-center gap-2 rounded-[var(--radius-sm)] px-2 py-1.5 text-left text-sm hover:bg-surface-2"
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
            icon={<FolderGit2 size={13} />}
            label={
              draft.repos.size ? `${draft.repos.size} repo(s)` : "Repos"
            }
            width="w-80"
          >
            {() =>
              !draft.githubConnected ? (
                <Link
                  to="/settings"
                  className="block px-2 py-1.5 text-sm text-muted hover:text-text"
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
                        className="flex cursor-pointer items-center gap-2 px-2 py-1 text-sm hover:bg-surface-2"
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
                    <p className="px-2 py-1 text-sm text-muted">
                      No repos visible to the app installation.
                    </p>
                  )}
                </div>
              )
            }
          </PopoverMenu>

          <PopoverMenu
            testId="config-skills"
            icon={<Boxes size={13} />}
            label={draft.skills.size ? `${draft.skills.size} skill(s)` : "Skills"}
            width="w-80"
          >
            {() =>
              (bundles ?? []).length === 0 ? (
                <Link
                  to="/skills"
                  className="block px-2 py-1.5 text-sm text-muted hover:text-text"
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
                        className="flex cursor-pointer items-center gap-2 px-2 py-1 text-sm hover:bg-surface-2"
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
            icon={<Plug size={13} />}
            label={draft.mcp.size ? `${draft.mcp.size} MCP` : "MCP"}
            width="w-72"
          >
            {() =>
              enabledMcp.length === 0 ? (
                <Link
                  to="/settings"
                  className="block px-2 py-1.5 text-sm text-muted hover:text-text"
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
                        className="flex cursor-pointer items-center gap-2 px-2 py-1 text-sm hover:bg-surface-2"
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
        icon={<Brain size={13} />}
        label={
          draft.memorySpaces.size
            ? `${draft.memorySpaces.size} memory`
            : "Memory"
        }
        width="w-72"
      >
        {() =>
          (memorySpaces ?? []).length === 0 ? (
            <Link
              to="/memory"
              className="block px-2 py-1.5 text-sm text-muted hover:text-text"
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
                    className="flex cursor-pointer items-center gap-2 px-2 py-1 text-sm hover:bg-surface-2"
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
          icon={<Lightbulb size={13} />}
          label={draft.thinkingEffort || "Thinking"}
          width="w-52"
        >
          {() => (
            <div className="space-y-0.5">
              <label className="flex cursor-pointer items-center gap-2 px-2 py-1 text-sm hover:bg-surface-2">
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
                  className="flex cursor-pointer items-center gap-2 px-2 py-1 text-sm hover:bg-surface-2"
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
          icon={<Cpu size={13} />}
          label={modelLabel}
          width="w-72"
        >
          {(close) =>
            models.length === 0 ? (
              <Link
                to="/settings"
                className="block px-2 py-1.5 text-sm text-muted hover:text-text"
              >
                No models configured — add one in Settings
              </Link>
            ) : (
              models.map((m) => (
                <button
                  key={m.alias}
                  type="button"
                  className="flex w-full flex-col rounded-[var(--radius-sm)] px-2 py-1.5 text-left hover:bg-surface-2"
                  data-testid="model-option"
                  data-value={m.alias}
                  onClick={() => {
                    draft.setModel(m.alias);
                    close();
                  }}
                >
                  <span className="font-mono text-sm text-text">{m.alias}</span>
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
  const hasWorkspace =
    detail.repos.length > 0 ||
    detail.plugins.length > 0 ||
    detail.mcpServers.length > 0;
  return (
    <>
      <LockedChip icon={<Server size={13} />} testId="config-runtime">
        {detail.vendor}
      </LockedChip>
      {hasWorkspace && (
        <>
          {detail.repos.length > 0 && (
            <LockedChip icon={<FolderGit2 size={13} />} testId="config-repos">
              {detail.repos.map((r) => basename(r)).join(", ")}
            </LockedChip>
          )}
          {detail.plugins.length > 0 && (
            <LockedChip icon={<Boxes size={13} />} testId="config-skills">
              {detail.plugins.join(", ")}
            </LockedChip>
          )}
          {detail.mcpServers.length > 0 && (
            <LockedChip icon={<Plug size={13} />} testId="config-mcp">
              {detail.mcpServers.join(", ")}
            </LockedChip>
          )}
        </>
      )}
      {detail.memorySpaces.length > 0 && (
        <LockedChip icon={<Brain size={13} />} testId="config-memory">
          {detail.memorySpaces.join(", ")}
        </LockedChip>
      )}
      <div className="ml-auto">
        <LockedChip icon={<Cpu size={13} />} testId="config-model">
          {detail.model}
        </LockedChip>
      </div>
    </>
  );
}
