# Live vendor activation + inline-only secrets

- **Date:** 2026-07-17
- **Status:** Design — approved
- **Branch (planned):** `live-vendor-activation`

## Summary

Two related changes to the session server's settings/vendor system, prompted
by a homelab deploy where dropping `KIMI_API_KEY`/`VELOS_TOKEN` from the
container env crash-looped the server (the DB still referenced those env-var
names) and then, once fixed, editing the `velos` vendor's settings showed
"needs a server restart to become active":

1. **Inline-only secrets** — remove `api_key_env`/`token_env` support
   entirely (schema, wire types, web UI). A value already stored inline in the
   settings DB is the only way to configure a provider's API key or a vendor's
   token; the env-var indirection is a second, silently-broken-until-restart
   failure mode with no remaining benefit once the DB path is the supported
   one.
2. **Live vendor activation** — adding a vendor, or bringing a
   previously-misconfigured/inactive vendor online, takes effect immediately
   on save. Editing an already-active vendor's non-listener settings (token,
   image, cpu/memory, http port, connect timeout) also applies immediately.
   Only a change to an *already-active* vendor's `listen`/`advertise_host`/
   `server_url` still needs a restart — rebinding a live listener safely is a
   separate, harder problem, deferred.

## Goals

- No `api_key_env`/`token_env` field anywhere in the session server's DB
  schema, wire types (`settings.fl`), or web UI — inline secrets only.
- A vendor add/edit applies live in the common case: new vendor, previously
  inactive vendor, or an active vendor's non-listener settings.
- The Settings view reports **why** a vendor is inactive (its last build
  error) instead of a boolean the operator has to guess about.
- `restart_required` narrows to mean only "an active vendor's listener
  address changed and is pending a restart" — true in one specific case, not
  on every vendor edit.

## Non-goals

- Live-changing an already-active vendor's `listen`, `advertise_host`, or
  `server_url` — still needs a restart. Safely rebinding a listener whose old
  instance may still be referenced by in-flight sessions is a distinct
  problem; not attempted here.
- Any change to the CLI's local `config.json` provider config
  (`cli/src/config.rs`) — a different product surface (no server, DB, or web
  UI involved), where the `api_key_env` convention still makes sense. Out of
  scope.
- Force-terminating sessions when their vendor is removed — an in-flight
  session keeps the `Arc<dyn RuntimeVendor>` it already holds and finishes
  normally; the vendor just stops being offered to *new* sessions.
- Migrating other deployments' data — this repo has exactly one real
  deployment (the homelab); the migration drops the `providers.api_key_env`
  column outright and the operator re-enters secrets inline via Settings.

## Current shape (verified)

- `server/src/config/store.rs`: `DbConfigStore::open()` builds the provider
  `Registry` and the vendor map **once**, handing back
  `OpenedConfig { store, registry: SharedProviderRegistry, vendors: HashMap<String, Arc<dyn RuntimeVendor>>, pool }`.
  `registry` is already `Arc<RwLock<...>>` and `update()` swaps it live
  (providers/models apply without a restart today). `vendors` is a **plain**
  `HashMap`; `active_vendors: Vec<String>` is a **frozen snapshot** computed
  once at `open()` and never revisited. `update()` only flips
  `vendors_dirty: AtomicBool` for any vendor edit, which the view reports as
  `restart_required: bool` — unconditionally, for every vendor change.
- `cli/src/serve.rs:136-138`: constructs
  `ServerDeps { vendors: opened.vendors, ... }` — the map is cloned as-is into
  `ServerDeps` (`#[derive(Clone)]`), so every session actor gets an
  independent, never-updated copy.
- `server/src/sessions/session_actor.rs:147-153`:
  `self.deps.vendors.get(&self.spec.vendor).cloned()` — a simple keyed lookup
  at provision time, not held long-term beyond one call.
- `server/src/vendor/velos.rs` `VelosVendor::bind()`: binds a
  `RuntimeListenerServer` TCP listener on `settings.listen` (fixed at
  `0.0.0.0:3790` in this deployment) and spawns its accept loop
  (`_serve_guard: DropGuard`) — the **only** resource acquired once and held
  for the vendor's life. `image`/`runtime_bin`/`workspace_root`/`cpu`/
  `memory_bytes`/`connect_timeout`/`public_http_base` are plain fields,
  cloned fresh into a `VelosRuntimeProvider` on every `provision()` call —
  nothing else is stateful.
- `server/src/config/store.rs` `build_anthropic` / `resolve_velos_token`:
  resolve an inline value first, else fall back to
  `std::env::var(api_key_env / token_env)`, erroring if the env var name is
  configured but the var is absent or empty. This is exactly why the homelab
  deploy crash-looped this session: the DB still had
  `kimi.api_key_env = "KIMI_API_KEY"` after the container env var was
  dropped, and a provider registry build failure is fatal at startup.
- `models/fluorite/settings.fl`: `ProviderView.api_key_env` / `has_inline_key`;
  `ProviderInput.api_key_env` / `api_key`; `VelosView.token_env` /
  `has_inline_token`; `VelosInput.token_env` / `token`;
  `SettingsView.restart_required: bool`.
- `clients/web/src/pages/SettingsPage.tsx`: an "env var" text input next to
  each inline-secret input, for both providers (~721-730) and velos vendors
  (~825-834), plus a static "Velos vendor changes are saved but need a server
  restart" banner keyed off `restartRequired`. The Providers section's
  description reads "Prefer an env var for the key to keep secrets out of the
  database" — no longer true once this ships.

## Design

### 1. Drop `api_key_env` / `token_env`

- Migration `server/migrations/0006_drop_api_key_env.sql`:
  `ALTER TABLE providers DROP COLUMN api_key_env;`. `token_env` isn't a
  column — it lives inside the vendor `config` JSON blob — so no schema
  change is needed there, only code.
- `server/src/config/store.rs`: remove `ProviderRow.api_key_env` and the
  `api_key_env` parameter from `build_anthropic` — it collapses to
  `Some(k) if !k.is_empty() => Some(Secret::from(k))`, `Some("") => Err(...)`,
  `None => None`. Same collapse for `VelosConfig.token_env` /
  `resolve_velos_token` (checks `vc.token` only).
- `models/fluorite/settings.fl`: remove `api_key_env`/`token_env` from
  `ProviderView`/`ProviderInput`/`VelosView`/`VelosInput`; regenerate Rust +
  TS (`ts-drift` check must stay green).
- `clients/web/src/pages/SettingsPage.tsx`: remove the "env var" input and its
  state (`apiKeyEnv`, `tokenEnv`) from both provider and velos rows; keep the
  existing inline-secret input and its "•••• stored — blank keeps it"
  placeholder pattern unchanged. Update the Providers section's description
  text (remove the now-false "prefer an env var" line).
- The homelab's current DB has `kimi.api_key_env` (already nulled out by hand
  this session, ahead of this change) and `velos.config.token_env` in the
  JSON blob — the new deserializer simply ignores the stale `token_env` key
  until the vendor is next saved, at which point it's overwritten.

### 2. Live vendor activation

- New type alias in `spec.rs`, mirroring `SharedProviderRegistry`:
  `pub type SharedVendors = Arc<RwLock<HashMap<String, Arc<dyn RuntimeVendor>>>>`.
  `ServerDeps.vendors` and `OpenedConfig.vendors` become this type.
  `cli/src/serve.rs` passes it straight through (already an `Arc`, cloning
  `ServerDeps` stays cheap). `session_actor.rs`'s `fn vendor()` becomes
  `self.deps.vendors.read().unwrap().get(...).cloned()...`. Test helpers
  (`supervisor.rs` `test_deps`, `session_actor.rs` test setup) wrap their
  `HashMap` in `Arc::new(RwLock::new(...))`.
- `DbConfigStore` holds a clone of the same `SharedVendors` (stored at
  construction, alongside `registry`) so `update()` can reach it.
  `active_vendors: Vec<String>` (frozen) is deleted — `vendors_view`'s
  `active` check and the `default_vendor` validation read the live map's
  current keys instead.
- `VelosVendor` gains an inner `RwLock<VelosMutableSettings>` holding
  everything `provision()` currently reads as plain fields (`image`,
  `runtime_bin`, `workspace_root`, `cpu`, `memory_bytes`, `connect_timeout`,
  `public_http_base`, and the `VelosClient`/token). `provision()` takes one
  read-lock at the top instead of touching `self.<field>` directly. The
  listener/`connected`/`endpoint_ws`/`_serve_guard` built once in `bind()`
  stay genuinely immutable — bound once, held for the vendor's process
  lifetime. A new `VelosVendor::reconfigure(&self, settings: VelosMutableSettings)`
  swaps the lock's contents.
- `ConfigStore::update()`'s vendor block, after persisting rows, reconciles
  the live map against the new DB state per row (the per-kind build logic
  factors out of `build_vendors`'s loop so it can run for one row instead of
  the whole table):
  - **Row not in the live map** (new, or previously failed to build) →
    attempt to build+bind it; on success, insert into the live map; on
    failure, leave it absent and record the error (see below). No restart.
  - **Row in the live map, `listen`/`advertise_host`/`server_url` unchanged**
    → call `.reconfigure(...)` on the existing instance (kind-matched the
    same way `build_vendors` already switches on `r.kind`, so the concrete
    type is known without `dyn` downcasting). No restart.
  - **Row in the live map, `listen`/`advertise_host`/`server_url` changed** →
    can't apply live; the old instance keeps running unchanged, and that one
    vendor's view reports a restart-needed error (not a global flag).
  - **Row removed from the DB** → removed from the live map. In-flight
    sessions holding an `Arc` clone keep working; the vendor stops being
    offered for new sessions immediately.
  - **Row's `kind` changed** (only one non-`local` kind exists today, so this
    is a theoretical case) → treated as remove-old + add-new, not
    `reconfigure` (a kind change can't reuse the old instance's listener).
- `VendorView` (settings.fl) gains `error: Option<String>` — the last
  build/reconfigure failure for that vendor (`None` when active or never
  attempted). `SettingsView.restart_required` narrows to mean only "an active
  vendor's listener-affecting fields changed and are pending a restart" — one
  specific case, not every vendor edit.
- Web UI: vendor rows show their per-vendor `error` inline instead of relying
  on a global banner; the banner now only appears in the one narrow
  `restartRequired` case.

## Error handling

- A per-vendor build/reconfigure failure never fails the whole
  `PUT /api/config` call — the row still persists (no losing edits mid
  troubleshooting), and the vendor stays/becomes inactive with `error`
  populated. This matches today's boot-time behavior (`build_vendors` already
  treats a vendor build failure as a non-fatal warning) — now it also runs
  per-update, not just per-boot, and is surfaced to the caller instead of only
  stderr.
- A provider's inline `api_key` being explicitly set to `""` is still a hard
  validation error on `update()`, same as today — providers apply
  synchronously to the registry that models reference directly, so there's no
  sensible "half-configured, try again later" state for them the way there is
  for a vendor.
- `reconfigure()` itself can't leave a vendor half-broken — it only swaps
  in-memory settings behind a lock. Whether the new settings actually work
  (bad token, unreachable server) surfaces on the *next* `provision()` call
  via the vendor's normal `VendorError`, exactly like today.

## Testing summary

- `server/src/config/store.rs`: update a not-yet-active vendor with a bad
  token → view shows `active: false`, `error: Some(...)`,
  `restart_required: false`; update again with a good token → `active: true`,
  `error: None`. Update an active vendor's `image`/`http_port`/`token` → the
  live map's entry reflects the new settings immediately (assert via the
  vendor's next `provision()` or a settings probe), `restart_required` stays
  false. Update an active vendor's `listen` → that vendor's `error` reports
  restart-needed, the old instance keeps serving, `restart_required: true`.
- `server/src/vendor/velos.rs`: `reconfigure()` changes what a subsequent
  `provision()` uses (image/cpu/etc.) without rebinding — assert the
  listener's bound local address is unchanged before/after.
- Migration test: `0006_drop_api_key_env.sql` applied to a DB seeded via the
  old schema succeeds and existing provider rows survive minus the column.
- Full gate (`fmt`/`clippy`/`test`/`deny`) + web build/`ts-drift` green.
  Structured as two commits (env-var removal, then live-activation) since
  they're independently reviewable and the second only loosely depends on the
  first.

## Alternatives considered

- **Full rebuild + swap the whole vendor map on every edit** (reuse
  `build_vendors()` as-is, swap the map behind an `Arc<RwLock<...>>` or
  `ArcSwap`): simpler code, reusing the existing boot path verbatim. Rejected
  — rebuilding an *already-active* velos vendor tries to rebind the same
  fixed port while the old listener may still be referenced by in-flight
  sessions holding the old `Arc<dyn RuntimeVendor>`; a real bind-conflict
  race, not a theoretical one.
- **Per-vendor supervised actor** (each configurable vendor is its own actor
  handling reconfigure messages, coordinating its own teardown/rebuild):
  more robust in the abstract, but over-engineered for two vendor kinds
  today (`local`, always-on; `velos`, one configurable instance) — the
  lock-and-swap approach gets the same safety with far less new machinery,
  and matches the codebase's existing pattern (provider registry already
  hot-swaps via `Arc<RwLock<...>>`) rather than introducing a new actor
  pattern for this.
