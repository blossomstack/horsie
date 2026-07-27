# Settings & Admin navigation redesign

**Date:** 2026-07-27
**Scope:** `clients/web` only — no server, schema, or fluorite changes.

## Problem

Configuration is scattered across four sibling top-level pages (`/settings`, `/skills`,
`/memory`, `/admin`), reachable only from four chips crammed into the sessions-sidebar
footer. `/settings` itself is a single 1568-line file holding six unrelated sections
behind one global Save button, so an edit to a provider and an edit to a velos vendor
share one dirty flag and one PUT. Adding a second admin surface, or a seventh settings
section, makes both problems worse.

## Goals

- Skills and Memory become settings sub-pages, not top-level pages.
- Settings gets a nav column; each nav item is its own page with its own save state.
- Admin gets the same layout, with Model cards as its only page for now.
- `SettingsPage.tsx` is decomposed into files small enough to reason about.

## Routes

```
/                             SessionsLayout (unchanged)
  index                       NewSessionView
  sessions/:id                SessionView
  settings                    SettingsLayout  → SettingsNav + <Outlet/>
    index                     <Navigate replace to="models">
    settings/models           Models & providers        (own Save/Discard)
    settings/runtimes         Runtimes                  (own Save/Discard)
    settings/skills           Skills                    (self-saving rows)
    settings/memory           Memory                    (self-saving rows)
    settings/integrations     GitHub + MCP + Server info (self-saving)
  admin                       AdminLayout     → SettingsNav + <Outlet/>
    index                     <Navigate replace to="model-cards">
    admin/model-cards         Model cards
  skills                      <Navigate replace to="/settings/skills">
  memory                      <Navigate replace to="/settings/memory">
```

Both layouts nest inside `SessionsLayout`, producing three columns: sessions sidebar
(`w-72`), settings/admin nav (`w-52`), page content. The old `/skills` and `/memory`
paths redirect permanently so existing bookmarks keep working.

## Components

`SettingsNav` is a single component taking `{ items: { to, label, icon }[] }`. Settings
passes five items, Admin passes one. A second admin page is a one-line array addition.

New `clients/web/src/pages/settings/`:

| File | Contents |
| --- | --- |
| `SettingsLayout.tsx` | nav column, `<Outlet/>`, dirty-guard context provider |
| `ModelsSettings.tsx` | Providers + Models sections, drafts, validation, save |
| `RuntimesSettings.tsx` | default-vendor card, Velos rows, connection tests, save |
| `IntegrationsSettings.tsx` | `GithubSection`, `McpSection`, `ServerInfoCard` |
| `SkillsSettings.tsx` | today's `SkillsPage` body |
| `MemorySettings.tsx` | today's `MemoryPage` body |
| `fields.tsx` | shared `Section`, `RowLabel`, `TextField`, `RowShell` primitives |
| `SettingsHeader.tsx` | title, description, dirty/saved indicator, Discard, Save |

New `clients/web/src/pages/admin/`: `AdminLayout.tsx`, `ModelCardsPage.tsx`.

`fields.tsx` absorbs the `RowLabel`/`TextField` copies currently duplicated in
`SettingsPage.tsx`, `SkillsPage.tsx`, and `AdminPage.tsx`; all consumers switch to the
shared versions. The old `SettingsPage.tsx`, `SkillsPage.tsx`, `MemoryPage.tsx`, and
`AdminPage.tsx` are deleted — no shim re-exports.

Row components move with their pages: `ProviderRow`, `ModelRow`, `ModelIdField` to
`ModelsSettings.tsx`; `VelosRow`, `VendorsCard` to `RuntimesSettings.tsx`; `GithubMcpToggle`,
`McpServerRow`, `FieldRow` to `IntegrationsSettings.tsx`.

## Save semantics

`SettingsUpdate` has all-optional fields and each present field fully replaces that
collection, so the split is safe:

- Models & providers page PUTs `{ providers, models }`.
- Runtimes page PUTs `{ vendors, defaultVendor }`.

Neither touches the other's slice. Each page keeps its own draft state, seeds it from
`useSettings()` on load and after a successful save, and renders `SettingsHeader` with
its own dirty flag — the same behaviour as today's single header, scoped down.

Validation moves with the fields it guards: provider-name and model-alias uniqueness,
numeric `maxTokens`/`contextWindow` to the Models page; velos name/URL/image/advertise
and numeric CPU/memory/timeout checks to the Runtimes page. The `restartRequired` banner
renders on the Runtimes page, where the fields that trigger it live.

Skills, Memory, Integrations, and Model cards already save per row or per section; they
are moved unchanged and render `SettingsHeader` without Save/Discard buttons.

## Unsaved-changes guard

`SettingsLayout` provides a context with `{ dirty, setDirty }`. `ModelsSettings` and
`RuntimesSettings` publish their dirty flag to it; `SettingsNav` links intercept clicks
and call `window.confirm("Discard unsaved changes?")` when dirty, cancelling navigation
on refusal. Pages clear the flag on unmount.

`useBlocker` is deliberately not used: react-router v7 only supports it under a data
router, and this app mounts `<BrowserRouter>` + `<Routes>`. Migrating the router for one
confirm prompt is out of scope.

## Sidebar footer

The four chips become two: `Settings` and `Admin`, alongside the session count and theme
toggle. Skills and Memory are reached through the settings nav. The transcript's gear
popover (thinking visibility) is unchanged and stays in the transcript header.

## Tests

- `e2e/k-model-cards.spec.ts`: update `/admin` → `/admin/model-cards`, `/settings` →
  `/settings/models`.
- New `e2e/l-settings-nav.spec.ts`:
  - `/settings` redirects to `/settings/models` and the nav lists the five pages.
  - Clicking each nav item renders that page; deep-linking to `/settings/runtimes` works.
  - `/skills` redirects to `/settings/skills`.
  - Editing a provider name then clicking another nav item raises the confirm dialog;
    dismissing it keeps you on the page, accepting it discards and navigates.
- `bun run build` (typecheck) and existing lint/format checks stay green.

## Non-goals

- No change to what any setting does, only to where it lives.
- No autosave conversion for provider/model/velos rows.
- No second admin page.
- No responsive/mobile collapse work for the third column.
