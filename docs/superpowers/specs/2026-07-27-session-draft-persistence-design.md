# Persist new-session draft in the browser

**Status:** approved (design) — pending spec review
**Date:** 2026-07-27
**Scope:** `clients/web` only. No server or protocol changes.

## Problem

The new-session config bar (`SessionConfigBar`) lets the user pick a runtime
(vendor), model, repos, skill bundles, MCP servers, and memory spaces. All of
that state lives in `useSessionDraft` as plain `useState`, so it is lost on
every page reload or new tab — the user re-picks the same setup each time.

## Goal

Remember the last new-session setup per browser: restore the previous draft
when the user opens a new-session page, and keep it up to date continuously.

Non-goals: cross-device sync (would require server-side storage), per-session
draft history, sharing drafts between tabs in real time.

## Decisions (from brainstorming)

- **Live draft, not last-created session.** Persist on every selection change;
  the restored state survives reload even if no session was ever created.
- **localStorage, per-browser.** Matches the existing `useUiSettings` /
  `useTheme` pattern; no server changes.
- **Silently drop stale entries on restore.** A persisted vendor/model that no
  longer exists falls back to server defaults; persisted skills/MCP/repos/
  memory spaces are filtered to what still exists. No warning UI.

## Approach

Generic `usePersistentState` hook + reconciliation in `useSessionDraft`.
(Alternative considered: persistence code inline in `useSessionDraft` —
rejected in favor of a reusable primitive.)

## Design

### New hook: `clients/web/src/hooks/usePersistentState.ts`

```ts
function usePersistentState<T>(
  key: string,
  initial: T,
  options?: {
    serialize?: (value: T) => unknown;
    deserialize?: (raw: unknown) => T | undefined;
  },
): [T, (next: T) => void]
```

- Drop-in `useState` replacement. Initial value hydrates lazily from
  `localStorage` (JSON under `key`); every set writes through.
- Corrupt JSON, missing key, or `deserialize` returning `undefined` → fall
  back to `initial` (and treat as "no stored value").
- Optional `serialize`/`deserialize` so `Map`/`Set` fields can be stored as
  plain objects/arrays.
- No cross-tab sync: last write wins, same as `useUiSettings` today.

### Modified: `clients/web/src/hooks/useSessionDraft.ts`

- The whole draft is stored as a single versioned payload under one
  localStorage key, `horsie-session-draft`. `useSessionDraft` holds the draft
  object in one `usePersistentState` call and exposes the same field-level
  getters/setters as today (setters replace one field inside the object).
  Stored format:

```json
{
  "v": 1,
  "vendor": "local",
  "model": "sonnet",
  "repos": { "owner/repo": "" },
  "skills": ["bundle-a"],
  "mcp": ["server-x"],
  "memorySpaces": ["horsie"]
}
```

  Unknown `v` values are discarded entirely (first-visit behavior), so future
  shape changes never break old clients.

- Whether a stored draft **existed** is tracked explicitly: loading returns
  `undefined` for absent/corrupt/wrong-version payloads, and that signal —
  not a value comparison — is what suppresses `enabledDefault` skill seeding.
  (A stored draft that happens to equal the defaults must still suppress
  seeding.)

- The stored payload is plain JSON (strings, arrays, objects) — `Map`/`Set`
  never touch localStorage. `useSessionDraft` converts between the payload
  shape and the `Map`/`Set` fields in its getters/setters.

- The `SessionDraft` public interface and `buildRequest()` are unchanged;
  `SessionConfigBar`, the composer, and the API layer are untouched.

### Reconciliation (replaces today's seed-once logic)

Applied once the relevant server data loads:

- **Model / vendor** — keep the stored value only if it still exists
  (`settings.models` / active vendors); otherwise reset to the first model /
  `settings.defaultVendor`. (This matches the existing effect exactly.)
- **Skills** — if a stored draft exists, use the stored selection as-is
  (filtered to installed bundles once they load) and do **not** re-seed from
  `enabledDefault`. Seeding from `enabledDefault` happens only on a true first
  visit (no stored draft). A stored empty selection stays empty — the user
  deliberately unchecked everything.
- **MCP** — filter the stored selection to enabled servers once they load.
- **Memory spaces** — filter the stored selection to existing spaces once
  `useMemorySpaces()` loads.
- **Repos** — kept as stored. The repo list is user-specific and lazy-loaded;
  unknown repos simply don't render a checkbox, matching today's behavior.

### Edge cases

- Corrupt/unparseable JSON → ignore, behave as first visit.
- Unknown payload version → discard, behave as first visit.
- Multiple tabs → last write wins (consistent with `useUiSettings`).
- First-ever visit (no stored draft) → current behavior unchanged: default
  vendor, first model, `enabledDefault` bundles pre-selected.

## Testing

The web client has no unit-test runner today (Playwright e2e only, under
`clients/web/e2e`); introducing one is out of scope. To keep logic verifiable
via e2e, serialization and reconciliation are implemented as small pure
functions that `useSessionDraft` wires together.

E2E coverage (Playwright, in a new `m-draft-persistence.spec.ts` under
`clients/web/e2e`):

- Change selections in the config bar (runtime, model, skills, MCP, memory),
  reload the page → all selections restored.
- Stored draft with an empty skills selection → stays empty after reload
  (seeding does not re-add `enabledDefault` bundles).
- No stored draft (fresh `localStorage`) → current first-visit behavior:
  default vendor, first model, `enabledDefault` bundles pre-selected.
- Stored draft referencing a since-removed model/vendor → falls back to
  defaults without errors.
