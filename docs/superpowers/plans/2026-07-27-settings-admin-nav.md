# Settings & Admin Navigation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn the single 1568-line `/settings` page plus three sibling top-level pages into a sidebar-navigated Settings area (Models & providers, Runtimes, Skills, Memory, Integrations) and an identically-shaped Admin area (Model cards).

**Architecture:** Two nested route layouts (`SettingsLayout`, `AdminLayout`) render a shared `SettingsNav` column plus an `<Outlet/>`, both nested inside the existing `SessionsLayout` for a three-column shape. `SettingsPage.tsx` is carved into three pages by save boundary: Models & providers PUTs `{providers, models}`, Runtimes PUTs `{vendors, defaultVendor}`, Integrations is self-saving. Shared form primitives move to one module consumed by every settings page.

**Tech Stack:** React 19, react-router-dom v7 (`<BrowserRouter>` + `<Routes>`, *not* a data router), TanStack Query v5, Tailwind v4, Playwright for e2e, Bun as package manager.

**Spec:** `docs/superpowers/specs/2026-07-27-settings-admin-nav-design.md`

## Global Constraints

- Frontend only. No changes under `server/`, `models/fluorite/`, or `clients/web/src/generated/`.
- No behaviour changes to what any setting *does* — only where it lives and which button saves it.
- Work in the worktree `/Users/xiaoguang/works/repos/bloomstack/october/horsie-settings-nav`, branch `feat/settings-admin-nav`.
- All web commands run from `clients/web`.
- Typecheck gate after every task: `bun run typecheck` (this is `tsc -b`; it must exit 0).
- `useBlocker` is forbidden — it requires a data router. Unsaved-change guarding uses `window.confirm` inside the nav links.
- Keep every existing `data-testid` value byte-identical when moving code; e2e specs key off them.
- Route paths are exactly: `/settings/models`, `/settings/runtimes`, `/settings/skills`, `/settings/memory`, `/settings/integrations`, `/admin/model-cards`.
- Commit after each task with a short subject line, no body, no AI attribution trailers.

## File Structure

**Created**

| Path | Responsibility |
| --- | --- |
| `src/pages/settings/fields.tsx` | Shared form primitives: `Section`, `RowLabel`, `TextField`, `RowShell` |
| `src/pages/settings/SettingsHeader.tsx` | Page header bar: title, description, dirty/saved indicator, optional Discard + Save |
| `src/pages/settings/ModelsSettings.tsx` | Providers + Models drafts, validation, `PUT {providers, models}` |
| `src/pages/settings/RuntimesSettings.tsx` | Default-vendor card, Velos rows + tests, restart banner, `PUT {vendors, defaultVendor}` |
| `src/pages/settings/IntegrationsSettings.tsx` | GitHub section, MCP section, server info |
| `src/pages/settings/SkillsSettings.tsx` | Former `SkillsPage` body |
| `src/pages/settings/MemorySettings.tsx` | Former `MemoryPage` body |
| `src/pages/settings/SettingsLayout.tsx` | Settings nav column + `<Outlet/>` + dirty-guard context |
| `src/pages/settings/dirty.tsx` | `SettingsDirtyProvider`, `useSettingsDirty`, `usePublishDirty` |
| `src/components/SettingsNav.tsx` | Generic nav column driven by an items array; used by Settings and Admin |
| `src/pages/admin/AdminLayout.tsx` | Admin nav column + `<Outlet/>` |
| `src/pages/admin/ModelCardsPage.tsx` | Former `AdminPage` model-cards section |
| `e2e/l-settings-nav.spec.ts` | Nav rendering, deep links, redirects, unsaved-changes guard |

**Deleted**

`src/pages/SettingsPage.tsx`, `src/pages/SkillsPage.tsx`, `src/pages/MemoryPage.tsx`, `src/pages/AdminPage.tsx` — no shim re-exports.

**Modified**

`src/App.tsx` (routes), `src/components/Sidebar.tsx` (footer chips), `e2e/k-model-cards.spec.ts` (paths).

---

### Task 1: Shared form primitives

Extract the four primitives duplicated across three pages into one module, and add the header component the split pages will all use. Pure refactor — the UI must render identically.

**Files:**
- Create: `clients/web/src/pages/settings/fields.tsx`
- Create: `clients/web/src/pages/settings/SettingsHeader.tsx`
- Modify: `clients/web/src/pages/SettingsPage.tsx` (delete lines 664–763; import instead)
- Modify: `clients/web/src/pages/SkillsPage.tsx` (delete `RowLabel` at 226–232 and `TextField` at 234–256; import instead)
- Modify: `clients/web/src/pages/AdminPage.tsx` (delete `RowLabel` at 68–76; import instead)

**Interfaces:**
- Produces: `Section({title, desc, children, onAdd, addLabel, empty})`, `RowLabel({children})`, `TextField({label, value, onChange, placeholder, type})`, `RowShell({onRemove, removeLabel, children})`, `SettingsHeader({title, desc, dirty, saved, saving, onSave, onDiscard})`.

- [ ] **Step 1: Create `fields.tsx`**

Copy `Section`, `RowLabel`, `TextField`, `RowShell` verbatim from `SettingsPage.tsx:664-763`, exporting each. The file's imports:

```tsx
import { Plus, Trash2 } from "lucide-react";
import type { ReactNode } from "react";
```

Change each `function X(` to `export function X(`. Do not alter class names or markup.

- [ ] **Step 2: Create `SettingsHeader.tsx`**

```tsx
import { Check, Loader2, RotateCcw, Save } from "lucide-react";

/**
 * The header bar every settings/admin page renders. Pages that own a batched
 * save pass `onSave`/`onDiscard`; self-saving pages (Skills, Memory,
 * Integrations, Model cards) omit them and get title + description only.
 */
export function SettingsHeader({
  title,
  desc,
  dirty = false,
  saved = false,
  saving = false,
  onSave,
  onDiscard,
}: {
  title: string;
  desc: string;
  dirty?: boolean;
  saved?: boolean;
  saving?: boolean;
  onSave?: () => void;
  onDiscard?: () => void;
}) {
  return (
    <header className="flex items-center gap-3 border-b px-6 py-3.5">
      <div>
        <h1 className="text-[15px] font-semibold text-text">{title}</h1>
        <p className="text-xs text-faint">{desc}</p>
      </div>
      {onSave && (
        <div className="ml-auto flex items-center gap-2">
          {dirty && !saving && (
            <span className="text-xs text-faint">Unsaved changes</span>
          )}
          {saved && !dirty && (
            <span className="flex items-center gap-1 text-xs text-success">
              <Check size={13} /> Saved
            </span>
          )}
          <button className="btn-ghost" onClick={onDiscard} disabled={!dirty}>
            <RotateCcw size={14} /> Discard
          </button>
          <button
            className="btn-primary"
            onClick={onSave}
            disabled={!dirty || saving}
            data-testid="settings-save"
          >
            {saving ? (
              <Loader2 size={15} className="animate-spin" />
            ) : (
              <Save size={15} />
            )}
            Save changes
          </button>
        </div>
      )}
    </header>
  );
}
```

- [ ] **Step 3: Point the three existing pages at the shared primitives**

In `SettingsPage.tsx`: delete lines 664–763 and add `import { RowShell, RowLabel, Section, TextField } from "./settings/fields";`. Drop now-unused `Plus`/`Trash2` from the lucide import if nothing else in the file uses them (`Plus` is still used by `ModelIdField`; check before removing).

In `SkillsPage.tsx`: delete its local `RowLabel` and `TextField`, import `{ RowLabel, TextField } from "./settings/fields"`. Its `Toggle` stays local.

In `AdminPage.tsx`: delete its local `RowLabel`, import `{ RowLabel } from "../pages/settings/fields"` — the correct relative path from `src/pages/AdminPage.tsx` is `./settings/fields`.

- [ ] **Step 4: Typecheck**

Run: `cd clients/web && bun run typecheck`
Expected: exit 0, no unused-import errors.

- [ ] **Step 5: Commit**

```bash
git add clients/web/src/pages
git commit -m "web: extract shared settings form primitives"
```

---

### Task 2: Split SettingsPage into three pages

Carve the monolith along its save boundary. Routes go flat for this task (`/settings`, `/settings/runtimes`, `/settings/integrations`); Task 3 wraps them in the nav layout. Every commit must build.

**Files:**
- Create: `clients/web/src/pages/settings/ModelsSettings.tsx`
- Create: `clients/web/src/pages/settings/RuntimesSettings.tsx`
- Create: `clients/web/src/pages/settings/IntegrationsSettings.tsx`
- Delete: `clients/web/src/pages/SettingsPage.tsx`
- Modify: `clients/web/src/App.tsx`

**Interfaces:**
- Consumes: `Section`, `RowLabel`, `TextField`, `RowShell`, `SettingsHeader` from Task 1.
- Produces: `ModelsSettings()`, `RuntimesSettings()`, `IntegrationsSettings()` — all zero-arg default-styled page components.

Source ranges in the pre-deletion `SettingsPage.tsx` (line numbers as of commit `6c2f820`):

| Range | Symbol | Destination |
| --- | --- | --- |
| 49–65 | `ProviderKind`, `ProviderDraft`, `ModelDraft` types | `ModelsSettings.tsx` |
| 67–81 | `VelosDraft` type | `RuntimesSettings.tsx` |
| 83–99 | `toProviderDrafts`, `toModelDrafts` | `ModelsSettings.tsx` |
| 101–126 | `num`, `toVelosDrafts` | `RuntimesSettings.tsx` |
| 765–813 | `ProviderRow` | `ModelsSettings.tsx` |
| 814–891 | `ModelIdField` | `ModelsSettings.tsx` |
| 892–944 | `ModelRow` | `ModelsSettings.tsx` |
| 945–1064 | `VelosRow` | `RuntimesSettings.tsx` |
| 1065–1112 | `VendorsCard` | `RuntimesSettings.tsx` |
| 44–47, 498–662 | `GITHUB_MCP_URL`, `GITHUB_MCP_NAME`, `GithubSection` | `IntegrationsSettings.tsx` |
| 1113–1213 | `GithubMcpToggle` | `IntegrationsSettings.tsx` |
| 1214–1284 | `McpSection` | `IntegrationsSettings.tsx` |
| 1285–1533 | `McpServerRow` | `IntegrationsSettings.tsx` |
| 1534–1568 | `ServerInfoCard`, `FieldRow` | `IntegrationsSettings.tsx` |

- [ ] **Step 1: Create `ModelsSettings.tsx`**

Move the ranges above, then write the page component. It is the old `SettingsPage` with velos/github/mcp/server-info removed and the save narrowed:

```tsx
export function ModelsSettings() {
  const { data: settings, isLoading, isError } = useSettings();
  const update = useUpdateSettings();

  const [providers, setProviders] = useState<ProviderDraft[]>([]);
  const [models, setModels] = useState<ModelDraft[]>([]);
  const [dirty, setDirty] = useState(false);
  const [localError, setLocalError] = useState<string | null>(null);

  useEffect(() => {
    if (!settings) return;
    setProviders(toProviderDrafts(settings));
    setModels(toModelDrafts(settings));
    setDirty(false);
    setLocalError(null);
  }, [settings]);

  const providerNames = useMemo(
    () => providers.map((p) => p.name.trim()).filter(Boolean),
    [providers],
  );

  const touch = () => setDirty(true);

  const save = () => {
    setLocalError(null);
    const uniq = (xs: string[]) => new Set(xs).size === xs.length;
    if (providers.some((p) => !p.name.trim()))
      return setLocalError("Every provider needs a name.");
    if (!uniq(providers.map((p) => p.name.trim())))
      return setLocalError("Provider names must be unique.");
    if (models.some((m) => !m.alias.trim()))
      return setLocalError("Every model needs an alias.");
    if (!uniq(models.map((m) => m.alias.trim())))
      return setLocalError("Model aliases must be unique.");
    for (const m of models)
      if (m.maxTokens.trim() && !/^\d+$/.test(m.maxTokens.trim()))
        return setLocalError(`Max tokens for "${m.alias}" must be a number.`);
    for (const m of models)
      if (m.contextWindow.trim() && !/^\d+$/.test(m.contextWindow.trim()))
        return setLocalError(`Context window for "${m.alias}" must be a number.`);

    const providerInputs: ProviderInput[] = providers.map((p) => ({
      name: p.name.trim(),
      kind: p.kind,
      baseUrl: p.baseUrl.trim() || undefined,
      apiKey: p.apiKeyInput === "" ? undefined : p.apiKeyInput,
    }));
    const modelInputs: ModelInput[] = models.map((m) => ({
      alias: m.alias.trim(),
      provider: m.provider,
      modelId: m.modelId.trim(),
      maxTokens: m.maxTokens.trim() ? Number(m.maxTokens.trim()) : undefined,
      contextWindow: m.contextWindow.trim()
        ? Number(m.contextWindow.trim())
        : undefined,
    }));
    update.mutate({ providers: providerInputs, models: modelInputs });
  };

  const discard = () => {
    if (!settings) return;
    setProviders(toProviderDrafts(settings));
    setModels(toModelDrafts(settings));
    setDirty(false);
    setLocalError(null);
    update.reset();
  };

  const saveError =
    update.error instanceof ApiRequestError
      ? update.error.message
      : update.isError
        ? "Failed to save settings."
        : null;

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <SettingsHeader
        title="Models & providers"
        desc="API endpoints and the model aliases sessions pick from."
        dirty={dirty}
        saved={update.isSuccess}
        saving={update.isPending}
        onSave={save}
        onDiscard={discard}
      />
      <div className="min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto max-w-3xl space-y-6 px-6 py-6">
          {isLoading && (
            <div className="py-16 text-center text-sm text-faint">Loading…</div>
          )}
          {isError && (
            <div className="rounded-[var(--radius)] border border-error/40 bg-error-soft px-3 py-2 text-sm text-error">
              Couldn’t load settings. Is <code>horsie serve</code> running?
            </div>
          )}
          {(localError || saveError) && (
            <div className="rounded-[var(--radius)] border border-error/40 bg-error-soft px-3 py-2 text-sm text-error">
              {localError ?? saveError}
            </div>
          )}
          {settings && (
            <>
              {/* Providers <Section> — copied verbatim from SettingsPage.tsx:354-387 */}
              {/* Models <Section>    — copied verbatim from SettingsPage.tsx:389-423 */}
            </>
          )}
        </div>
      </div>
    </div>
  );
}
```

Replace the two comment placeholders with the actual `<Section>` blocks from those ranges — unchanged, including the `providerNames[0] ?? ""` default in the Add-model handler.

- [ ] **Step 2: Create `RuntimesSettings.tsx`**

Same shape, holding `VelosDraft`, `num`, `toVelosDrafts`, `VelosRow`, `VendorsCard`, plus the velos-test state (`velosTests`, `runVelosTest` — copied from `SettingsPage.tsx:139-164`) and the `restartRequired` banner from `SettingsPage.tsx:343-350`.

State: `velos`, `defaultVendor`, `dirty`, `localError`, `velosTests`. Seeding effect sets `setVelos(toVelosDrafts(settings))` and `setDefaultVendor(settings.defaultVendor)`.

Its `save()` keeps the velos validation block (`SettingsPage.tsx:201-221`) and the `vendorInputs` mapping (`236-254`), then:

```tsx
    update.mutate(
      {
        vendors: vendorInputs,
        defaultVendor: defaultVendor || undefined,
      },
      {
        onSuccess: (view) => {
          for (const vd of view.vendors) {
            if (vd.config?.kind === "Velos") runVelosTest(vd.name);
          }
        },
      },
    );
```

Header: `title="Runtimes"`, `desc="Where sessions execute — the default vendor and any velos clusters."`. Body order: restart banner, `<VendorsCard>`, the Velos `<Section>` (`SettingsPage.tsx:434-478`).

- [ ] **Step 3: Create `IntegrationsSettings.tsx`**

Move `GITHUB_MCP_URL`, `GITHUB_MCP_NAME`, `GithubSection`, `GithubMcpToggle`, `McpSection`, `McpServerRow`, `ServerInfoCard`, `FieldRow` verbatim. The page component has no draft state and no Save button:

```tsx
export function IntegrationsSettings() {
  const { data: settings, isLoading, isError } = useSettings();
  return (
    <div className="flex h-full flex-col overflow-hidden">
      <SettingsHeader
        title="Integrations"
        desc="GitHub, MCP servers, and this server's build info."
      />
      <div className="min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto max-w-3xl space-y-6 px-6 py-6">
          {isLoading && (
            <div className="py-16 text-center text-sm text-faint">Loading…</div>
          )}
          {isError && (
            <div className="rounded-[var(--radius)] border border-error/40 bg-error-soft px-3 py-2 text-sm text-error">
              Couldn’t load settings. Is <code>horsie serve</code> running?
            </div>
          )}
          <GithubSection />
          <McpSection />
          {settings && <ServerInfoCard view={settings} />}
        </div>
      </div>
    </div>
  );
}
```

- [ ] **Step 4: Delete the monolith and rewire routes**

Delete `src/pages/SettingsPage.tsx`. In `App.tsx`, replace the `settings` route with three flat routes for now:

```tsx
<Route path="settings" element={<ModelsSettings />} />
<Route path="settings/runtimes" element={<RuntimesSettings />} />
<Route path="settings/integrations" element={<IntegrationsSettings />} />
```

- [ ] **Step 5: Typecheck**

Run: `cd clients/web && bun run typecheck`
Expected: exit 0.

- [ ] **Step 6: Commit**

```bash
git add -A clients/web/src
git commit -m "web: split settings page by save boundary"
```

---

### Task 3: Settings nav, layout, and moved Skills/Memory

**Files:**
- Create: `clients/web/src/components/SettingsNav.tsx`
- Create: `clients/web/src/pages/settings/dirty.tsx`
- Create: `clients/web/src/pages/settings/SettingsLayout.tsx`
- Create: `clients/web/src/pages/settings/SkillsSettings.tsx` (from `SkillsPage.tsx`)
- Create: `clients/web/src/pages/settings/MemorySettings.tsx` (from `MemoryPage.tsx`)
- Delete: `clients/web/src/pages/SkillsPage.tsx`, `clients/web/src/pages/MemoryPage.tsx`
- Modify: `clients/web/src/App.tsx`, `clients/web/src/components/Sidebar.tsx`
- Modify: `clients/web/src/pages/settings/ModelsSettings.tsx`, `RuntimesSettings.tsx` (publish dirty)

**Interfaces:**
- Produces: `SettingsNav({title, items})` where `items: { to: string; label: string; icon: LucideIcon }[]`; `SettingsDirtyProvider`, `usePublishDirty(dirty: boolean)`, `useSettingsDirty(): { confirmLeave: () => boolean }`.

- [ ] **Step 1: Create `dirty.tsx`**

```tsx
import {
  createContext,
  useContext,
  useEffect,
  useMemo,
  useRef,
  type ReactNode,
} from "react";

type DirtyCtx = {
  /** Called by pages that batch edits; the ref keeps the provider stable. */
  setDirty: (dirty: boolean) => void;
  /** True to proceed with navigation; prompts when there are unsaved edits. */
  confirmLeave: () => boolean;
};

const Ctx = createContext<DirtyCtx | null>(null);

export function SettingsDirtyProvider({ children }: { children: ReactNode }) {
  const dirtyRef = useRef(false);
  const value = useMemo<DirtyCtx>(
    () => ({
      setDirty: (d) => {
        dirtyRef.current = d;
      },
      confirmLeave: () =>
        !dirtyRef.current ||
        window.confirm("Discard unsaved changes?"),
    }),
    [],
  );
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

/** Nav links use this to gate navigation. Safe outside the provider. */
export function useSettingsDirty(): DirtyCtx {
  return (
    useContext(Ctx) ?? { setDirty: () => {}, confirmLeave: () => true }
  );
}

/** Pages with a batched save publish their dirty flag and clear it on unmount. */
export function usePublishDirty(dirty: boolean) {
  const { setDirty } = useSettingsDirty();
  useEffect(() => {
    setDirty(dirty);
    return () => setDirty(false);
  }, [dirty, setDirty]);
}
```

- [ ] **Step 2: Create `SettingsNav.tsx`**

```tsx
import type { LucideIcon } from "lucide-react";
import { NavLink } from "react-router-dom";
import { cn } from "../lib/cn";
import { useSettingsDirty } from "../pages/settings/dirty";

export type NavItem = { to: string; label: string; icon: LucideIcon };

/** The second column of the settings/admin areas: a vertical page switcher. */
export function SettingsNav({
  title,
  items,
}: {
  title: string;
  items: NavItem[];
}) {
  const { confirmLeave } = useSettingsDirty();
  return (
    <nav
      className="flex h-full w-52 shrink-0 flex-col border-r"
      style={{ background: "var(--surface)" }}
      data-testid="settings-nav"
    >
      <div className="px-4 py-3.5 text-[15px] font-semibold tracking-tight text-text">
        {title}
      </div>
      <div className="space-y-0.5 px-2">
        {items.map(({ to, label, icon: Icon }) => (
          <NavLink
            key={to}
            to={to}
            end
            data-testid={`settings-nav-${to}`}
            onClick={(e) => {
              if (!confirmLeave()) e.preventDefault();
            }}
            className={({ isActive }) =>
              cn(
                "flex items-center gap-2 rounded-[var(--radius)] px-2.5 py-2 text-sm transition-colors",
                isActive
                  ? "bg-surface-3 text-text"
                  : "text-muted hover:bg-surface-2 hover:text-text",
              )
            }
          >
            <Icon size={15} />
            {label}
          </NavLink>
        ))}
      </div>
    </nav>
  );
}
```

- [ ] **Step 3: Create `SettingsLayout.tsx`**

```tsx
import { Boxes, Brain, Cpu, Plug, SlidersHorizontal } from "lucide-react";
import { Outlet } from "react-router-dom";
import { SettingsNav, type NavItem } from "../../components/SettingsNav";
import { SettingsDirtyProvider } from "./dirty";

const ITEMS: NavItem[] = [
  { to: "models", label: "Models", icon: SlidersHorizontal },
  { to: "runtimes", label: "Runtimes", icon: Cpu },
  { to: "skills", label: "Skills", icon: Boxes },
  { to: "memory", label: "Memory", icon: Brain },
  { to: "integrations", label: "Integrations", icon: Plug },
];

export function SettingsLayout() {
  return (
    <SettingsDirtyProvider>
      <div className="flex h-full overflow-hidden">
        <SettingsNav title="Settings" items={ITEMS} />
        <div className="min-w-0 flex-1">
          <Outlet />
        </div>
      </div>
    </SettingsDirtyProvider>
  );
}
```

Verify each icon name exists in the installed `lucide-react` before committing (`grep -o '"SlidersHorizontal"' node_modules/lucide-react/dist/lucide-react.d.ts | head -1`); substitute a present icon if not.

- [ ] **Step 4: Move Skills and Memory**

`git mv clients/web/src/pages/SkillsPage.tsx clients/web/src/pages/settings/SkillsSettings.tsx` and the same for `MemoryPage.tsx` → `settings/MemorySettings.tsx`. In each:
- rename the exported component (`SkillsPage` → `SkillsSettings`, `MemoryPage` → `MemorySettings`);
- fix relative imports (`../api/client` → `../../api/client`, etc.);
- replace the hand-rolled `<header>` block with `<SettingsHeader title="Skills" desc="Shareable skill bundles installed from git repos — pick them per session." />` and `<SettingsHeader title="Memory" desc="Durable notes the agent saves and reads back — grouped into spaces you pick per session." />`;
- in `SkillsSettings.tsx`, import `RowLabel`/`TextField` from `./fields` (path shortens after the move).

- [ ] **Step 5: Rewire routes**

```tsx
<Route path="settings" element={<SettingsLayout />}>
  <Route index element={<Navigate to="models" replace />} />
  <Route path="models" element={<ModelsSettings />} />
  <Route path="runtimes" element={<RuntimesSettings />} />
  <Route path="skills" element={<SkillsSettings />} />
  <Route path="memory" element={<MemorySettings />} />
  <Route path="integrations" element={<IntegrationsSettings />} />
</Route>
<Route path="skills" element={<Navigate to="/settings/skills" replace />} />
<Route path="memory" element={<Navigate to="/settings/memory" replace />} />
```

Import `Navigate` from `react-router-dom`.

- [ ] **Step 6: Publish dirty state from the two batched pages**

In `ModelsSettings.tsx` and `RuntimesSettings.tsx`, add `usePublishDirty(dirty);` immediately after the `dirty` state declaration, importing from `./dirty`.

- [ ] **Step 7: Trim the sidebar footer**

In `Sidebar.tsx`, delete the Skills and Memory `NavLink`s (lines 144–171) and drop `Boxes`/`Brain` from the lucide import. Leave Settings and Admin, whose `to` values stay `/settings` and `/admin` (both redirect to their index child).

- [ ] **Step 8: Typecheck**

Run: `cd clients/web && bun run typecheck`
Expected: exit 0.

- [ ] **Step 9: Commit**

```bash
git add -A clients/web/src
git commit -m "web: settings sidebar nav with skills and memory inside"
```

---

### Task 4: Admin layout

**Files:**
- Create: `clients/web/src/pages/admin/AdminLayout.tsx`
- Create: `clients/web/src/pages/admin/ModelCardsPage.tsx` (from `AdminPage.tsx`)
- Delete: `clients/web/src/pages/AdminPage.tsx`
- Modify: `clients/web/src/App.tsx`

**Interfaces:**
- Consumes: `SettingsNav`, `SettingsHeader` from Tasks 1 and 3.
- Produces: `AdminLayout()`, `ModelCardsPage()`.

- [ ] **Step 1: Create `AdminLayout.tsx`**

```tsx
import { Layers } from "lucide-react";
import { Outlet } from "react-router-dom";
import { SettingsNav, type NavItem } from "../../components/SettingsNav";

const ITEMS: NavItem[] = [
  { to: "model-cards", label: "Model cards", icon: Layers },
];

/** Operator-facing surfaces. Adding a page = one more entry in ITEMS. */
export function AdminLayout() {
  return (
    <div className="flex h-full overflow-hidden">
      <SettingsNav title="Admin" items={ITEMS} />
      <div className="min-w-0 flex-1">
        <Outlet />
      </div>
    </div>
  );
}
```

- [ ] **Step 2: Create `ModelCardsPage.tsx`**

`git mv clients/web/src/pages/AdminPage.tsx clients/web/src/pages/admin/ModelCardsPage.tsx`. Replace the `AdminPage` wrapper (lines 14–27) with:

```tsx
export function ModelCardsPage() {
  return (
    <div className="flex h-full flex-col overflow-hidden">
      <SettingsHeader
        title="Model cards"
        desc="Well-known models and their token limits, offered as autocomplete in Settings."
      />
      <div className="min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto max-w-3xl space-y-6 px-6 py-6">
          <ModelCardsSection />
        </div>
      </div>
    </div>
  );
}
```

Keep `ModelCardsSection`, `ModelCardRow`, and every `data-testid` unchanged. Fix relative imports for the new depth (`../api/client` → `../../api/client`) and import `RowLabel` from `../settings/fields`, `SettingsHeader` from `../settings/SettingsHeader`.

- [ ] **Step 3: Rewire routes**

```tsx
<Route path="admin" element={<AdminLayout />}>
  <Route index element={<Navigate to="model-cards" replace />} />
  <Route path="model-cards" element={<ModelCardsPage />} />
</Route>
```

- [ ] **Step 4: Typecheck**

Run: `cd clients/web && bun run typecheck`
Expected: exit 0.

- [ ] **Step 5: Commit**

```bash
git add -A clients/web/src
git commit -m "web: admin sidebar nav with model cards page"
```

---

### Task 5: End-to-end coverage

**Files:**
- Modify: `clients/web/e2e/k-model-cards.spec.ts:11,57`
- Create: `clients/web/e2e/l-settings-nav.spec.ts`

**Interfaces:**
- Consumes: `test`, `expect`, and the `appBase` fixture from `./fixtures`; the `settings-nav-*` and `settings-save` testids from Tasks 1 and 3.

- [ ] **Step 1: Update the model-cards spec paths**

Line 11: `await page.goto(`${appBase}/admin/model-cards`);`
Line 57: `await page.goto(`${appBase}/settings/models`);`

- [ ] **Step 2: Write `l-settings-nav.spec.ts`**

```ts
// Settings/admin navigation: the nav column, deep links, legacy redirects, and
// the unsaved-changes guard on the batched-save pages.

import { test, expect } from "./fixtures";

test.describe("settings navigation", () => {
  test("index redirects to Models and the nav lists every page", async ({
    page,
    appBase,
  }) => {
    await page.goto(`${appBase}/settings`);
    await expect(page).toHaveURL(/\/settings\/models$/);

    const nav = page.getByTestId("settings-nav");
    for (const label of [
      "Models",
      "Runtimes",
      "Skills",
      "Memory",
      "Integrations",
    ]) {
      await expect(nav.getByRole("link", { name: label })).toBeVisible();
    }

    await nav.getByTestId("settings-nav-runtimes").click();
    await expect(page).toHaveURL(/\/settings\/runtimes$/);
    await expect(
      page.getByRole("heading", { name: "Runtimes", level: 1 }),
    ).toBeVisible();

    await nav.getByTestId("settings-nav-memory").click();
    await expect(page).toHaveURL(/\/settings\/memory$/);
    await expect(
      page.getByRole("heading", { name: "Memory", level: 1 }),
    ).toBeVisible();
  });

  test("legacy top-level paths redirect into settings", async ({
    page,
    appBase,
  }) => {
    await page.goto(`${appBase}/skills`);
    await expect(page).toHaveURL(/\/settings\/skills$/);

    await page.goto(`${appBase}/memory`);
    await expect(page).toHaveURL(/\/settings\/memory$/);

    await page.goto(`${appBase}/admin`);
    await expect(page).toHaveURL(/\/admin\/model-cards$/);
  });

  test("leaving a page with unsaved edits prompts first", async ({
    page,
    appBase,
  }) => {
    await page.goto(`${appBase}/settings/models`);
    await page.getByRole("button", { name: "Add model" }).click();
    await expect(page.getByTestId("settings-save")).toBeEnabled();

    // Dismiss: stay put.
    page.once("dialog", (d) => d.dismiss());
    await page.getByTestId("settings-nav-runtimes").click();
    await expect(page).toHaveURL(/\/settings\/models$/);

    // Accept: navigate and drop the edit.
    page.once("dialog", (d) => d.accept());
    await page.getByTestId("settings-nav-runtimes").click();
    await expect(page).toHaveURL(/\/settings\/runtimes$/);
  });
});
```

- [ ] **Step 3: Run the two specs**

Run:
```bash
cd clients/web && bun run test:e2e -- k-model-cards.spec.ts l-settings-nav.spec.ts
```
Expected: all tests pass. First run builds the Rust binaries; re-runs can use `HORSIE_E2E_SKIP_BUILD=1`.

- [ ] **Step 4: Run the whole e2e suite**

Run: `cd clients/web && HORSIE_E2E_SKIP_BUILD=1 bun run test:e2e`
Expected: green. Investigate any spec that navigated to `/settings` or `/admin` and now lands on a redirect target.

- [ ] **Step 5: Commit**

```bash
git add clients/web/e2e
git commit -m "web: e2e coverage for settings and admin navigation"
```

---

### Task 6: Manual verification and PR

- [ ] **Step 1: Build**

Run: `cd clients/web && bun run build`
Expected: exit 0.

- [ ] **Step 2: Drive the UI**

Start `horsie-server` plus `bun run dev`, then walk: each of the five settings pages renders with the sessions sidebar still on the left; saving a model persists across reload; saving a velos vendor does not wipe providers; `/admin` lands on Model cards; `/skills` redirects.

- [ ] **Step 3: Open the PR**

```bash
git push -u origin feat/settings-admin-nav
gh pr create --title "Settings and admin navigation" --body "<one line per paragraph, no hard wrapping>"
```

- [ ] **Step 4: Confirm CI is green**

Run: `gh pr checks --watch`

---

## Self-Review

**Spec coverage:** routes → Tasks 2–4; component table → Tasks 1–4 (every file in the spec's table is created by a task); save semantics → Task 2 (both PUT slices, validation split, restart banner on Runtimes); unsaved-changes guard → Task 3 Steps 1, 2, 6; sidebar footer → Task 3 Step 7; tests → Task 5; non-goals respected (no autosave, no second admin page, no responsive work, no server changes).

**Placeholders:** the two `{/* … copied verbatim from … */}` markers in Task 2 Step 1 are deliberate move instructions with exact source line ranges, not undefined work.

**Type consistency:** `NavItem` is defined once in `SettingsNav.tsx` and imported by both layouts; `usePublishDirty`/`useSettingsDirty`/`SettingsDirtyProvider` names match between `dirty.tsx`, `SettingsNav.tsx`, and the two batched pages; `SettingsHeader`'s prop names (`title`, `desc`, `dirty`, `saved`, `saving`, `onSave`, `onDiscard`) are used identically in Tasks 2–4.
