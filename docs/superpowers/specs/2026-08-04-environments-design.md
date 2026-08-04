# Environments — experimental first-class concept

Status: experimental. The goal is to explore how far the "environment" concept
can go; this first step deliberately wires nothing up.

An **environment** is a named, reusable bundle of **runtime + repos** (plus
env vars and provision steps). It is CRUD over the API, persisted in the
config database, and listed in the web sidebar between Agents and Routines.
No session, agent preset, or routine references an environment yet.

## Decisions (from brainstorming)

- The runtime half is a **vendor name plus extra runtime config**. Env vars
  and provision steps are included in this step even though nothing consumes
  them yet.
- **Local runtime is not supported**: `vendor` is required and `"local"` is
  rejected at the API boundary. (Agent presets deliberately name no vendor at
  all — "where the work runs belongs to the invocation". Environments explore
  the opposite pole: the runtime *is* the saved thing.)
- **Env vars are plain text, non-sensitive.** A secrets concept comes later;
  it is not this step.
- Full CRUD web UI: list page + create/edit form, cloned from the agents pages.
- Approach: mirror the agents stack (store / service / http / pages) so the
  experiment is isolated and already production-shaped if it succeeds.

## Protocol model — `models/fluorite/environments.fl`

New fluorite package `environments`, reusing existing types:

```florite
package environments;

use session_api.RepoConfig;
use executor.EnvVar;
use executor.ProvisionStep;

/// An environment as shown to clients: a named runtime + repos bundle.
struct EnvironmentView {
    /// Slug; the id of record, used in API paths.
    name: String,
    description: String,
    /// Runtime vendor name. Required, and never "local": environments only
    /// target vendor-managed, provisionable runtimes.
    vendor: String,
    /// Repositories cloned into the runtime workspace at provision time.
    repos: Vec<RepoConfig>,
    /// Plain-text, non-sensitive env vars for the runtime. Secrets are a
    /// future, separate concept.
    env_vars: Vec<EnvVar>,
    /// Setup steps the runtime executes before its message loop. Inert today:
    /// nothing provisions from an environment yet.
    provision: Vec<ProvisionStep>,
    /// Unix epoch seconds.
    created_at: String,
    updated_at: String,
}

/// Create or fully replace an environment. Omitted list fields default to
/// empty; `description` defaults to "".
struct EnvironmentInput {
    name: String,
    description: Option<String>,
    vendor: String,
    repos: Option<Vec<RepoConfig>>,
    env_vars: Option<Vec<EnvVar>>,
    provision: Option<Vec<ProvisionStep>>,
}
```

Generated Rust lands in `horsie_models::models::environments`; TS in
`clients/web/src/generated/environments`. The web client's `generate-types`
script in `clients/web/package.json` lists `.fl` files explicitly — add
`environments.fl` there.

## Storage — migration `0019_environments.sql` (sqlite + postgres mirrors)

(Planned as 0018; renumbered because `0018_agent_vendor.sql` landed first.)

Follows `0015_agents.sql`: list-typed columns are JSON text. `repos` elements
are `{"url", "git_ref"?, "dir"?}`; `env_vars` are `{"name", "value"}`;
`provision` are `{"name", "uses", "with": [{"key", "value"}]}`.

```sql
CREATE TABLE environments (
    name        TEXT PRIMARY KEY,
    description TEXT NOT NULL DEFAULT '',
    vendor      TEXT NOT NULL,
    repos       TEXT NOT NULL DEFAULT '[]',
    env_vars    TEXT NOT NULL DEFAULT '[]',
    provision   TEXT NOT NULL DEFAULT '[]',
    created_at  TEXT NOT NULL,              -- unix epoch seconds
    updated_at  TEXT NOT NULL
);
```

Protocol types are not storage types: `EnvironmentRow` in the store is a
hand-written struct with typed `Vec` fields; the JSON mapping lives only in
the store, as `agents/store.rs` does.

## Server

- `server/src/environments/mod.rs`, `store.rs`, `service.rs` — same split as
  `agents/`. The store does SQL + JSON mapping; the service does validation
  and wire↔storage mapping.
- Validation (service layer):
  - `name`: non-empty after trim (same rule as agents/routines).
  - `vendor`: non-empty after trim, and `!= "local"` (case-sensitive; `"local"`
    is the reserved built-in vendor name).
  - No connectivity check against live vendors — an environment can name a
    vendor that is currently offline, matching how agent presets treat vendor.
- `server/src/http/environments.rs`:
  - `GET /api/environments` → list, ordered by name.
  - `POST /api/environments` → create; 409-style error when the name is taken
    (insert, never upsert).
  - `GET /api/environments/:name` → one; 404 when missing.
  - `PUT /api/environments/:name` → full replace of the definition (name in
    path is the id; body's `name` must match); 404 when missing.
  - `DELETE /api/environments/:name` → delete; unconditional, since nothing
    references environments yet. 404 when missing.
  - Errors use the existing `ApiError` envelope and the same status-code
    conventions as `http/agents.rs` / `http/routines.rs`.
- Wiring is limited to: `http/mod.rs` routes + `State`, and
  `bin/horsie-server/main.rs` constructing `EnvironmentService`/`Store` next to
  routines.

## Web client

- `clients/web/src/pages/environments/EnvironmentsPage.tsx` — list page cloned
  from `AgentsPage`: name, description, vendor, repo count; new/edit/delete
  affordances.
- `clients/web/src/pages/environments/EnvironmentEditPage.tsx` — create/edit
  form cloned from `AgentEditPage`, trimmed to: name, description, vendor
  (text input; no picker in this step), repos as editable rows (url +
  optional gitRef; `dir` is supported by the API but the form omits it —
  the agents GitHub-checkbox picker is
  gated on the server default vendor's provisioning capability and GitHub
  connection, neither of which fits a named-vendor form), env-vars key/value
  rows, provision steps as a JSON textarea (raw `uses`/`with` structure — a
  structured editor is not worth it for an inert field).
- API client functions + hooks mirroring the agents ones in `api/client.ts`
  and `hooks/`.
- Routes in `App.tsx`: `/environments`, `/environments/new`,
  `/environments/:name/edit`.
- `Sidebar.tsx`: a `PrimaryLink` "Environments" between Agents and Routines
  (icon: `Container` or similar from lucide).

## Explicitly out of scope

- Sessions, agent presets, or routines referencing an environment.
- Any provisioning behavior; stored `provision`/`env_vars`/`repos` are inert.
- Secrets on environments.
- Vendor picker / capability-driven UI.
- CLI support.

## Testing

- Store unit tests (in-file `#[cfg(test)]`): CRUD round-trips, duplicate-name
  rejection, replace-miss returns false, JSON shape errors surface as errors
  (never silently defaulted), both sqlite dialect tests via `db::testing`.
- Service unit tests: vendor validation (`""`, `"local"` rejected), defaults
  for omitted optional fields.
- An `environments_crud_over_http` test in `server/src/http/mod.rs`'s test
  module, mirroring `routines_crud_over_http`: 201 on create, 409 duplicate,
  422 invalid (bad slug, empty/`"local"` vendor, rename via body), 404 on
  unknown names, 204 on delete.
- Web: page tests mirroring `AgentsPage.test.tsx` / `RoutinesPage.test.tsx`.

## Pre-PR checks

`cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt --check`,
`cargo test --workspace`, plus `npm run typecheck` and `npm run test:unit` in
`clients/web`.
