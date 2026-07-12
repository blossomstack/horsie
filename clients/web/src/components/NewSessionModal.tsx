import * as Dialog from "@radix-ui/react-dialog";
import { ChevronRight, Loader2, Settings2, X } from "lucide-react";
import { useEffect, useState, type ReactNode } from "react";
import { Link } from "react-router-dom";
import { ApiRequestError } from "../api/client";
import type { CreateSessionRequest, RepoConfig } from "../api/types";
import { cn } from "../lib/cn";
import { useGithubRepos, useGithubStatus } from "../hooks/useGithub";
import { useMcpServers } from "../hooks/useMcp";
import { usePlugins as usePluginBundles } from "../hooks/usePlugins";
import { useCreateSession } from "../hooks/useSessions";
import { useSettings } from "../hooks/useSettings";

/** Where a new session's workspace comes from. */
type WorkspaceSource = "dir" | "repos";

export function NewSessionModal({
  open,
  onOpenChange,
  onCreated,
}: {
  open: boolean;
  onOpenChange: (v: boolean) => void;
  onCreated: (id: string) => void;
}) {
  const create = useCreateSession();
  const { data: settings } = useSettings();
  const { data: bundles } = usePluginBundles();
  const models = settings?.models ?? [];
  const activeVendors = (settings?.vendors ?? []).filter((v) => v.active);
  const showVendor = activeVendors.length > 1;

  const [name, setName] = useState("");
  const [model, setModel] = useState("");
  const [workdir, setWorkdir] = useState("");
  const [vendor, setVendor] = useState("");
  const [systemPrompt, setSystemPrompt] = useState("");
  const [allowAskUser, setAllowAskUser] = useState(true);
  const [usePlugins, setUsePlugins] = useState(false);
  const [selectedPlugins, setSelectedPlugins] = useState<Set<string>>(
    new Set(),
  );
  const [mcpSelected, setMcpSelected] = useState<string[]>([]);
  const [advanced, setAdvanced] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const { data: mcpServers } = useMcpServers();
  const enabledMcp = (mcpServers ?? []).filter((s) => s.enabled);

  // Workspace source: a local directory, or a set of GitHub repos to clone
  // (fullName → ref; "" = default branch).
  const [source, setSource] = useState<WorkspaceSource>("dir");
  const [selected, setSelected] = useState<Map<string, string>>(new Map());
  const [repoFilter, setRepoFilter] = useState("");
  const { data: ghStatus } = useGithubStatus();
  const {
    data: repoList,
    isLoading: reposLoading,
    refetch: refetchRepos,
  } = useGithubRepos(open && source === "repos" && !!ghStatus?.connected);

  const reset = () => {
    setName("");
    setModel("");
    setWorkdir("");
    setVendor("");
    setSystemPrompt("");
    setAllowAskUser(true);
    setUsePlugins(false);
    setSelectedPlugins(new Set());
    setMcpSelected([]);
    setAdvanced(false);
    setError(null);
    setSource("dir");
    setSelected(new Map());
    setRepoFilter("");
  };

  // Clear the form on close so a cancelled draft never carries into the next
  // open (and a since-deleted model can't linger as a stale selection).
  useEffect(() => {
    if (!open) reset();
  }, [open]);

  // While open, keep model/vendor pointing at a choice that still exists —
  // reseed from server config when the current selection is empty or stale.
  useEffect(() => {
    if (!open || !settings) return;
    if (!models.some((m) => m.alias === model))
      setModel(models[0]?.alias ?? "");
    if (!activeVendors.some((v) => v.name === vendor))
      setVendor(settings.defaultVendor);
  }, [open, models, activeVendors, settings, model, vendor]);

  // On open, pre-select the bundles the server marks as default.
  useEffect(() => {
    if (!open || !bundles) return;
    setSelectedPlugins(
      new Set(bundles.filter((b) => b.enabledDefault).map((b) => b.name)),
    );
  }, [open, bundles]);

  const submit = async () => {
    setError(null);
    const wd = workdir.trim();
    if (!model.trim()) return setError("Select a model.");
    if (source === "dir" && !wd)
      return setError("A workspace directory is required.");

    const repos: RepoConfig[] =
      source === "repos"
        ? Array.from(selected.entries()).map(([fullName, ref]) => ({
            url: `https://github.com/${fullName}`,
            gitRef: ref.trim() || undefined,
          }))
        : [];

    const body: CreateSessionRequest = {
      name: name.trim() || undefined,
      agent: {
        model: model.trim(),
        systemPrompt: systemPrompt.trim() || undefined,
        allowAskUser,
        usePlugins,
        mcpServers: mcpSelected.length ? mcpSelected : undefined,
      },
      workdirs: source === "dir" ? [wd] : [],
      repos: source === "repos" ? repos : undefined,
      vendor: vendor.trim() || undefined,
      // Selected skill bundles; empty → server uses the default-enabled set.
      plugins: selectedPlugins.size ? Array.from(selectedPlugins) : undefined,
    };

    try {
      const res = await create.mutateAsync(body);
      reset();
      onOpenChange(false);
      onCreated(res.session.id);
    } catch (e) {
      setError(
        e instanceof ApiRequestError ? e.message : "Failed to create session.",
      );
    }
  };

  const noModels = !!settings && models.length === 0;

  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Overlay className="fixed inset-0 z-40 bg-black/50 backdrop-blur-sm animate-fade-in" />
        <Dialog.Content
          className="fixed left-1/2 top-1/2 z-50 w-[min(30rem,calc(100vw-2rem))] -translate-x-1/2 -translate-y-1/2 rounded-[var(--radius-xl)] border p-5 shadow-2xl animate-rise focus:outline-none"
          style={{ background: "var(--surface)" }}
        >
          <div className="mb-4 flex items-center justify-between">
            <Dialog.Title className="text-base font-semibold text-text">
              New session
            </Dialog.Title>
            <Dialog.Close className="btn-icon" aria-label="Close">
              <X size={18} />
            </Dialog.Close>
          </div>

          <div className="space-y-3.5">
            <Field label="Name" hint="optional">
              <input
                className="input"
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="Untitled session"
              />
            </Field>

            <Field label="Model">
              {noModels ? (
                <Link
                  to="/settings"
                  onClick={() => onOpenChange(false)}
                  className="flex items-center gap-1.5 rounded-[var(--radius)] border border-dashed px-3 py-2 text-sm text-muted transition-colors hover:text-text"
                >
                  <Settings2 size={14} />
                  No models configured — add one in Settings
                </Link>
              ) : (
                <select
                  className="input font-mono"
                  value={model}
                  onChange={(e) => setModel(e.target.value)}
                >
                  {models.map((m) => (
                    <option key={m.alias} value={m.alias}>
                      {m.alias} — {m.modelId}
                    </option>
                  ))}
                </select>
              )}
            </Field>

            <Field label="Workspace">
              <div className="mb-2 flex gap-1">
                {(["dir", "repos"] as const).map((s) => (
                  <button
                    key={s}
                    type="button"
                    className={cn(
                      "chip cursor-pointer",
                      source === s && "border-accent text-text",
                    )}
                    onClick={() => setSource(s)}
                  >
                    {s === "dir" ? "Local directory" : "GitHub repos"}
                  </button>
                ))}
              </div>

              {source === "dir" ? (
                <input
                  className="input font-mono"
                  value={workdir}
                  onChange={(e) => setWorkdir(e.target.value)}
                  placeholder="/Users/you/project"
                />
              ) : !ghStatus?.connected ? (
                <Link
                  to="/settings"
                  onClick={() => onOpenChange(false)}
                  className="flex items-center gap-1.5 rounded-[var(--radius)] border border-dashed px-3 py-2 text-sm text-muted transition-colors hover:text-text"
                >
                  <Settings2 size={14} />
                  Connect GitHub in Settings to pick repos
                </Link>
              ) : (
                <div className="space-y-2">
                  <div className="flex gap-2">
                    <input
                      className="input"
                      value={repoFilter}
                      onChange={(e) => setRepoFilter(e.target.value)}
                      placeholder="Filter repos…"
                    />
                    <button
                      type="button"
                      className="btn-outline shrink-0"
                      onClick={() => refetchRepos()}
                    >
                      Refresh
                    </button>
                  </div>
                  <div className="max-h-40 space-y-1 overflow-y-auto rounded-[var(--radius)] border p-1">
                    {reposLoading && (
                      <p className="px-2 py-1 text-sm text-muted">
                        Loading repos…
                      </p>
                    )}
                    {(repoList?.repos ?? [])
                      .filter((r) =>
                        r.fullName
                          .toLowerCase()
                          .includes(repoFilter.toLowerCase()),
                      )
                      .map((r) => {
                        const checked = selected.has(r.fullName);
                        return (
                          <div
                            key={r.fullName}
                            className="flex items-center gap-2 px-2 py-1"
                          >
                            <input
                              type="checkbox"
                              checked={checked}
                              onChange={() =>
                                setSelected((m) => {
                                  const next = new Map(m);
                                  if (checked) next.delete(r.fullName);
                                  else next.set(r.fullName, "");
                                  return next;
                                })
                              }
                            />
                            <span className="min-w-0 flex-1 truncate font-mono text-sm">
                              {r.fullName}
                            </span>
                            {checked && (
                              <input
                                className="input w-28 py-0.5 text-xs"
                                value={selected.get(r.fullName) ?? ""}
                                onChange={(e) =>
                                  setSelected((m) =>
                                    new Map(m).set(r.fullName, e.target.value),
                                  )
                                }
                                placeholder={r.defaultBranch}
                              />
                            )}
                          </div>
                        );
                      })}
                    {repoList && repoList.repos.length === 0 && (
                      <p className="px-2 py-1 text-sm text-muted">
                        No repos visible to the app installation.
                      </p>
                    )}
                  </div>
                  <p className="text-[11px] text-faint">
                    {selected.size === 0
                      ? "No repos selected — the session starts with an empty workspace."
                      : `${selected.size} repo${selected.size > 1 ? "s" : ""} selected.`}
                  </p>
                </div>
              )}
            </Field>

            <button
              className="flex items-center gap-1 text-xs font-medium text-muted transition-colors hover:text-text"
              onClick={() => setAdvanced((a) => !a)}
            >
              <ChevronRight
                size={13}
                className={cn("transition-transform", advanced && "rotate-90")}
              />
              Advanced
            </button>

            {advanced && (
              <div className="space-y-3.5 border-t pt-3.5">
                {showVendor && (
                  <Field label="Runtime vendor">
                    <select
                      className="input font-mono"
                      value={vendor}
                      onChange={(e) => setVendor(e.target.value)}
                    >
                      {activeVendors.map((v) => (
                        <option key={v.name} value={v.name}>
                          {v.name}
                          {v.isDefault ? " (default)" : ""}
                        </option>
                      ))}
                    </select>
                  </Field>
                )}
                <Field label="System prompt" hint="optional">
                  <textarea
                    className="input min-h-[68px] resize-y"
                    value={systemPrompt}
                    onChange={(e) => setSystemPrompt(e.target.value)}
                    placeholder="Override the default system prompt…"
                  />
                </Field>
                <Toggle
                  label="Allow the agent to ask you questions"
                  checked={allowAskUser}
                  onChange={setAllowAskUser}
                />
                <Toggle
                  label="Enable plugins"
                  checked={usePlugins}
                  onChange={setUsePlugins}
                />
                {bundles && bundles.length > 0 && (
                  <Field
                    label="Skills"
                    hint="bundles provisioned for this session"
                  >
                    <div className="max-h-40 space-y-1 overflow-y-auto rounded-[var(--radius)] border p-1">
                      {bundles.map((b) => {
                        const checked = selectedPlugins.has(b.name);
                        return (
                          <label
                            key={b.name}
                            className="flex cursor-pointer items-start gap-2 px-2 py-1"
                          >
                            <input
                              type="checkbox"
                              className="mt-0.5"
                              checked={checked}
                              onChange={() =>
                                setSelectedPlugins((s) => {
                                  const next = new Set(s);
                                  if (checked) next.delete(b.name);
                                  else next.add(b.name);
                                  return next;
                                })
                              }
                            />
                            <span className="min-w-0 flex-1">
                              <span className="font-mono text-sm text-text">
                                {b.name}
                              </span>
                              {b.hasHooks && (
                                <span className="ml-1.5 rounded bg-surface-3 px-1 py-0.5 text-[10px] text-muted">
                                  hooks
                                </span>
                              )}
                              {b.description && (
                                <span className="block truncate text-[11px] text-faint">
                                  {b.description}
                                </span>
                              )}
                            </span>
                          </label>
                        );
                      })}
                    </div>
                  </Field>
                )}
                {enabledMcp.length > 0 && (
                  <Field label="MCP servers" hint="optional">
                    <div className="space-y-1 rounded-[var(--radius)] border p-1.5">
                      {enabledMcp.map((s) => {
                        const checked = mcpSelected.includes(s.name);
                        return (
                          <label
                            key={s.name}
                            className="flex cursor-pointer items-center gap-2 px-1.5 py-1 text-sm"
                          >
                            <input
                              type="checkbox"
                              checked={checked}
                              onChange={() =>
                                setMcpSelected((cur) =>
                                  checked
                                    ? cur.filter((n) => n !== s.name)
                                    : [...cur, s.name],
                                )
                              }
                            />
                            <span className="min-w-0 flex-1 truncate font-mono">
                              {s.name}
                            </span>
                            <span className="text-[11px] text-faint">
                              {s.toolCount ?? 0} tools
                            </span>
                          </label>
                        );
                      })}
                    </div>
                  </Field>
                )}
              </div>
            )}

            {error && (
              <div className="rounded-[var(--radius)] border border-error/40 bg-error-soft px-3 py-2 text-sm text-error">
                {error}
              </div>
            )}
          </div>

          <div className="mt-5 flex justify-end gap-2">
            <Dialog.Close className="btn-ghost">Cancel</Dialog.Close>
            <button
              className="btn-primary"
              onClick={submit}
              disabled={create.isPending || noModels}
            >
              {create.isPending && (
                <Loader2 size={15} className="animate-spin" />
              )}
              Create session
            </button>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

function Field({
  label,
  hint,
  children,
}: {
  label: string;
  hint?: string;
  children: ReactNode;
}) {
  return (
    <label className="block">
      <div className="mb-1 flex items-baseline gap-2">
        <span className="text-xs font-semibold text-text">{label}</span>
        {hint && <span className="text-[11px] text-faint">{hint}</span>}
      </div>
      {children}
    </label>
  );
}

function Toggle({
  label,
  checked,
  onChange,
}: {
  label: string;
  checked: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <label className="flex cursor-pointer items-center gap-2.5 text-sm text-text">
      <button
        type="button"
        role="switch"
        aria-checked={checked}
        onClick={() => onChange(!checked)}
        className={cn(
          "relative h-5 w-9 shrink-0 rounded-full transition-colors",
          checked ? "bg-accent" : "bg-surface-3",
        )}
      >
        <span
          className={cn(
            "absolute top-0.5 h-4 w-4 rounded-full bg-white transition-transform",
            checked ? "translate-x-4" : "translate-x-0.5",
          )}
        />
      </button>
      {label}
    </label>
  );
}
