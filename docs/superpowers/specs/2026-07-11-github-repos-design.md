# GitHub integration: repo selection and provisioning for sessions

**Date:** 2026-07-11
**Status:** Approved design, pending implementation plan

## Overview

Connect horsie to GitHub so a user can pick zero or more repositories when
launching a session. The server converts the picks into **provision steps**,
mints a scoped GitHub token, and hands both to the runtime vendor. The runtime
executes the steps (git clone) inside its sandbox before the agent loop starts.

Reference implementation: agentx (`github_routes.rs`, `provision_from_repos()`),
adapted to horsie's single-tenant, vendor-abstracted architecture.

## Goals

- "Connect GitHub" once per deployment (OAuth + GitHub App), managed from Settings.
- Repo picker at session launch: 0..N repos, optional ref per repo.
- A generic, extensible provision-step pipeline (server → vendor → runtime);
  `git_checkout` is the only step kind for now.
- Workspace allocation becomes **vendor-owned**: local allocates a host dir,
  velos uses its in-container workspace — the server no longer needs to supply
  a path for vendor-managed sessions.

## Non-goals (deferred)

- Mixing `workdirs` and `repos` in one session.
- Mid-session token refresh (token is only needed at provision time).
- Agent-driven push/PR auth (hackamore territory).
- Per-user credentials (horsie has no user/tenant layer; connection is
  deployment-global).
- Tight network egress (github.com-only) for the clone; first cut uses
  `NetworkPolicy::Allow` on repo sessions.

## Background: current state

- Sessions are created via `POST /sessions` with `workdirs: Vec<String>`
  (existing host paths, required non-empty). `derive_workspaces()` turns them
  into named workspaces.
- `RuntimeVendor` trait (`server/src/vendor/mod.rs`): `create`/`attach` take a
  `RuntimeSpec { workspaces, capabilities_file, plugins_dir, hook_path }`.
- The **local** vendor passes host paths verbatim to `horsie-runtime`; the
  **velos** vendor drops host paths and computes `<workspace_root>/<name>`
  inside the container (host paths are silently ignored — a latent bug this
  design fixes by rejecting them).
- The runtime already has a fail-closed pre-agent provision phase
  (`runtime/src/provision.rs`, hackamore credential setup) that runs after the
  sandbox is applied and before the message loop.
- Settings are stored in the SQLite-backed `DbConfigStore`
  (`server/src/config/store.rs`) with established patterns: write-only secret
  inputs (omit = keep, `""` = clear), redacted views (`has_*` flags),
  validate-then-commit transactions, `Secret` for credentials in memory.

## 1. GitHub connection (server)

### Storage

New migration `0002_github.sql`, two single-row tables (single-tenant,
explicit columns per the `providers` style):

```sql
CREATE TABLE github_app (
    id            INTEGER PRIMARY KEY CHECK (id = 1),
    client_id     TEXT NOT NULL,
    client_secret TEXT NOT NULL,
    app_id        INTEGER,
    private_key   TEXT,            -- PEM, raw or base64
    app_slug      TEXT
);

CREATE TABLE github_credentials (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    login           TEXT NOT NULL,
    access_token    TEXT NOT NULL,
    refresh_token   TEXT,
    expires_at      TEXT,           -- RFC 3339
    installation_id INTEGER
);
```

`client_secret`, `private_key`, and tokens are `Secret` in memory and
write-only in the API (`has_client_secret` / `has_private_key` in views),
following `VelosConfig`'s pattern exactly.

### Endpoints

New `server/src/http/github.rs`, wired into the existing HTTP layer:

| Endpoint | Behavior |
|---|---|
| `GET /github/status` | `{ connected, login?, app_installed, repo_count }` |
| `GET /github/auth` | Redirect to GitHub OAuth authorize URL |
| `GET /github/callback` | Exchange code → store credentials, auto-discover `installation_id`, redirect to Settings with `github_connected=1` or `github_error=…` |
| `PUT /github/app-config` | Save App config (write-only secrets) |
| `GET /github/app-config` | Redacted view |
| `DELETE /github/disconnect` | Clear `github_credentials` |
| `GET /github/repos` | Repos visible to the installation; in-memory cache, 5 min TTL, `?refresh=1` bypass |
| `GET /github/repos/branches?repo=owner/name` | Branch list for the ref picker |

### Token minting

`generate_installation_token(app_id, pem, installation_id, repos)` — RS256
App JWT → `POST /app/installations/{id}/access_tokens` with
`repositories: [...]` scoped to **exactly the selected repo set** (least
privilege, ~1 h lifetime). Adapted from agentx; the token is never sent to the
browser.

## 2. Session launch

`CreateSessionRequest` gains `repos: Vec<RepoConfig>`:

```
struct RepoConfig {
    url: String,           // https clone URL
    git_ref: Option<String>,  // branch / tag / sha
    dir: Option<String>,   // subdir in workspace; default: repo basename, deduped
}
```

Validation and behavior:

- `workdirs` and `repos` are mutually exclusive.
- `workdirs` given → `HostDir` workspaces (today's flow, unchanged).
- Otherwise → **one `Managed` workspace**; the 0..N repos become
  `git_checkout` provision steps cloning into it. Zero repos = empty managed
  workspace (no special case).
- When repos are selected, the server mints the scoped installation token and
  injects `GITHUB_TOKEN` into the runtime env, and ensures the session's
  `CapabilitySpec` grants network egress (`NetworkPolicy::Allow` for now).
- `SessionSpec` persists workspace **sources** (not resolved paths), provision
  steps, and env, so `attach` replays them: a fresh token is minted at every
  `create` *and* `attach`.

## 3. Workspace model: vendor-owned allocation

`RuntimeSpec` (server → vendor) changes from "resolved paths" to "requests":

```rust
pub struct RuntimeSpec {
    pub workspaces: Vec<WorkspaceSpec>,   // was Vec<Workspace {name, path}>
    pub provision: Vec<ProvisionStep>,    // NEW
    pub env: Vec<EnvVar>,                 // NEW (GITHUB_TOKEN, …)
    pub capabilities_file: PathBuf,
    pub plugins_dir: Option<PathBuf>,
    pub hook_path: Vec<PathBuf>,
}

pub struct WorkspaceSpec { pub name: String, pub source: WorkspaceSource }
pub enum WorkspaceSource {
    HostDir(PathBuf),   // bring-your-own local checkout
    Managed,            // vendor allocates; server never sees a path
}
```

Per-vendor semantics:

| | `Managed` | `HostDir` |
|---|---|---|
| **local** | allocate `<workspace_root>/<runtime_id>` on host (new vendor setting, default `<state_dir>/workspaces`; deterministic so `attach` re-finds it, `delete` cleans it) | use path verbatim (today's behavior) |
| **velos** | `<workspace_root>/<name>` in-container (what it already does) | **rejected at session create** (containers cannot mount host dirs) |

The wire `RuntimeConfig` → runtime still carries `--workspace name=path`; only
*who computes the path* changes (vendor, after allocation). The sandbox stays
correct for free: the `WorkingDir` capability grant resolves against actual
workspace roots at apply time in the runtime.

## 4. Provision steps

### Wire model (fluorite)

In `models/fluorite/executor.fl` (rides `RuntimeConfig`):

```
struct ProvisionStep {
    name: String,           // display label, e.g. "checkout horsie"
    uses: String,           // step kind: "git_checkout" (only kind for now)
    with: Vec<StepParam>,   // open key/value params
}
struct StepParam { key: String, value: String }
```

`RuntimeConfig` gains `provision: Vec<ProvisionStep>`. `env` already exists.
Unknown `uses` → the runtime fails the step (fail-closed, forward-compatible).

`git_checkout` params: `url` (required), `ref` (optional), `dir` (optional,
defaults to repo basename; the server dedupes collisions).

### Runtime execution

A step interpreter (extending `runtime/src/provision.rs` or a sibling
`steps.rs`) runs **after** sandbox apply + hackamore provisioning, **after**
connecting to the executor, but **before** reporting ready — so failures flow
back over the existing `CommandFailed` path with a real error message, and the
session lands in `Failed { reason }` instead of dying silently.

`git_checkout` behavior:

- **Idempotent:** skip if `<workspace>/<dir>/.git` exists (local attach skips
  the re-clone; a fresh velos container re-clones).
- **Auth:** if `GITHUB_TOKEN` is set and the URL host is github.com, pass it
  via one-shot config (`git -c http.extraHeader=… clone`) so the token is
  never written to `.git/config` or credential files.
- `ref` set → clone then checkout; unset → default branch.
- **Timeouts:** large clones must not outrun the vendor's ready-wait (velos
  `connect_timeout`); the implementation must ensure the create-wait tolerates
  provision time. Exact fix depends on where `await_ready` sits — pinned
  during implementation planning.

## 5. Web UI (`clients/web`)

- **Settings → new "GitHub" section**, following the providers/vendors
  patterns: App config form with write-only secrets, Connect/Disconnect
  button, status line ("Connected as @login · N repos").
- **Session launch → repo picker:** multi-select from `GET /github/repos`
  (5-min cache, refresh button), optional ref per repo, zero selections
  allowed. Hidden with a "Connect GitHub in Settings" hint when not connected.
- TS models come from fluorite generation like the rest of `src/generated`.

## 6. Error handling

- OAuth errors → redirect back to Settings with `github_error` param.
- Repo-list/API failures → non-fatal, surfaced in the UI.
- velos + `HostDir` workdirs → rejected by the velos vendor at provision time
  (the session lands `Failed` with a clear "cannot mount host directory"
  reason). A 422 pre-check at create would need vendor-capability
  introspection in the HTTP layer — possible follow-up, not in this cut.
- Clone failure → `SessionStatus::Failed` with the git stderr tail in the
  reason.

## 7. Testing

- Store round-trip tests for the new tables (mirroring `store.rs` tests):
  secret keep/clear/set, redacted views.
- Unit tests for repos → steps conversion and `dir` dedup.
- Runtime `git_checkout` tests against local `file://` fixtures (no network,
  no token), plus a token-injection test asserting the secret never appears in
  the cloned repo's `.git/config` or on disk.
- Token minting against a mocked GitHub API.
- Real-clone coverage lives in a provider↔runtime integration test
  (`runtime/tests/provision_steps.rs`: real `ProcessRuntimeProvider` + real
  `horsie-runtime` binary + `file://` fixture); the server e2e
  (`tests/tests/session_server_e2e.rs`) exercises the repos-session HTTP flow
  over `MockVendor`.
