# Settings UX + config store for the horsie session server

**Status:** implemented
**Branch:** `webui-settings` (off `origin/main`)

## Problem

The session server's web UI (`clients/web`) gives no way to see or change what
the server runs with. Two concrete pain points:

1. **Session creation is guesswork.** In `NewSessionModal` the *Model* and
   *Vendor* fields were free-text, so a user had to already know the exact model
   *alias* and vendor name from the server's config. A typo produced a session
   that only failed on its first turn (HTTP 502, "no provider registered").
2. **Config was invisible and file-only.** Providers, models, and runtime
   vendors lived in `config.json`, read once at startup. Changing a model on a
   containerized/homelab deployment meant editing a bind-mounted file and
   restarting — no UI path.

## Goal

A **Settings** page that makes the frequently-changed configuration editable
from the browser, applied live where safe, plus config-driven dropdowns in the
new-session form. Keep it simple — no config-management framework.

## Two non-overlapping configs

The core decision: `config.json`/env and the settings store are **two separate
configs that are never copied or synced**.

- **`config.json` / env — deployment/bootstrap.** Storage dirs, sandbox caps,
  runtime bin, hackamore, and the **settings-DB location**. Read once at startup.
  Still drives the job daemon unchanged.
- **Settings database — the app config the UI owns.** Providers, models, velos
  vendor instances, and the default vendor. `horsie serve` reads these from the
  DB, not from `config.json` (it logs a note that any `config.json`
  providers/models/velos are ignored).

The store is **SQLite** via `sqlx`, chosen so a PostgreSQL backend is a later
driver swap behind the same trait. Migrations are embedded (`sqlx::migrate!`),
and all queries are runtime (no `DATABASE_URL` at compile time), so CI needs no
database.

## What's editable, and when it applies

- **Live (no restart):** providers, models, default vendor. On save the provider
  registry is rebuilt from the new DB state and swapped atomically, so the next
  turn of every session sees it. `build_registry` is cheap and pure.
- **Persisted, activates on restart:** velos vendor instances (0..N, each named
  and self-contained). Binding a reverse-dial listener per instance is startup
  work; live add/remove would mean dynamic listener lifecycle threaded through
  the actor tree — deferred as an additive follow-up. A `restart_required`
  banner appears after a velos edit.

Secrets (provider API keys, velos tokens) are **never returned** by the API —
redacted views expose only the env-var name and a `has_inline_*` boolean.
Write-only inputs: omit to keep the stored value, `""` to clear, a value to set.

## Architecture

The config store is a **server** concern, so it lives in the `server` crate: the
server builds providers (`horsie-anthropic`) and vendors (its own
`RuntimeVendor` impls) directly from the database. The `cli` only resolves the
deployment config from `config.json`/env — including the DB location — and hands
it to the store. This makes the bootstrap-vs-runtime split match the crate
boundary; the cli keeps `config.json` + its own `build_registry` solely for the
job daemon.

### Wire types — `models/fluorite/settings.fl` (package `settings`)

Redacted views (`ProviderView`, `ModelView`, `VendorView`, `VelosView`,
`ServerInfo`, `SettingsView`) + update inputs (`ProviderInput`, `ModelInput`,
`VendorInput`, `SettingsUpdate`). Vendors are **generic**: `VendorView`/`VendorInput`
carry a kind-tagged config union (`VendorConfigView`/`VendorConfigInput`) with a
`Velos` variant today — a new vendor kind is a new variant, no schema change.
Generated Rust lands in `horsie_models::settings`; TS is committed in both
`clients/ts` and `clients/web`. `SettingsUpdate` fields are optional — a present
field fully replaces that section, an omitted one leaves it unchanged.

### Server crate — `DbConfigStore`

- `SharedProviderRegistry = Arc<RwLock<HashMap<String, Arc<dyn LlmProvider>>>>`;
  read in `SessionActor::ensure_agent` under a short-lived guard (never across an
  `.await`), so live edits take effect next turn.
- `ConfigStore` trait (async `view`/`update`, sync `default_vendor`) + the
  `DbConfigStore` impl (sqlx SQLite, migrations in `server/migrations/`), added
  to `AppState`. `create_session` reads the default vendor from it.
- `DbConfigStore::open(db_url, deps)` opens/migrates the DB and returns the store
  plus the live registry and vendors the supervisor needs (`local` always, plus
  one built per configured vendor row). `update()` runs in a transaction: apply
  the replace-semantics edits, then **validate by building the registry** from
  the new state before committing — a bad edit rolls back untouched. On success
  it commits, swaps the live registry, and updates the default vendor. Vendor
  edits persist and set `restart_required` (they activate at next open).
- Handlers `GET /api/config` → `view()`, `PUT /api/config` → `update()`.

### DB schema (`server/migrations/`)

`providers`, `models`, a generic `vendors(name, kind, config-json)`, and a
`settings` key/value table. Secrets are stored raw in the DB; the API redacts
them.

### Web UI

- `api/client.ts` `config.get`/`config.update`; `useSettings` +
  `useUpdateSettings` hooks.
- Route `/settings` → `SettingsPage`: providers, models, and velos instances as
  inline add/edit/remove rows; a default-vendor picker; a read-only Server info
  card (config file, database, dirs, version). A gear nav entry in the sidebar.
- `NewSessionModal`: *Model* and *Vendor* become `<select>`s sourced from the
  server config; the form resets on close and reseeds a stale selection on open,
  so a since-deleted model can't be submitted.

## Testing

- Rust unit tests for `DbConfigStore` (temp SQLite): persist + live registry
  swap; inline-secret round-trip; invalid edit rolls back; velos vendor persists
  redacted and flags restart; bad default vendor rejected.
- Server handler tests drive `GET`/`PUT /api/config` against a real
  `DbConfigStore` on a temp DB.
- Web `bun run build` (tsc + vite); manual smoke of the settings API + UI.

## Verification gate

`make check` (fmt + clippy + `cargo test --workspace`), `cargo deny`, the
`ts-types` drift check, and `clients/web` `bun run build` — all green — then the
PR.
