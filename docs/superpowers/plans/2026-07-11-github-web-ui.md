# GitHub Web UI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A "GitHub" section in Settings (App config + Connect/Disconnect + status) and a repo picker in the new-session modal (0..N repos, optional ref), wired to the Plan 2 endpoints.

**Architecture:** Follows the existing web client patterns exactly: fluorite-generated TS types (committed), `src/api/client.ts` typed fetch wrappers, TanStack Query hooks, Settings page `Section` components with write-only secret fields (`has*` placeholders), Radix dialog form in `NewSessionModal`.

**Tech Stack:** React + Vite + Tailwind in `clients/web` (bun lockfile — use `bun install` / `bun run …`). Type generation via the `fluorite` CLI (`bun run generate-types`; install with `cargo install fluorite` if missing).

**Prerequisites:** Plans 1–2 merged (server exposes `/api/github/*`, `CreateSessionRequest.repos`, `SessionDetail.repos`).

**Reference spec:** `docs/superpowers/specs/2026-07-11-github-repos-design.md` (§5). All paths below are relative to `clients/web/` unless noted.

## Global Constraints

- No new UI libraries; reuse `.input`, `.btn-*`, `.chip`, `Field`/`Toggle`/`Section` patterns already in the codebase.
- Generated types are committed; never hand-edit files under `src/generated/`.
- Verification for every task: `bun run typecheck` (and `bun run build` at the end). There is no JS test runner in this package — typecheck + build + manual smoke via `bun run dev` are the gates.
- Commit messages: short subject, no AI attribution. Same feature branch off `main`.

---

### Task 1: Generated types + API client + hooks

**Files:**
- Modify: `package.json` (add `../../models/fluorite/github.fl` to the `generate-types` inputs)
- Generate: `src/generated/github/*` (+ regenerated `session_api`/`session` picking up `repos`)
- Modify: `src/api/client.ts`
- Create: `src/hooks/useGithub.ts`

**Interfaces:**
- Consumes: server endpoints from Plan 2; `RepoConfig`, `SessionDetail.repos` from Plan 1.
- Produces: `api.github.{status, authUrl, appConfig, saveAppConfig, disconnect, repos, branches}`; hooks `useGithubStatus()`, `useGithubAppConfig()`, `useSaveGithubAppConfig()`, `useGithubDisconnect()`, `useGithubRepos(enabled)`. Tasks 2–3 consume these names.

- [ ] **Step 1: Regenerate types**

In `package.json`, extend the `generate-types` script's `-i` list with `../../models/fluorite/github.fl`. Then:

Run: `cd clients/web && bun run generate-types && bun run typecheck`
Expected: `src/generated/github/` appears (gitHubStatus.ts, gitHubAppConfigView.ts, gitHubAppConfigInput.ts, gitHubRepo.ts, gitHubRepoList.ts, gitHubBranch.ts, gitHubBranchList.ts, index.ts); `createSessionRequest.ts` gains `repos?: RepoConfig[]`; typecheck passes.

- [ ] **Step 2: API client endpoints**

In `src/api/client.ts`, import the new types and add a `github` group next to `config` (same `request<T>` helper):

```typescript
import type {
  GitHubAppConfigInput,
  GitHubAppConfigView,
  GitHubBranchList,
  GitHubRepoList,
  GitHubStatus,
} from "../generated/github";

export const api = {
  // … existing health/sessions/config …
  github: {
    status: () => request<GitHubStatus>("/github/status"),
    /** Browser navigation target (not fetch) — starts the OAuth flow. */
    authUrl: () => `${BASE}/github/auth`,
    appConfig: () => request<GitHubAppConfigView>("/github/app-config"),
    saveAppConfig: (body: GitHubAppConfigInput) =>
      request<GitHubAppConfigView>("/github/app-config", {
        method: "PUT",
        body: JSON.stringify(body),
      }),
    disconnect: () =>
      request<void>("/github/disconnect", { method: "DELETE" }),
    repos: (refresh = false) =>
      request<GitHubRepoList>(`/github/repos${refresh ? "?refresh=1" : ""}`),
    branches: (repo: string) =>
      request<GitHubBranchList>(
        `/github/repos/branches?repo=${encodeURIComponent(repo)}`,
      ),
  },
};
```

(Match the file's actual `api` object shape — if endpoints are standalone consts, follow that instead.)

- [ ] **Step 3: Hooks**

Create `src/hooks/useGithub.ts` (mirroring `useSettings.ts`):

```typescript
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import type { GitHubAppConfigInput } from "../generated/github";

export const githubKeys = {
  status: ["github", "status"] as const,
  appConfig: ["github", "app-config"] as const,
  repos: ["github", "repos"] as const,
};

export function useGithubStatus() {
  return useQuery({ queryKey: githubKeys.status, queryFn: api.github.status });
}

export function useGithubAppConfig() {
  return useQuery({
    queryKey: githubKeys.appConfig,
    queryFn: api.github.appConfig,
  });
}

export function useSaveGithubAppConfig() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (body: GitHubAppConfigInput) => api.github.saveAppConfig(body),
    onSuccess: (view) => {
      qc.setQueryData(githubKeys.appConfig, view);
      qc.invalidateQueries({ queryKey: githubKeys.status });
    },
  });
}

export function useGithubDisconnect() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: () => api.github.disconnect(),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: githubKeys.status });
      qc.invalidateQueries({ queryKey: githubKeys.repos });
    },
  });
}

/** Repo list for the picker; only fetched while the picker is open/connected. */
export function useGithubRepos(enabled: boolean) {
  return useQuery({
    queryKey: githubKeys.repos,
    queryFn: () => api.github.repos(),
    enabled,
    staleTime: 5 * 60 * 1000, // server caches 5 min too
  });
}
```

- [ ] **Step 4: Verify + commit**

Run: `cd clients/web && bun run typecheck`
Expected: clean.

```bash
git add -A
git commit -m "web: github api client, hooks, generated types"
```

---

### Task 2: Settings → GitHub section

**Files:**
- Modify: `src/pages/SettingsPage.tsx`

**Interfaces:**
- Consumes: Task 1 hooks; the page's existing `Section`, `RowLabel`/`TextField`, save conventions.
- Produces: a self-contained `<GithubSection />` rendered between the vendors section and the Server info section. It manages its own state and saves independently of the page-level Save button (it talks to `/api/github/app-config`, not `/api/config`).

- [ ] **Step 1: Add the section component**

In `SettingsPage.tsx`, add (adapting `TextField` usage to the file's actual helper — it exists for the provider/velos rows):

```tsx
function GithubSection() {
  const { data: status } = useGithubStatus();
  const { data: cfg } = useGithubAppConfig();
  const save = useSaveGithubAppConfig();
  const disconnect = useGithubDisconnect();
  const [params, setParams] = useSearchParams();

  const [clientId, setClientId] = useState("");
  const [clientSecret, setClientSecret] = useState("");
  const [appId, setAppId] = useState("");
  const [privateKey, setPrivateKey] = useState("");
  const [dirty, setDirty] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Seed the form from the stored config (once per load).
  useEffect(() => {
    if (!cfg || dirty) return;
    setClientId(cfg.clientId ?? "");
    setAppId(cfg.appId != null ? String(cfg.appId) : "");
  }, [cfg, dirty]);

  // Surface the OAuth callback outcome, then strip the params.
  const connected = params.get("github_connected");
  const oauthError = params.get("github_error");
  useEffect(() => {
    if (connected || oauthError) {
      const next = new URLSearchParams(params);
      next.delete("github_connected");
      next.delete("github_error");
      setParams(next, { replace: true });
      if (oauthError) setError(oauthError);
    }
  }, [connected, oauthError, params, setParams]);

  const submit = async () => {
    setError(null);
    try {
      await save.mutateAsync({
        clientId: clientId.trim(),
        clientSecret: clientSecret === "" ? undefined : clientSecret,
        appId: appId.trim() === "" ? undefined : Number(appId),
        privateKey: privateKey === "" ? undefined : privateKey,
      });
      setClientSecret("");
      setPrivateKey("");
      setDirty(false);
    } catch (e) {
      setError(e instanceof ApiRequestError ? e.message : "Failed to save.");
    }
  };

  return (
    <Section
      title="GitHub"
      desc="Connect a GitHub App so sessions can clone your repositories."
    >
      <div className="space-y-3">
        {status?.connected ? (
          <div className="flex items-center justify-between rounded-[var(--radius)] border px-3 py-2 text-sm">
            <span>
              Connected as <span className="font-mono">@{status.login}</span>
            </span>
            <button
              className="btn-ghost text-error"
              onClick={() => disconnect.mutate()}
            >
              Disconnect
            </button>
          </div>
        ) : (
          <div className="flex items-center justify-between rounded-[var(--radius)] border border-dashed px-3 py-2 text-sm text-muted">
            <span>
              {status?.appConfigured
                ? "App configured — connect your account."
                : "Configure the GitHub App below, then connect."}
            </span>
            <a
              className="btn-outline"
              href={api.github.authUrl()}
              aria-disabled={!status?.appConfigured}
              onClick={(e) => {
                if (!status?.appConfigured) e.preventDefault();
              }}
            >
              Connect GitHub
            </a>
          </div>
        )}

        <div className="grid grid-cols-2 gap-3">
          <TextField
            label="Client ID"
            value={clientId}
            onChange={(v) => {
              setClientId(v);
              setDirty(true);
            }}
          />
          <TextField
            label="Client secret"
            type="password"
            value={clientSecret}
            onChange={(v) => {
              setClientSecret(v);
              setDirty(true);
            }}
            placeholder={
              cfg?.hasClientSecret ? "•••• stored — blank keeps it" : "not set"
            }
          />
          <TextField
            label="App ID"
            value={appId}
            onChange={(v) => {
              setAppId(v);
              setDirty(true);
            }}
          />
          <TextField
            label="Private key (PEM or base64)"
            type="password"
            value={privateKey}
            onChange={(v) => {
              setPrivateKey(v);
              setDirty(true);
            }}
            placeholder={
              cfg?.hasPrivateKey ? "•••• stored — blank keeps it" : "not set"
            }
          />
        </div>

        {error && (
          <div className="rounded-[var(--radius)] border border-error/40 bg-error-soft px-3 py-2 text-sm text-error">
            {error}
          </div>
        )}

        <div className="flex justify-end">
          <button
            className="btn-primary"
            onClick={submit}
            disabled={!dirty || save.isPending}
          >
            Save GitHub settings
          </button>
        </div>
      </div>
    </Section>
  );
}
```

Imports to add at the top of the file: `useSearchParams` from `react-router-dom`, `api`/`ApiRequestError` from `../api/client`, and the four hooks from `../hooks/useGithub`. If the page's `TextField` helper lacks a `type`/`placeholder` prop, extend it (it already supports `type="password"` for the provider key rows — reuse as-is).

Render `<GithubSection />` in the page layout between the velos/vendors section and the Server info section.

- [ ] **Step 2: Verify + manual smoke**

Run: `cd clients/web && bun run typecheck`
Expected: clean.

Manual smoke (optional but recommended): `bun run dev` against a running `horsie serve`; save an app config, confirm redacted placeholders on reload, click Connect (with a real App) or confirm the disabled state without one.

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "web: github settings section"
```

---

### Task 3: New-session repo picker

**Files:**
- Modify: `src/components/NewSessionModal.tsx`

**Interfaces:**
- Consumes: `useGithubStatus`, `useGithubRepos` (Task 1); `RepoConfig` from `src/generated/session_api`.
- Produces: the modal submits either `workdirs: [dir]` (Local directory mode) or `repos: [...]` + `workdirs: []` (GitHub repos mode).

- [ ] **Step 1: Add source-mode state + picker UI**

In `NewSessionModal.tsx`:

State additions:

```tsx
type WorkspaceSource = "dir" | "repos";
const [source, setSource] = useState<WorkspaceSource>("dir");
const [selected, setSelected] = useState<Map<string, string>>(new Map()); // fullName → ref ("" = default branch)
const [repoFilter, setRepoFilter] = useState("");
const { data: ghStatus } = useGithubStatus();
const { data: repoList, isLoading: reposLoading, refetch: refetchRepos } =
  useGithubRepos(open && source === "repos" && !!ghStatus?.connected);
```

Reset `source`/`selected`/`repoFilter` in the modal's existing `reset()`.

Replace the single "Workspace directory" `Field` with a mode switch plus per-mode body:

```tsx
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
          <p className="px-2 py-1 text-sm text-muted">Loading repos…</p>
        )}
        {(repoList?.repos ?? [])
          .filter((r) =>
            r.fullName.toLowerCase().includes(repoFilter.toLowerCase()),
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
```

- [ ] **Step 2: Submit-path changes**

In `submit()`, replace the workdir validation + body:

```tsx
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
  },
  workdirs: source === "dir" ? [wd] : [],
  repos: source === "repos" ? repos : undefined,
  vendor: vendor.trim() || undefined,
};
```

Import `RepoConfig` from `../generated/session_api`.

- [ ] **Step 3: Verify + build + commit**

Run: `cd clients/web && bun run typecheck && bun run build`
Expected: both clean.

Manual smoke: `bun run dev` — create a repos-mode session against a server with GitHub connected (or zero repos selected: expect an empty managed workspace session).

```bash
git add -A
git commit -m "web: repo picker in new-session modal"
```
