# Velos vendor connection test

- **Date:** 2026-07-18
- **Status:** Design — approved
- **Branch:** `velos-vendor-test`

## Summary

Add a "Test connection" action for configured velos vendors: an on-demand
check that the vendor's `server_url` is reachable and its stored token is
still accepted, surfaced in the Settings UI as a per-row button plus an
automatic check right after saving. Prompted by a gap in the recently-shipped
[[2026-07-17-live-vendor-activation-inline-secrets-design]]: a velos vendor's
`active`/`error` state reflects whether its listener bound, not whether the
token actually works — a bad token isn't discovered until the first real
session tries to provision a container.

Velos already exposes exactly the primitive this needs: `GET /auth/v1/me`
(whoami) is an authenticated endpoint that returns the caller's identity or
401s, and `velosctl login` already uses it to validate a token before saving
it. This change is horsie-only — no velos repo changes.

## Goals

- An operator can check, on demand, whether a configured velos vendor's
  `server_url` is reachable and its stored token is currently valid, without
  needing to start a session.
- The check runs automatically right after saving a velos vendor, so the
  Settings page shows connection status without an extra click.
- The check is read-only: it never mutates a vendor's `active`/`error` state
  (that continues to mean "did the listener bind," a distinct concern) and
  never persists anything — it's a live probe, not a stored status.

## Non-goals

- No velos repo changes — `GET /auth/v1/me` already does the job.
- No change to `VendorView.active`/`VendorView.error` semantics or to the
  live-reconfigure machinery in `server/src/config/store.rs` /
  `server/src/vendor/velos.rs` — this is an independent, additive check.
- No testing of unsaved form edits. The test always checks the vendor's
  *saved* config, matching the existing MCP-server "Test connection" pattern.
  The Settings page already gates on a page-level `dirty` flag for this
  reason (see Design).
- No new vendor kind or generalization beyond velos — `local` has no config
  to test and is skipped; a future non-velos vendor kind would add its own
  match arm the same way `build_one_vendor` does today.

## Current shape (verified)

- `server/src/velos/client.rs`: `VelosClient` wraps a `reqwest::Client` +
  `base_url` + `Option<Secret>` token, with `auth()` attaching
  `bearer_auth`. It implements `ContainerApi` (`create_container` /
  `delete_container` / `container_phase`) — deliberately scoped to container
  lifecycle only, per its module doc comment. No request currently sets a
  timeout.
- `server/src/config/store.rs`: `resolve_velos_token(&VelosConfig) ->
  Result<Option<Secret>, String>` and `build_velos_vendor(&VelosConfig)`
  already do "read a vendor row's config, resolve its token, build a
  `VelosClient`" for the boot/live-reconfigure path. `VendorRow { name, kind,
  config }` is the raw DB row; `read_vendors()` reads all rows (no
  by-name lookup exists yet).
- `server/src/config/mod.rs`: `ConfigStore` trait (`view`, `update`,
  `default_vendor`), implemented by `DbConfigStore`.
- `server/src/http/config.rs` / `server/src/http/mod.rs`: `GET`/`PUT
  /api/config` delegate straight to `ConfigStore`. `POST
  /api/mcp/servers/:name/test` (`server/src/http/mcp.rs`,
  `server/src/mcp/service.rs::test`) is the precedent this follows: no body,
  path is the id, connects live, persists the outcome, returns a result
  envelope — always `200`, failure is `ok: false` + `error`, not an HTTP
  error.
- `models/fluorite/settings.fl`: `VendorView { name, active, is_default,
  config, error }`; no test-result type exists yet.
- `clients/web/src/pages/SettingsPage.tsx`: velos vendors are edited as a
  `VelosDraft[]` array under one page-level `save()`/`dirty` flag shared with
  providers/models/default vendor — there's no per-row save, unlike
  `McpServerRow`, which has its own `save`/`test`/`connect` per row. `VelosRow`
  (~L780) renders the fields plus `draft.error` / "Not loaded yet."
  `clients/web/src/hooks/useMcp.ts::useTestMcpServer` is the frontend
  precedent: a mutation that calls `api.mcp.test(name)` and invalidates the
  server list query.

## Design

### Backend

**`VelosClient::whoami(&self) -> Result<String, VelosError>`** — new inherent
method on `VelosClient` in `server/src/velos/client.rs` (not added to the
`ContainerApi` trait, which stays scoped to container ops so its test doubles
don't need a new no-op impl). Sends `GET {base_url}/auth/v1/me` with the
client's bearer auth and a 10s per-request timeout (`reqwest::RequestBuilder
::timeout`) — the container-lifecycle methods have no timeout today because
they run inside an already-bounded provision/attach flow, but this is a
user-triggered "ping" against a URL that might be dead, so it needs its own
bound. Maps the JSON response (`{"identity": "admin"}` or `{"identity":
{"worker": "<name>"}}`) to a display string: `"admin"` or `"worker:<name>"`.
A non-2xx status (401 for a bad token) or a transport failure (unreachable
host, DNS failure, timeout) both come back as the existing `VelosError`
variants (`Status`/`Request`) — the two read distinctly when displayed, so
"wrong token" and "wrong URL" aren't confused.

**`ConfigStore::test_vendor(&self, name: &str) -> Result<VendorTestResult,
String>`** — new trait method (`server/src/config/mod.rs`), implemented on
`DbConfigStore` (`server/src/config/store.rs`). Reads the vendor's row
straight from the DB by name (a new small `read_vendor(&pool, name) ->
Option<VendorRow>` query, or a filter over `read_vendors`), *not* from the
live `vendors`/`velos_instances` maps — this means the check still tells you
"is the token good" even when the vendor is currently inactive for an
unrelated reason (e.g. a bad `listen` address failed to bind). For a `velos`
row: deserialize `VelosConfig`, resolve the token via the existing
`resolve_velos_token`, build a throwaway `VelosClient` (never registered
anywhere, dropped after the call), call `.whoami()`. Maps the result:
- `Ok(identity)` → `VendorTestResult { ok: true, identity: Some(identity),
  error: None }`
- `Err(VelosError::Status { status: 401, .. })` → `VendorTestResult { ok:
  false, identity: None, error: Some("token rejected (401 Unauthorized)") }`
- `Err(e)` (other status, or a transport error) → `VendorTestResult { ok:
  false, identity: None, error: Some(e.to_string()) }`
- Unknown vendor name, or a row whose `kind` isn't `velos` (only `velos`
  exists today; `local` isn't a DB row at all) → `Err("unknown vendor
  '<name>'")` / `Err("vendor kind '<kind>' does not support testing")`,
  propagated as the method's outer `Result::Err`, not wrapped in
  `VendorTestResult` — this is a caller-error case (bad name), distinct from
  "the check ran and failed."

No DB write, and no interaction with `vendor_errors`/`restart_required`/the
live vendor map — this is a pure read probe, unlike the MCP `test`'s
`enabled`/`last_error` persistence, because a velos vendor's `active`/`error`
already means something else (listener bind state) and nothing downstream
reads a stored token-validity flag.

**`POST /api/config/vendors/:name/test`** (`server/src/http/config.rs`,
wired in `server/src/http/mod.rs` next to the existing `/api/config` route) →
`Json<VendorTestResult>`. Mirrors `mcp::test` exactly: `state.config_store
.test_vendor(&name).await.map(Json).map_err(Api::internal)`. Always `200` on
a completed check (even a failed one); only a bad vendor name is an HTTP
error.

**Wire type** (`models/fluorite/settings.fl`):
```
/// The outcome of an on-demand connection check for a configurable vendor
/// (currently velos only) — reachability + token validity, checked live.
/// Never persisted; a fresh check every call.
struct VendorTestResult {
    ok: bool,
    /// The authenticated identity when `ok` (e.g. "admin", "worker:name").
    identity: Option<String>,
    error: Option<String>,
}
```
Regenerated into `clients/ts` and `clients/web/src/generated` (`ts-drift`
CI check must stay green).

### Frontend

**`VelosRow`** (`clients/web/src/pages/SettingsPage.tsx`) gains local state
`testResult: { ok: boolean; identity?: string; error?: string } | null` and
`testing: boolean`, plus a "Test connection" button rendered next to the
existing `draft.error` / "Not loaded yet" line. On click: `POST
/api/config/vendors/:name/test`, render the result inline — success as
`Connected as {identity}` (success styling, matching the `chip
!py-0 text-[10px] text-success` treatment used elsewhere), failure as the
error string in the same red text style `draft.error` already uses.

The button is **disabled while the page-level `dirty` flag is true** (passed
down as a new `VelosRow` prop), with a `title` tooltip explaining why — the
test always checks the *saved* row, so testing mid-edit would silently check
stale values. This also means a brand-new, never-saved row can't be tested
(it's `dirty` from the moment it's added), which is correct: there's nothing
saved yet to check.

**Auto-test after save**: `SettingsPage`'s `save()` already gets the fresh
`SettingsView` back from `update.mutateAsync`. On a successful save, fire
`POST /api/config/vendors/:name/test` for every vendor in the response whose
`config.kind === "Velos"` (in parallel, fire-and-forget per row — a failure
to reach one vendor doesn't block showing results for the others), and feed
each result into that row's `testResult` state via the same path the manual
button uses. This covers "I just fixed the token, does it work now" without
an extra click, while the manual button stays available for re-checking
later without touching the form (e.g. confirming after the token was rotated
on the velos side, independent of horsie).

**Plumbing**: `clients/web/src/api/client.ts` gains `config.testVendor(name:
string): Promise<VendorTestResult>` (`POST
/api/config/vendors/${name}/test`). `clients/web/src/hooks/useSettings.ts`
gains `useTestVendor()`, a `useMutation` wrapping it — no query invalidation
(nothing server-side changed), unlike `useTestMcpServer`.

## Error handling

- A check that completes (successfully reaching velos and getting a
  definitive accept/reject) always returns `VendorTestResult` with `ok`
  false or true — never an HTTP error. Only "this vendor doesn't exist to
  test" is an HTTP-level error, and the UI can't reach that state given the
  dirty-gate.
- The 10s timeout means a dead `server_url` resolves to a clear "velos
  request failed: ..." message within a bounded time instead of hanging the
  button indefinitely.
- Auto-test-after-save failures (e.g. the browser drops the follow-up
  request) are invisible to the save itself — save's success/failure is
  reported independently and first; the auto-test is a best-effort follow-up
  that only populates `testResult`, and the manual button remains available
  if it doesn't land.

## Testing summary

- `server/src/velos/client.rs`: `whoami()` against the existing mock-server
  test harness — 200 with `"admin"`, 200 with `{"worker": "w1"}`, 401, and a
  connection to a closed port (asserts the error is a `Request` variant, not
  a hang — exercises the timeout).
- `server/src/config/store.rs`: `test_vendor()` against a seeded DB —
  velos row with a token the mock accepts → `ok: true` +
  `identity: Some(...)`; a token the mock rejects with 401 → `ok: false` +
  `error: Some("token rejected...")`; unknown vendor name → `Err`.
- `server/src/http/mod.rs`: one HTTP-level test hitting `POST
  /api/config/vendors/:name/test`, mirroring the existing `/api/mcp/servers
  /acme/test` test's shape.
- Full gate (`fmt`/`clippy`/`test`/`deny`) + `clients/web` `bun run build` +
  `ts-drift` check green, same as prior settings-store changes.
- No new frontend automated tests (matches the existing pattern — this repo
  has no frontend test suite). Manual smoke test: run `horsie serve` against
  a real or mock velos, save a vendor with a good token (auto-test shows
  "Connected as ..."), edit to a bad token and save again (auto-test shows
  the rejection), click "Test connection" standalone to re-confirm.

## Alternatives considered

- **Add a dedicated velos health endpoint** (e.g. a purpose-built
  `/healthz/auth` distinct from `/auth/v1/me`): more explicit intent, but
  `/auth/v1/me` already does exactly this job and `velosctl login` already
  relies on it for the same purpose — a second endpoint would duplicate
  behavior in the velos repo for no functional gain. Rejected in favor of
  reusing it as-is.
- **Test the live vendor instance's `Arc<dyn ContainerApi>` instead of
  rebuilding a client from the row**: would reuse exactly the settings the
  running vendor has, but `ContainerApi` is deliberately scoped to container
  lifecycle ops (per its own doc comment), and `whoami` isn't one — adding it
  would force every `ContainerApi` test double (used for `VelosVendor`'s own
  unit tests) to grow a matching no-op impl. It would also make the check
  unusable for a vendor that's currently inactive (failed to bind), which is
  exactly a case where knowing "is at least the token fine" is useful.
  Rejected in favor of a fresh, ephemeral client built straight from the
  row — cheap (one HTTP client, immediately dropped) and independent of
  live-vendor state.
- **Test unsaved form fields (pre-save validation)**: lets you catch a typo
  before persisting, but needs the endpoint to accept ad-hoc
  server_url/token in the request body with the same "blank token = use
  stored" fallback `resolve_secret` already implements for saves, and departs
  from the MCP `test` precedent (which also only ever checks the saved row).
  Rejected for consistency and because the auto-test-after-save behavior
  covers the "did my edit fix it" case with one extra (automatic) round trip
  instead of new request-shape surface.
