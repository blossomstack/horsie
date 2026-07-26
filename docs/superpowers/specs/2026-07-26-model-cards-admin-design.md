# Model cards: admin-managed catalog with startup seeding

Date: 2026-07-26
Status: approved, ready for planning

## Problem

A model's `context_window` and `max_tokens` live on the per-alias rows of the
`models` table (`server/migrations/0001_init.sql:14-19`,
`0007_model_context_window.sql`), edited by hand in the Settings page. When a
user adds a model and omits `context_window`, the only help they get is a
hardcoded six-entry substring table, `default_context_window()`
(`server/src/config/store.rs:602-619`), applied once at write time. There is
no shared, maintainable knowledge of "common models and their limits" — every
user re-enters the same numbers, and the built-in guesses can only be changed
by editing Rust.

We want a **model card catalog**: reference records of well-known models
(official `model_id` + display name + `context_window` + `max_tokens`),
seeded with sensible defaults at server startup, managed by the operator
through a new `/admin` page and admin API, and consumed by the Settings model
form for autocomplete + prefill.

## What the code actually says

Verified against the tree at `origin/main` (47614b4):

- **`models` table**: `alias PK, provider, model_id, max_tokens?,
  context_window?` (`server/migrations/0001_init.sql:14-19`,
  `0007_model_context_window.sql`). Row struct `ModelRow`
  (`server/src/config/store.rs:547-553`); wire types `ModelView`/`ModelInput`
  (`models/fluorite/settings.fl:19-30,125-131`).
- **`context_window` is display-only.** It flows DB →
  `GET /api/sessions/:id/usage` (`server/src/http/handlers.rs:304-323`) →
  `AgentUsageView.context_window` →
  `clients/web/src/components/ContextStatsPanel.tsx:79-122`. No compaction or
  budgeting consumes it.
- **`max_tokens` is a generation cap** baked into provider instances at
  registry build (`store.rs:625-660`), falling back to
  `DEFAULT_MAX_TOKENS = 16_384` in both providers
  (`providers/anthropic/src/lib.rs:21,308-311`,
  `providers/openai/src/lib.rs:26,166-169`).
- **Config updates are whole-section replace.** `GET/PUT /api/config`
  (`server/src/http/config.rs:11-32`) round-trips the entire `SettingsView`;
  `DbConfigStore::update()` (`store.rs:378-410`) does `DELETE FROM models` +
  re-insert. Poor fit for catalog CRUD.
- **Per-resource CRUD precedent exists.** `/api/mcp/servers/:name` (per-name
  PUT/DELETE) is the shape to follow.
- **Seeding precedent exists.** Migrations seed default workflows and the
  superpowers marketplace with `INSERT OR IGNORE`.
- **No server-side config bootstrap for providers/models.** `BootConfig`
  (`server/src/bin/horsie-server/config.rs`) explicitly excludes them; server
  startup (`server/src/bin/horsie-server/main.rs:109-117`) just opens the DB.
- **No auth anywhere on `/api/*`.** Single-user, localhost-bound by default;
  the only guarded route is `GET /api/plugin-artifacts/:file`. "Admin" here
  means a management surface, not a role.
- **Web client**: React 19 + react-router 7 + react-query; routes in
  `clients/web/src/App.tsx:23-30`, sidebar in
  `clients/web/src/components/Sidebar.tsx:123-135`; Settings model form with
  `ModelDraft`/`ModelRow` in `clients/web/src/pages/SettingsPage.tsx`
  (`:57-63,:808-864`). Types are fluorite-codegen'd
  (`bun run generate-types`).

## Decisions (from brainstorming)

- **Cards are a prefill catalog, not a source of truth.** Configured models
  keep owning their own `context_window`/`max_tokens` copies; card edits
  never propagate to existing models. No FK, no join.
- **Minimal fields**: `model_id`, `name`, `context_window`, `max_tokens`.
  Nothing else (no provider kind, capabilities, pricing).
- **`model_id` is the card's identity** — the official provider model id,
  enforced unique as the table PK. `name` is a display label only.
- **Seeding: bundled defaults, seed-if-missing.** Embedded JSON in the server
  binary, `INSERT OR IGNORE` at startup; admin edits are never overwritten on
  restart. Optional operator seed file merged the same way.
- **One public read endpoint**, prefix-search on `model_id`, serving both
  autocomplete and prefill in the Settings form. All mutations (and the admin
  list) live under `/api/admin/model-cards`.
- **New `/admin` page**, structured in sections so future admin settings slot
  in; model cards is the first section.
- **No auth gating** — consistent with the rest of `/api/*`.
- `default_context_window()` stays as the last-resort fallback for manually
  added, card-less models. Untouched.

## Scope

In scope:

- `model_cards` table + migration, store module, startup seeding (bundled +
  optional file), public prefix-search endpoint, admin CRUD endpoints,
  fluorite schema + codegen, `/admin` page with model-cards section, Settings
  form autocomplete/prefill, tests at store/HTTP/e2e layers.

Out of scope:

- Propagating card edits into configured models (cards are copies at prefill
  time, by design).
- Auth/roles for the admin surface.
- Removing or reworking `default_context_window()`.
- Cards carrying pricing/capabilities/provider-kind metadata.
- Making `context_window` drive compaction or budgeting (still display-only).

## Design

### Data model & storage

New migration `server/migrations/0008_model_cards.sql`:

```sql
CREATE TABLE model_cards (
    model_id TEXT PRIMARY KEY,   -- official provider model id, e.g. "claude-sonnet-4-5"
    name TEXT NOT NULL,          -- display label, e.g. "Claude Sonnet 4.5"
    context_window INTEGER,      -- nullable
    max_tokens INTEGER,          -- nullable
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
```

New module `server/src/config/model_cards.rs`: `ModelCardStore` over the same
sqlx pool as `DbConfigStore`, exposing `list()`, `search_by_prefix(prefix,
limit)`, `get(model_id)`, `insert(card)`, `update(model_id, name,
context_window, max_tokens)`, `delete(model_id)`, and
`seed_if_missing(cards)` (batched `INSERT OR IGNORE`). Deliberately separate
from `DbConfigStore`/`SettingsView`: cards are reference data, not runtime
config — no registry rebuild, no `SettingsUpdate` involvement.

### Seeding

- Bundled defaults: `server/src/config/model_cards_seed.json`, embedded via
  `include_str!`. Starter set covers the models today's
  `default_context_window()` table knows (current Claude opus/sonnet/haiku
  ids, gpt-4o, gpt-4.1, o1, o3, deepseek) with their real limits.
- At startup in `server/src/bin/horsie-server/main.rs`, after `sqlx::migrate!`:
  parse the embedded JSON, `seed_if_missing`. Failures to parse the bundled
  file are a startup error (it's compiled in — a broken seed is a build-time
  bug); DB errors during seeding are logged as warnings and do not prevent
  startup (admin page remains usable to fix state).
- Optional operator seed file: `--model-cards-seed <path>` CLI flag (env
  `HORSIE_MODEL_CARDS_SEED`) on `horsie-server`; same JSON array shape, merged
  with the same `INSERT OR IGNORE` semantics after the bundled defaults.
  Unreadable/invalid operator file → startup error (operator-supplied input
  should fail loud).

### API

Fluorite schema `models/fluorite/model_cards.fl` with `ModelCard` (the stored
record, including timestamps) and `ModelCardInput` (`model_id`, `name`,
`context_window`/`max_tokens` as `Option<u32>`); Rust via
`models/build.rs`, TS via `bun run generate-types`.

Public:

- `GET /api/model-cards?prefix=<s>` → `ModelCard[]`, `model_id` prefix match
  (`LIKE '<s>%'`, escaped), ordered by `model_id`, limit 50. Empty/absent
  `prefix` → all cards (same cap). This one endpoint serves both Settings
  behaviors (autocomplete list + exact-match prefill).

Admin (under `/api/admin`, the prefix future admin APIs share):

- `GET /api/admin/model-cards` → full list (kept separate from the public
  endpoint so admin-only fields can be added later without touching the
  public contract).
- `POST /api/admin/model-cards` — create from `ModelCardInput`;
  409 on duplicate `model_id`.
- `PUT /api/admin/model-cards/:model_id` — update `name`/`context_window`/
  `max_tokens`; `model_id` itself is immutable (rename = delete + create);
  404 if absent.
- `DELETE /api/admin/model-cards/:model_id` — 404 if absent.

Handlers in `server/src/http/model_cards.rs` (public) and
`server/src/http/admin.rs` (admin, new home for future admin handlers);
routes registered in `server/src/http/mod.rs`. Errors via the existing `Api`
error type: 404 unknown card, 409 duplicate, 422 validation (empty
`model_id`/`name`, zero/negative limits).

### Web UI

- **`/admin` page**: new route in `App.tsx` + "Admin" sidebar entry
  (`Sidebar.tsx`), placed after Settings. The page is a sectioned shell
  (heading + stacked sections) so future admin settings add a section, not a
  redesign. v1 contains the **Model cards** section: a table (name, model_id,
  context window, max tokens) with an add/edit modal and delete confirmation,
  built with the same react-query hooks + modal patterns as the Settings
  sections. Edit does not allow changing `model_id`.
- **Settings model form** (`SettingsPage.tsx`): the `model_id` text input
  gains debounced autocomplete backed by `GET /api/model-cards?prefix=` —
  a small suggestion list (card name + limits) under the input. Selecting a
  suggestion sets `model_id` and fills `context_window`/`max_tokens` **only
  where the field is currently empty**; all fields remain editable. No
  card→model link is stored; prefill is a one-time copy.
- Types: regenerate via `bun run generate-types`.

### Error handling

- Duplicate `model_id` on create → 409 with a clear message; UI surfaces it
  inline in the modal.
- Seed file problems: bundled = startup panic-free error (compile-time
  adjacency makes this a dev bug); operator file = loud startup error.
- Prefix search with special SQL wildcard chars (`%`, `_`) escaped so they
  match literally.
- Deleting a card that models were prefilled from: no effect on those models
  (copies, by design) — the delete confirmation states this.

### Testing

- **Store** (`model_cards.rs` tests): CRUD round-trip; duplicate insert
  rejected; `seed_if_missing` twice preserves edited rows; prefix search
  ordering/limit/wildcard-escaping; update of unknown id errors.
- **Seeding**: startup seeds bundled defaults into an empty DB; restart does
  not overwrite admin edits; operator seed file merges; invalid operator file
  fails startup.
- **HTTP**: route tests for all endpoints — happy paths plus 404/409/422 —
  following existing handler test patterns.
- **E2E** (`clients/web/e2e/`, Playwright): admin page CRUD flow (add →
  edit → delete visible in table, survives reload); Settings form
  autocomplete suggests on typed prefix and prefills empty limit fields on
  selection.
