# Session Config Toolbar Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the new-session modal with an inline, chat-first flow: a config toolbar above the input box where the user picks runtime (and, for a remote runtime, repos/skills/MCP); the session is created only when the first message is sent; existing sessions show their config read-only in the same toolbar.

**Architecture:** Client-only draft — opening "new chat" (the `/` route) creates nothing server-side; config lives in React state. The first message does `POST /api/sessions` then sends, then navigates to `/sessions/:id`. A single `SessionConfigBar` component renders the toolbar in `draft` (editable) or `locked` (read-only) mode. The only backend change is extending `SessionDetail` to echo the full config so an existing session renders read-only. Resource allocation is already deferred to first message because no server session exists until then.

**Tech Stack:** Rust (axum session server, fluorite build-time codegen), React 19 + Vite + TanStack Query + React Router v7, Tailwind v4, Playwright e2e. Types generated from `models/fluorite/*.fl`.

## Global Constraints

- **No AI attribution** in any commit message (no `Co-Authored-By: Claude`, no "Generated with"). Keep commit subjects short.
- **Fluorite types are generated, never hand-edited.** Rust wire types regenerate on `cargo build` from `models/fluorite/*.fl` (via `models/build.rs`). TS types regenerate via `bun run generate-types`. When a `.fl` changes, regenerate **both** `clients/ts/src/generated` (`cd clients/ts && bun run generate-types`) and `clients/web/src/generated` (`cd clients/web && bun run generate-types`) and commit the output. CI's ts-drift job checks `clients/ts`.
- **Rust gate:** `make check` (fmt + `clippy --workspace --all-targets --all-features -D warnings` + `cargo test --workspace --all-features`). Formatting is validated by **CI nightly rustfmt** — do not trust local nightly `cargo fmt` (it emits false repo-wide diffs); rely on `cargo fmt` stable + CI.
- **Web gate:** `cd clients/web && bun run typecheck && bun run build`. Behavioral coverage is Playwright e2e (`bun run test:e2e`) — there is **no** React unit-test harness; do not invent one. Frontend tasks verify via typecheck/build; behavior is asserted in the e2e task.
- **Capability, not name:** never branch on the literal vendor name. "Remote/provisioning" ⇔ `vendorView.capabilities.supportsProvisioning === true`. "Local" ⇔ `false`.

---

## Task 1: Backend — `SessionDetail` echoes full session config

**Files:**
- Modify: `models/fluorite/session.fl:40-52` (add fields to `SessionDetail`)
- Modify: `server/src/http/handlers.rs:182-203` (`get_session` populates them)
- Test: `tests/tests/session_server_e2e.rs` (new test near `repos_session_creates_and_reports_repos`, ~line 913)
- Regenerate: `clients/ts/src/generated`, `clients/web/src/generated`

**Interfaces:**
- Produces: wire `SessionDetail` gains `plugins: string[]`, `mcpServers: string[]`, `usePlugins: boolean` (TS) — consumed by `SessionConfigBar` (Task 3/5).

- [ ] **Step 1: Write the failing Rust e2e test**

Add to `tests/tests/session_server_e2e.rs` (mirrors `repos_session_creates_and_reports_repos`; reads detail while Provisioning, so it needs no plugin/MCP service wired and no wait for Idle):

```rust
#[tokio::test]
async fn session_detail_echoes_full_config() {
    let mock = MockLlmServer::builder().build().await;
    let tmp = tempfile::tempdir().unwrap();
    let vendor = Arc::new(MockVendor::new());
    let server = start_server(tmp.path(), vendor.clone(), &mock.url()).await;
    let client = reqwest::Client::new();

    let body = serde_json::json!({
        "agent": {"model": "mock", "use_plugins": true, "mcpServers": ["gh"]},
        "vendor": "mock"
    });
    let res = client
        .post(format!("http://{}/api/sessions", server.addr))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 201);
    let id = res.json::<serde_json::Value>().await.unwrap()["session"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let detail: serde_json::Value = client
        .get(format!("http://{}/api/sessions/{id}", server.addr))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detail["session"]["usePlugins"], serde_json::json!(true));
    assert_eq!(detail["session"]["mcpServers"], serde_json::json!(["gh"]));
    assert_eq!(detail["session"]["plugins"], serde_json::json!([]));

    server.shutdown().await;
}
```

- [ ] **Step 2: Run it to confirm it fails to compile / fails**

Run: `cargo test -p tests --test session_server_e2e session_detail_echoes_full_config`
Expected: compile error — the wire `SessionDetail` has no `usePlugins`/`mcpServers`/`plugins` (and the handler will not construct them yet).

- [ ] **Step 3: Add the fields to the fluorite schema**

In `models/fluorite/session.fl`, extend `struct SessionDetail` (after `repos`):

```
struct SessionDetail {
    id: String,
    name: Option<String>,
    status: SessionStatusKind,
    created_at: u64,
    last_error: Option<String>,
    /// The question the agent is awaiting an answer to (status AwaitingInput).
    pending_question: Option<String>,
    model: String,
    vendor: String,
    /// Clone URLs of the session's provisioned repos (empty when none).
    repos: Vec<String>,
    /// Selected skill-bundle names (empty when none).
    plugins: Vec<String>,
    /// Enabled MCP server names (empty when none).
    mcp_servers: Vec<String>,
    /// Whether the runtime's plugin/skill machinery is enabled for this session.
    use_plugins: bool,
}
```

- [ ] **Step 4: Populate the fields in the handler**

In `server/src/http/handlers.rs`, `get_session`, extend the `SessionDetail { … }` literal (after the `repos:` field, ~line 202) with:

```rust
        plugins: rec.spec.plugins.clone(),
        mcp_servers: rec.spec.agent.mcp_servers.clone(),
        use_plugins: rec.spec.agent.use_plugins.unwrap_or(false),
```

(`SessionSpec.plugins: Vec<String>` — `server/src/sessions/spec.rs:83`; storage `AgentSettings.mcp_servers: Vec<String>` — `spec.rs:42`; `use_plugins: Option<bool>` — `spec.rs:35`.)

- [ ] **Step 5: Run the test to confirm it passes**

Run: `cargo test -p tests --test session_server_e2e session_detail_echoes_full_config`
Expected: PASS (build regenerates the Rust `SessionDetail` from the `.fl`).

- [ ] **Step 6: Regenerate TS types for both clients**

Run:
```bash
cd clients/ts && bun run generate-types && cd -
cd clients/web && bun run generate-types && cd -
```
Confirm `clients/web/src/generated/session/sessionDetail.ts` now has `plugins: string[]`, `mcpServers: string[]`, `usePlugins: boolean`.

- [ ] **Step 7: Commit**

```bash
git add models/fluorite/session.fl server/src/http/handlers.rs tests/tests/session_server_e2e.rs clients/ts/src/generated clients/web/src/generated
git commit -m "server: SessionDetail echoes plugins, mcp servers, use_plugins"
```

---

## Task 2: Frontend primitives — PopoverMenu, draft hook, start-session hook, Composer gating

Pure additive scaffolding used by later tasks. No user-visible change yet. Verifies via `bun run typecheck`.

**Files:**
- Create: `clients/web/src/components/PopoverMenu.tsx`
- Create: `clients/web/src/hooks/useSessionDraft.ts`
- Modify: `clients/web/src/hooks/useSessions.ts` (add `useStartSession`)
- Modify: `clients/web/src/components/Composer.tsx` (add `blockedReason` prop)

**Interfaces:**
- Produces: `PopoverMenu` (button + dropdown panel opening upward); `useSessionDraft(): SessionDraft`; `useStartSession()` mutation `{ body, text } → Promise<string>` (session id); `Composer` accepts `blockedReason?: string | null`.
- Consumes: `useSettings`, `useGithubStatus`, `usePlugins`, `useMcpServers`, `api.sessions.create/send`, `applyOptimisticTitle`.

- [ ] **Step 1: Create `PopoverMenu.tsx`**

Mirrors the existing outside-click popover pattern in `SettingsMenu.tsx`, but the panel opens **upward** (`bottom-full`) because the toolbar sits just above the input near the bottom of the viewport.

```tsx
import { ChevronDown } from "lucide-react";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { cn } from "../lib/cn";

export function PopoverMenu({
  label,
  icon,
  disabled = false,
  testId,
  width = "w-64",
  children,
}: {
  label: ReactNode;
  icon?: ReactNode;
  disabled?: boolean;
  testId?: string;
  width?: string;
  children: (close: () => void) => ReactNode;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onDown = (e: PointerEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("pointerdown", onDown);
    return () => document.removeEventListener("pointerdown", onDown);
  }, [open]);

  return (
    <div className="relative" ref={ref}>
      <button
        type="button"
        className={cn(
          "flex items-center gap-1.5 rounded-[var(--radius)] border px-2.5 py-1.5 text-xs font-medium text-text transition-colors",
          disabled ? "cursor-default opacity-70" : "hover:bg-surface-2",
        )}
        onClick={() => !disabled && setOpen((o) => !o)}
        disabled={disabled}
        data-testid={testId}
      >
        {icon}
        <span className="max-w-[12rem] truncate">{label}</span>
        {!disabled && <ChevronDown size={13} className="text-faint" />}
      </button>
      {open && !disabled && (
        <div
          className={cn(
            "card absolute bottom-full left-0 z-20 mb-1.5 max-h-72 overflow-y-auto p-1.5 shadow-lg",
            width,
          )}
        >
          {children(() => setOpen(false))}
        </div>
      )}
    </div>
  );
}
```

- [ ] **Step 2: Create `useSessionDraft.ts`**

Holds all draft config + defaults + gating + request assembly. `usePlugins` is sent `true` only for a provisioning vendor (preserves the prior local-vendor default of "unset → server default"; the dropped modal toggle previously did the same via `provisions ? … : undefined`).

```tsx
import { useEffect, useMemo, useState } from "react";
import type { CreateSessionRequest, RepoConfig } from "../api/types";
import { useGithubStatus } from "./useGithub";
import { useMcpServers } from "./useMcp";
import { usePlugins } from "./usePlugins";
import { useSettings } from "./useSettings";

export interface SessionDraft {
  vendor: string;
  setVendor: (v: string) => void;
  model: string;
  setModel: (m: string) => void;
  /** fullName → gitRef ("" = default branch). */
  repos: Map<string, string>;
  setRepos: (m: Map<string, string>) => void;
  skills: Set<string>;
  setSkills: (s: Set<string>) => void;
  mcp: Set<string>;
  setMcp: (s: Set<string>) => void;
  provisions: boolean;
  githubConnected: boolean;
  canSend: boolean;
  blockedReason: string | null;
  buildRequest: () => CreateSessionRequest;
}

export function useSessionDraft(): SessionDraft {
  const { data: settings } = useSettings();
  const { data: ghStatus } = useGithubStatus();
  const { data: bundles } = usePlugins();
  const models = settings?.models ?? [];
  const activeVendors = useMemo(
    () => (settings?.vendors ?? []).filter((v) => v.active),
    [settings],
  );

  const [vendor, setVendor] = useState("");
  const [model, setModel] = useState("");
  const [repos, setRepos] = useState<Map<string, string>>(new Map());
  const [skills, setSkills] = useState<Set<string>>(new Set());
  const [mcp, setMcp] = useState<Set<string>>(new Set());
  const [skillsSeeded, setSkillsSeeded] = useState(false);

  // Seed model/vendor from server config, and keep them on a still-existing
  // choice if config changes.
  useEffect(() => {
    if (!settings) return;
    if (!models.some((m) => m.alias === model)) setModel(models[0]?.alias ?? "");
    if (!activeVendors.some((v) => v.name === vendor))
      setVendor(settings.defaultVendor);
  }, [settings, models, activeVendors, model, vendor]);

  // Pre-select the server's default-enabled bundles once.
  useEffect(() => {
    if (skillsSeeded || !bundles) return;
    setSkills(new Set(bundles.filter((b) => b.enabledDefault).map((b) => b.name)));
    setSkillsSeeded(true);
  }, [bundles, skillsSeeded]);

  const selectedVendor = activeVendors.find(
    (v) => v.name === (vendor || settings?.defaultVendor),
  );
  const provisions = !!selectedVendor?.capabilities?.supportsProvisioning;
  const githubConnected = !!ghStatus?.connected;

  const blockedReason = useMemo(() => {
    if (!model.trim()) return "Select a model to start.";
    if (!vendor.trim()) return "Select a runtime to start.";
    if (provisions && !githubConnected)
      return "Connect GitHub to use this runtime.";
    return null;
  }, [model, vendor, provisions, githubConnected]);

  const buildRequest = (): CreateSessionRequest => {
    const repoList: RepoConfig[] = provisions
      ? Array.from(repos.entries()).map(([fullName, ref]) => ({
          url: `https://github.com/${fullName}`,
          gitRef: ref.trim() || undefined,
        }))
      : [];
    return {
      agent: {
        model: model.trim(),
        usePlugins: provisions ? true : undefined,
        mcpServers: provisions && mcp.size ? Array.from(mcp) : undefined,
      },
      vendor: vendor.trim() || undefined,
      repos: repoList.length ? repoList : undefined,
      plugins: provisions && skills.size ? Array.from(skills) : undefined,
    };
  };

  return {
    vendor,
    setVendor,
    model,
    setModel,
    repos,
    setRepos,
    skills,
    setSkills,
    mcp,
    setMcp,
    provisions,
    githubConnected,
    canSend: blockedReason === null,
    blockedReason,
    buildRequest,
  };
}
```

- [ ] **Step 3: Add `useStartSession` to `useSessions.ts`**

Add after `useSendMessage` (reuses the module-local `applyOptimisticTitle`). Add `CreateSessionRequest` to the existing type import if not already present (it is).

```tsx
/** Create a session and send its first message in one shot, returning the new
 * id. Used by the new-chat draft flow — nothing is created server-side until
 * this runs, so resource allocation is deferred to the first message. */
export function useStartSession() {
  const client = useQueryClient();
  return useMutation({
    mutationFn: async ({
      body,
      text,
    }: {
      body: CreateSessionRequest;
      text: string;
    }) => {
      const res = await api.sessions.create(body);
      const id = res.session.id;
      await api.sessions.send(id, text);
      return id;
    },
    onSuccess: (id, { text }) => {
      applyOptimisticTitle(client, id, text);
      client.invalidateQueries({ queryKey: qk.sessions });
    },
  });
}
```

- [ ] **Step 4: Add the `blockedReason` gating prop to `Composer.tsx`**

Add `blockedReason` to the props (destructure with default `null`), compute `blocked`, and fold it into the submit guard, the send-button `disabled`, its `title`, and a small hint line. Exact edits:

Props signature — add the field:
```tsx
export function Composer({
  status,
  pendingQuestion,
  busy,
  blockedReason = null,
  onSend,
  onStop,
}: {
  status: SessionStatusKind;
  pendingQuestion: string | null;
  busy: boolean;
  blockedReason?: string | null;
  onSend: (text: string) => void;
  onStop: () => void;
}) {
```

After `const awaiting = …;` add:
```tsx
  const blocked = blockedReason != null;
```

In `submit()` guard, change the first line to:
```tsx
    if (!trimmed || !meta.canSend || busy || blocked) return;
```

The send `<button>`: change `disabled` and `title`:
```tsx
            disabled={!text.trim() || !meta.canSend || busy || blocked}
            title={blockedReason ?? "Send"}
```

Add a hint line just after the input row's closing `</div>` (inside the outer `max-w-3xl` container), before the container closes:
```tsx
      {blocked && (
        <p
          className="mt-1.5 px-2 text-xs text-faint"
          data-testid="composer-blocked-hint"
        >
          {blockedReason}
        </p>
      )}
```

- [ ] **Step 5: Typecheck**

Run: `cd clients/web && bun run typecheck`
Expected: PASS (no unused/missing types; `Composer`'s new optional prop doesn't break existing call sites).

- [ ] **Step 6: Commit**

```bash
git add clients/web/src/components/PopoverMenu.tsx clients/web/src/hooks/useSessionDraft.ts clients/web/src/hooks/useSessions.ts clients/web/src/components/Composer.tsx
git commit -m "web: draft config primitives (PopoverMenu, useSessionDraft, useStartSession, composer gating)"
```

---

## Task 3: `SessionConfigBar` component (draft + locked modes)

The toolbar itself. One component, discriminated on `mode`. Draft mode is interactive (writes to `SessionDraft`); locked mode renders static disabled chips from a `SessionDetail`. Remote-only controls (repos/skills/MCP) appear only when the runtime provisions.

**Files:**
- Create: `clients/web/src/components/SessionConfigBar.tsx`

**Interfaces:**
- Consumes: `PopoverMenu`, `SessionDraft` (Task 2), `useGithubRepos`, `usePlugins`, `useMcpServers`, `useSettings`, wire `SessionDetail`.
- Produces: `<SessionConfigBar mode="draft" draft={draft} />` and `<SessionConfigBar mode="locked" detail={detail} />` — used by Tasks 4 and 5.

- [ ] **Step 1: Create `SessionConfigBar.tsx`**

```tsx
import { Boxes, Cpu, FolderGit2, Plug, Server } from "lucide-react";
import { Link } from "react-router-dom";
import type { SessionDetail } from "../api/types";
import { useGithubRepos } from "../hooks/useGithub";
import { useMcpServers } from "../hooks/useMcp";
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
  icon: React.ReactNode;
  children: React.ReactNode;
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
      <div className="ml-auto">
        <LockedChip icon={<Cpu size={13} />} testId="config-model">
          {detail.model}
        </LockedChip>
      </div>
    </>
  );
}
```

- [ ] **Step 2: Typecheck**

Run: `cd clients/web && bun run typecheck`
Expected: PASS. (`basename` is exported from `lib/format`; `useGithubRepos(enabled)` takes a boolean; `SessionDetail` now has `plugins`/`mcpServers` from Task 1.)

- [ ] **Step 3: Commit**

```bash
git add clients/web/src/components/SessionConfigBar.tsx
git commit -m "web: SessionConfigBar toolbar (draft + locked modes)"
```

---

## Task 4: New-chat view + routing, remove the modal

Makes the feature user-visible: `/` becomes the draft new-chat; the sidebar "New" button navigates there; the modal is deleted.

**Files:**
- Create: `clients/web/src/pages/NewSessionView.tsx`
- Modify: `clients/web/src/App.tsx` (index route → `NewSessionView`)
- Modify: `clients/web/src/components/Sidebar.tsx` (New button navigates to `/`; drop modal)
- Delete: `clients/web/src/components/NewSessionModal.tsx`
- Delete: `clients/web/src/pages/Welcome.tsx`

**Interfaces:**
- Consumes: `useSessionDraft`, `useStartSession`, `SessionConfigBar`, `Composer`.

- [ ] **Step 1: Create `NewSessionView.tsx`**

```tsx
import { Sparkles } from "lucide-react";
import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { ApiRequestError } from "../api/client";
import { SessionStatusKind } from "../api/types";
import { Composer } from "../components/Composer";
import { EmptyState } from "../components/EmptyState";
import { SessionConfigBar } from "../components/SessionConfigBar";
import { useSessionDraft } from "../hooks/useSessionDraft";
import { useStartSession } from "../hooks/useSessions";

export function NewSessionView() {
  const draft = useSessionDraft();
  const start = useStartSession();
  const navigate = useNavigate();
  const [error, setError] = useState<string | null>(null);

  const handleSend = async (text: string) => {
    setError(null);
    try {
      const id = await start.mutateAsync({ body: draft.buildRequest(), text });
      navigate(`/sessions/${id}`);
    } catch (e) {
      setError(
        e instanceof ApiRequestError ? e.message : "Failed to start session.",
      );
    }
  };

  return (
    <div className="flex h-full flex-col" data-testid="new-session-view">
      <div className="flex-1 overflow-y-auto">
        <EmptyState icon={<Sparkles size={24} />} title="New chat">
          Pick a runtime below, then send a message to start. For a remote
          runtime you can also select repositories, skills, and MCP servers.
        </EmptyState>
      </div>

      {error && (
        <div className="mx-auto w-full max-w-3xl px-4">
          <div
            data-testid="session-error"
            className="rounded-[var(--radius)] border border-error/40 bg-error-soft px-3 py-2 text-sm text-error"
          >
            {error}
          </div>
        </div>
      )}

      <SessionConfigBar mode="draft" draft={draft} />
      <Composer
        status={SessionStatusKind.Idle}
        pendingQuestion={null}
        busy={start.isPending}
        blockedReason={draft.blockedReason}
        onSend={handleSend}
        onStop={() => {}}
      />
    </div>
  );
}
```

- [ ] **Step 2: Point the index route at `NewSessionView` in `App.tsx`**

Replace the `Welcome` import with `NewSessionView`, and the index element:

```tsx
import { NewSessionView } from "./pages/NewSessionView";
```
```tsx
            <Route index element={<NewSessionView />} />
```
Remove the `import { Welcome } from "./pages/Welcome";` line.

- [ ] **Step 3: Update `Sidebar.tsx` — New navigates to `/`, drop the modal**

- Remove the `useState` `modal` and the `NewSessionModal` import and its `<NewSessionModal … />` element at the bottom.
- Change the New button's `onClick`:
```tsx
        <button
          className="btn-primary ml-auto !px-2.5 !py-1.5 text-xs"
          onClick={() => navigate("/")}
          data-testid="new-session-button"
        >
          <Plus size={15} />
          New
        </button>
```
(`navigate` is already imported via `useNavigate`.)

- [ ] **Step 4: Delete the dead files**

```bash
git rm clients/web/src/components/NewSessionModal.tsx clients/web/src/pages/Welcome.tsx
```

- [ ] **Step 5: Typecheck + build**

Run: `cd clients/web && bun run typecheck && bun run build`
Expected: PASS with no dangling imports of `NewSessionModal` or `Welcome`.

- [ ] **Step 6: Commit**

```bash
git add clients/web/src/pages/NewSessionView.tsx clients/web/src/App.tsx clients/web/src/components/Sidebar.tsx
git commit -m "web: inline new-chat draft view; remove new-session modal"
```

---

## Task 5: Show locked config bar on existing sessions

Render `SessionConfigBar` (locked) in `SessionView` and remove the now-duplicated model/vendor/repos header chips.

**Files:**
- Modify: `clients/web/src/pages/SessionView.tsx`

- [ ] **Step 1: Import the config bar**

Add near the other component imports in `SessionView.tsx`:
```tsx
import { SessionConfigBar } from "../components/SessionConfigBar";
```

- [ ] **Step 2: Remove the model/vendor/repos chips from the header**

In the header chip row (`SessionView.tsx:141-165`), delete the three `Chip` blocks for `detail?.model`, `detail?.vendor`, and `detail?.repos?.map(...)`. Keep the `<ContextStatsPanel …/>` (it is usage, not config). The `Cpu`, `Server`, `FolderGit2` lucide imports and the local `Chip` helper become unused — remove them to keep typecheck clean (`FolderGit2`/`Server`/`Cpu` are only used by those chips; verify with a quick search before deleting the imports).

- [ ] **Step 3: Render the locked bar above the Composer**

Immediately before the `<Composer … />` element (after the progression block), add:
```tsx
        {detail && <SessionConfigBar mode="locked" detail={detail} />}
```

- [ ] **Step 4: Typecheck + build**

Run: `cd clients/web && bun run typecheck && bun run build`
Expected: PASS (no unused-import errors; if `Chip` is now unused, remove it too).

- [ ] **Step 5: Commit**

```bash
git add clients/web/src/pages/SessionView.tsx
git commit -m "web: show locked config bar on existing sessions"
```

---

## Task 6: e2e harness migration + new coverage

The client-only-draft model changes the `createSession` contract: it now only *drafts* a chat (no server session), and the session is created by the **first** `sendMessage`. Update the harness helpers and the specs that relied on the old semantics (name field, create-without-send, id-before-send), then add coverage for the draft flow and the locked bar.

**Files:**
- Modify: `clients/web/e2e/helpers.ts`
- Modify: `clients/web/e2e/a-turn-basics.spec.ts` (rewrite A1)
- Modify: `clients/web/e2e/d-lifecycle.spec.ts` (D2, D3 — id capture + drop names)
- Modify: `clients/web/e2e/i-history-pagination.spec.ts` (create via first UI send)
- Create: `clients/web/e2e/j-new-session.spec.ts`

**Interfaces:**
- Produces: `createSession(page, appBase, opts?: { model?: string }): Promise<void>` (drafts only); `sendMessage(page, text): Promise<string>` (returns the session id, creating the session on a draft send).

- [ ] **Step 1: Rewrite the helpers**

Replace `createSession` and `sendMessage` in `clients/web/e2e/helpers.ts`:

```ts
/**
 * Start a new-chat draft: navigate to `/`, wait for the draft config bar, and
 * optionally pick a model. Creates NOTHING server-side — the session is created
 * by the first `sendMessage`.
 */
export async function createSession(
  page: Page,
  appBase: string,
  opts: { model?: string } = {},
): Promise<void> {
  await page.goto(appBase);
  await expect(page.getByTestId("config-model")).toBeVisible();
  if (opts.model) {
    await page.getByTestId("config-model").click();
    await page
      .locator(`[data-testid="model-option"][data-value="${opts.model}"]`)
      .click();
  }
}

/**
 * Type a message and send it (Enter). On a draft (`/`) this creates the session
 * and waits for the `/sessions/:id` route. Returns the session id.
 */
export async function sendMessage(page: Page, text: string): Promise<string> {
  const onDraft = new URL(page.url()).pathname === "/";
  const input = page.getByTestId("composer-input");
  await input.fill(text);
  await input.press("Enter");
  if (onDraft) await page.waitForURL(/\/sessions\/[0-9a-f-]+$/);
  const id = new URL(page.url()).pathname.split("/").pop();
  if (!id) throw new Error("no session id in URL after send");
  return id;
}
```

- [ ] **Step 2: Rewrite A1 in `a-turn-basics.spec.ts`**

A draft is not listed/Idle until the first message. Replace the A1 test body:

```ts
test("A1: draft creates a session on the local runtime vendor via first message", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueText("ok");
  await createSession(page, appBase);
  // Draft toolbar is present and editable before anything is created.
  await expect(page.getByTestId("session-config-bar")).toHaveAttribute(
    "data-mode",
    "draft",
  );

  const id = await sendMessage(page, "first message");

  await expectStatus(page, "Idle");
  await expect(
    page.locator('[data-testid="session-row"]', { hasText: "first message" }),
  ).toBeVisible();
  // The locked config bar shows the real local vendor.
  await expect(page.getByTestId("session-config-bar")).toHaveAttribute(
    "data-mode",
    "locked",
  );
  await expect(page.getByTestId("config-runtime")).toContainText("e2e");
  expect(id).toMatch(/[0-9a-f-]{8,}/);
});
```

- [ ] **Step 3: Fix D2 and D3 in `d-lifecycle.spec.ts`**

D2 — capture the id from the first send (no name field):
```ts
test("D2: delete a session removes it and navigates away", async ({ page, appBase, mock }) => {
  await mock.queueText("ok");
  await createSession(page, appBase);
  const id = await sendMessage(page, "to delete");

  page.on("dialog", (d) => d.accept()); // auto-accept the native confirm()
  await page.getByTestId("session-delete").click();

  await page.waitForURL((url) => url.pathname === "/");
  await expect(
    page.locator(`[data-testid="session-row"][data-session-id="${id}"]`),
  ).toHaveCount(0);
});
```

D3 — drop the `{ name }` options; the first message both creates the session and becomes its title; capture `id1` from the first send:
```ts
  await mock.queueText("Reply in session ONE.");
  await createSession(page, appBase);
  const id1 = await sendMessage(page, "hello one");
  await expect(page.getByTestId("assistant-text")).toContainText("Reply in session ONE.");

  await mock.queueText("Reply in session TWO.");
  await createSession(page, appBase);
  await sendMessage(page, "hello two");
  await expect(page.getByTestId("assistant-text")).toContainText("Reply in session TWO.");
```
(The rest of D3 — switching back via `id1` — is unchanged.)

- [ ] **Step 4: Fix `i-history-pagination.spec.ts` (create via first UI send)**

The test captured `id` from `createSession` then POSTed all turns via the API. Now the session is created by sending turn 1 through the UI; the remaining turns still go via the API. Replace the create + loop preamble (around lines 57-72):

```ts
  await createSession(page, appBase);
  // Turn 1 through the UI creates the session and yields its id; it consumes
  // the first queued mock reply (`answer 1`).
  const id = await sendMessage(page, "question 1");
  await expect
    .poll(async () =>
      (await page.request.get(`${appBase}/api/sessions/${id}/history?limit=100`)).status(),
    )
    .toBe(200);

  // Remaining turns via the API (fast + deterministic).
  for (let i = 2; i <= turns; i++) {
    const res = await page.request.post(
      `${appBase}/api/sessions/${id}/messages`,
      { data: { text: `question ${i}` } },
    );
    expect(res.status()).toBe(202);
    // …existing per-turn poll on reply count / Idle settle, unchanged…
```
Keep the existing per-turn poll body; only the loop now starts at `i = 2` and `id` comes from the UI send. (The `mock.queue` of `answer 1..26` is unchanged — turn 1 via UI consumes `answer 1`.)

- [ ] **Step 5: Add `j-new-session.spec.ts` (draft gating + locked bar)**

```ts
// Group J — the inline new-session draft flow: config toolbar, gating, and the
// read-only config bar on an existing session.
import { test, expect } from "./fixtures";
import { createSession, sendMessage } from "./helpers";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

test("J1: the New button opens an editable draft at /", async ({ page, appBase }) => {
  await page.goto(appBase);
  await page.getByTestId("new-session-button").click();
  await page.waitForURL((url) => url.pathname === "/");
  await expect(page.getByTestId("session-config-bar")).toHaveAttribute("data-mode", "draft");
  // Local (e2e) vendor does not provision, so no repo/skill/MCP controls show.
  await expect(page.getByTestId("config-runtime")).toBeVisible();
  await expect(page.getByTestId("config-model")).toBeVisible();
  await expect(page.getByTestId("config-repos")).toHaveCount(0);
});

test("J2: an existing session shows a locked, read-only config bar", async ({
  page,
  appBase,
  mock,
}) => {
  await mock.queueText("hello");
  await createSession(page, appBase);
  await sendMessage(page, "configure me");

  const bar = page.getByTestId("session-config-bar");
  await expect(bar).toHaveAttribute("data-mode", "locked");
  await expect(page.getByTestId("config-runtime")).toContainText("e2e");
  // Locked model chip is not a menu button — clicking opens nothing.
  await expect(page.getByTestId("config-model")).toContainText("mock");
});
```

- [ ] **Step 6: Run the full e2e suite**

Run: `cd clients/web && bun run test:e2e`
Expected: all specs green (A–J). If a spec that only did `await createSession(...)` then `await sendMessage(...)` fails, it is because it asserted state *before* sending — inspect and move the assertion after the first `sendMessage`.

- [ ] **Step 7: Commit**

```bash
git add clients/web/e2e
git commit -m "web/e2e: migrate helpers to draft flow; cover config toolbar"
```

**Coverage note (log, do not silently drop):** the e2e harness seeds only the non-provisioning `e2e` vendor and does not connect GitHub, so the **remote branch** of the toolbar (repo/skill/MCP controls visible, "Connect GitHub" gating that blocks send) is **not** exercised end-to-end. It is covered by typecheck + the shared capability logic and must be verified manually against a velos deployment. Do not claim remote-branch e2e coverage.

---

## Task 7: Full gate + manual verification

- [ ] **Step 1: Rust gate**

Run: `make check`
Expected: fmt clean, clippy `-D warnings` clean, all tests pass (including `session_detail_echoes_full_config`).

- [ ] **Step 2: Web gate**

Run: `cd clients/web && bun run typecheck && bun run build && bun run test:e2e`
Expected: all green.

- [ ] **Step 3: Confirm no fluorite drift**

Run: `cd clients/ts && bun run generate-types && git diff --exit-code src/generated`
Expected: no diff (Task 1 already committed the regen). If there is a diff, commit it.

- [ ] **Step 4: Manual smoke (local runtime)**

Start the dev stack and confirm by eye: `/` shows the empty chat with a draft toolbar (runtime + model, no repo/skill/MCP for the local vendor); send is disabled with a hint if no model; sending the first message creates the session, navigates to `/sessions/:id`, and the toolbar switches to a locked, disabled config bar. (Remote-branch verification — repos/skills/MCP + GitHub gating — is deferred to a velos deployment per the Task 6 coverage note.)

- [ ] **Step 5: Open the PR**

Push the branch and open a PR to `main` (no AI attribution). Summarize: inline new-session toolbar, client-only draft (deferred resource allocation), `SessionDetail` config echo, e2e migration; call out the remote-branch e2e coverage gap.

---

## Self-review notes

- **Spec coverage:** no popup (Task 4 deletes modal) ✓; no name field (draft has none) ✓; toolbar above input with runtime + remote repos/skills/MCP (Task 3) ✓; model on the right, editable-now/locked-later (Task 3, structured standalone) ✓; send gated until config complete (Task 2 `blockedReason` + Composer) ✓; existing session shows read-only config (Task 5 locked bar) ✓; resources allocated only on first message (client-only draft — Tasks 2/4) ✓; backend `SessionDetail` echo (Task 1) ✓.
- **`usePlugins` decision:** sent `true` only for provisioning vendors (preserves prior local default of unset). Verify in code that a provisioning session with zero selected bundles still loads the operator default-enabled set; if the server keys default-loading off `plugins` being absent vs. empty, adjust `buildRequest` to omit `plugins` when the set is empty (it already does).
- **No placeholders:** all steps carry concrete code. The one "apply existing poll body unchanged" reference (Task 6 Step 4) points at code already present in the file, not omitted new code.
