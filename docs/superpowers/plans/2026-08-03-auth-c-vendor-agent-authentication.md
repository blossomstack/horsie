# Auth C: vendor agent authentication — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Require a credential on `/api/vendor/connect`, bind each connected vendor to the principal that presented it, and stop a stranger displacing a live vendor by name.

**Architecture:** The WS upgrade grows a bearer check accepting sub-project A/B's `access` and `agent` token kinds. `RuntimeVendorLink` carries the owning principal, and `RuntimeVendorRegistry::register` becomes fallible so a name owned by someone else is refused rather than silently overwritten. Long-lived `agent` tokens get CRUD in Settings for headless deploys; `horsie connect` reuses a normal login.

**Tech Stack:** Rust 2024, axum 0.7, tokio-tungstenite, sqlx 0.8 (SQLite), fluorite 0.6 with TS codegen, React 19, Playwright.

## Global Constraints

- Production code denies `clippy::unwrap_used`, `expect_used`, `panic`, `wildcard_enum_match_arm`.
- Store/service layers return `Result<T, String>`. Wire types are fluorite-generated; JSON is camelCase.
- Test SQLite pools are file-backed with `busy_timeout`, never `sqlite::memory:`.
- Run `cargo fmt --all` **before** clippy. **`make ts-types` before pushing** — the drift job diffs only *tracked* files under `clients/ts/src/generated`, so new generated files pass silently and fail on the next PR.
- Auth-disabled deployments must behave exactly as today: no credential required anywhere.

## The hole being closed

`vendor_connect` performs a raw WS upgrade with no credential check, and `RuntimeVendorRegistry::register` does a bare `insert` keyed by the announced name. Anyone who can reach the port can announce `local`, silently replace the live vendor link, and receive the tool calls and injected credentials meant for someone's laptop.

---

### Task 1: Agent tokens in the store and service

**Files:** `server/src/auth/store.rs`, `server/src/auth/service.rs`, `server/src/auth/mod.rs`

**Interfaces:**
- Produces on `AuthStore`: `list_tokens_of_kind(kind) -> Vec<TokenSummary>`, `revoke_token_by_id` (reuse `revoke_token`); `TokenSummary { id, label, created_at, last_used_at }`.
- Produces on `AuthService`: `mint_agent_token(label, principal) -> Result<(String, TokenSummary), String>`, `list_agent_tokens()`, `revoke_agent_token(id)`.

Agent tokens never expire (`expires_at` NULL): a headless deploy has nobody to re-approve a device code, and revocation is the control that matters.

- [ ] **Step 1: Write the failing tests** — in `store.rs` tests, that `list_tokens_of_kind(Agent)` returns only live agent tokens newest-first with their labels, and that a revoked one drops out. In `service.rs` tests, that `mint_agent_token` returns a secret starting `hsk_agt_`, that it verifies to the minting principal with kind `Agent`, that it appears in `list_agent_tokens`, and that `revoke_agent_token` makes it stop verifying.

- [ ] **Step 2: Run to verify they fail** — `cargo test -p horsie-server --lib auth::`

- [ ] **Step 3: Implement.** `list_tokens_of_kind` selects `id, label, created_at, last_used_at FROM auth_tokens WHERE kind = ? AND revoked_at IS NULL ORDER BY created_at DESC`. `mint_agent_token` generates a `TokenKind::Agent` token with `expires_at = None`, a fresh uuid id, and the label, then returns the one-time secret alongside the summary.

- [ ] **Step 4: Run to verify they pass**, then commit: `feat(auth): mintable agent tokens`

---

### Task 2: Principal-owned vendor registration

**Files:** `server/src/runtime_vendor/link.rs`, `server/src/runtime_vendor/registry.rs`

**Interfaces:**
- `RuntimeVendorLink::start` gains an `owner: Principal` parameter, exposed as `owner()`.
- `RuntimeVendorRegistry::register` returns `Result<(), RegisterError>` with `RegisterError::NameTaken { by: String }`.

The rule: the **same** principal reconnecting replaces its own entry (a dropped socket must recover), a **different** principal claiming a live name is refused. With auth disabled every principal is `Anonymous`, so the "same principal" branch applies and today's behaviour is preserved exactly.

- [ ] **Step 1: Write the failing tests** in `registry.rs`: registering a fresh name succeeds; the same principal re-registering the same name succeeds and swaps the link; a different principal registering that name fails with `NameTaken` and leaves the original in place; and two different principals may hold two different names.

- [ ] **Step 2: Run to verify they fail** — `cargo test -p horsie-server --lib runtime_vendor::`

- [ ] **Step 3: Implement.** `register` reads the existing entry under the write lock, compares `owner()`, and either inserts or returns `NameTaken`. Log the refusal at `warn` with both principals — it is the signal of an attempted takeover.

- [ ] **Step 4: Run to verify they pass**, then commit: `feat(auth): vendor names are owned by the principal that claimed them`

---

### Task 3: Bearer on the vendor WS upgrade

**Files:** `server/src/http/vendor_connect.rs`, `server/src/http/mod.rs` (tests)

Reject **before** the 101, not after: an upgrade that completes and then closes looks to the agent like a transport fault it should retry, whereas a 401 is a fact it can report.

- [ ] **Step 1: Write the failing tests** in `http::tests`, over a real bound listener as the existing `a_connected_agent_becomes_a_selectable_vendor` test does: with auth enabled, a dial carrying no credential never registers; one carrying a `web` token never registers (right principal, wrong kind — a browser cookie has no business driving a machine link); one carrying an agent token registers; and with auth disabled a bare dial still registers.

- [ ] **Step 2: Run to verify they fail.**

- [ ] **Step 3: Implement.** In `vendor_connect`, before deriving the accept key: if `state.auth.enabled()`, read the bearer from the `Authorization` header, `verify` it, require `kind` ∈ {`Access`, `Agent`}, and return `401` otherwise. Pass the resolved `Principal` (or `Anonymous` when disabled) into `RuntimeVendorLink::start`, and handle `register`'s `Err` by logging and dropping the link.

- [ ] **Step 4: Run to verify they pass**, then commit: `feat(auth): require a credential on /api/vendor/connect`

---

### Task 4: Agent-token HTTP surface and Settings page

**Files:** `models/fluorite/auth.fl`, `server/src/http/auth.rs`, `server/src/http/mod.rs`, `clients/web/src/api/client.ts`, `clients/web/src/pages/settings/AccountSettings.tsx`

**Interfaces:** `GET/POST /api/auth/tokens`, `DELETE /api/auth/tokens/:id`, all cookie-or-bearer authenticated. Wire types `AgentTokenView { id, label, createdAt, lastUsedAt }`, `AgentTokenCreateInput { label }`, `AgentTokenCreated { token, view }`.

The secret is returned **once**, on creation, and never again — the store keeps only its hash, so there is nothing to show later even if we wanted to.

- [ ] **Step 1: Write the failing HTTP tests**: create → list shows it → the secret works as a bearer on `/api/sessions` → delete → it stops working. Unauthenticated create is a 401.

- [ ] **Step 2: Run to verify they fail.**

- [ ] **Step 3: Implement** the schema, handlers, routes, and a "Machine tokens" section on the existing Account settings page: create with a label, show the secret once in a copyable block with a warning that it will not be shown again, list id/label/created/last-used, revoke.

- [ ] **Step 4: `make ts-types`, typecheck, build**, then commit: `feat(auth): agent tokens for headless vendor agents`

---

### Task 5: Clients present credentials

**Files:** `runtime-vendor/src/vendor.rs`, `cli/src/connect.rs`, `cli/src/main.rs`, `velos-runtime/src/main.rs`

**Interfaces:** `RuntimeVendor::run(server_url, token: Option<&str>, cancel)`.

`connect_async` takes a URL *or* a request, so the token rides on an `into_client_request()` the dial already builds for validation.

- [ ] **Step 1: Write the failing test** in `runtime-vendor`: `run` with a token still fails fast on an undialable URL (proving the request-building path is exercised), and the header is attached — assert by building the client request through the same helper and reading `Authorization` back.

- [ ] **Step 2: Run to verify it fails.**

- [ ] **Step 3: Implement.** In `vendor.rs`, thread `Option<String>` into `connect`, build the request with `into_client_request()`, insert `Authorization: Bearer …`, and dial that. In `cli/src/connect.rs`, resolve the credential with `crate::auth::resolve_token(server)` and pass it through; a server with auth off yields `None` and nothing changes. In `velos-runtime`, add `--token` preferring `HORSIE_TOKEN` from the environment, matching the existing `HORSIE_VELOS_TOKEN` guidance so the secret stays out of process listings.

- [ ] **Step 4: Run to verify it passes**, then commit: `feat(cli): present credentials when dialing as a vendor agent`

---

### Task 6: End-to-end verification, docs, PR

**Files:** `docs/guide/runtime-vendors.md`, `docs/guide/getting-started.md`, `clients/web/e2e/p-agent-tokens.spec.ts`

- [ ] **Step 1: e2e spec** on an auth-enabled second server: mint an agent token through the UI, assert the secret is shown once, and assert a WS dial carrying it registers a vendor while one without is refused.
- [ ] **Step 2: Run the full e2e suite** — the shared server runs auth-disabled, so `horsie connect` in `global-setup` must still work untouched. That is the regression this step exists to catch.
- [ ] **Step 3: Docs.** `runtime-vendors.md` gains a section on machine tokens and when to use one instead of a login; `getting-started.md`'s connect step notes that a login is now required when the server has auth on.
- [ ] **Step 4: Manual verification.** Against a live auth-enabled server: `horsie connect` without credentials is refused with a clear message; after `horsie auth login` it registers; a minted agent token registers; and a second machine claiming the same name under a different token is refused.
- [ ] **Step 5: `cargo fmt --all && make check`, `make ts-types`, full e2e.**
- [ ] **Step 6: Open the PR** — closes #109, completes #106. State the hole closed, the same-principal-replaces rule and why, that `web`/`refresh` kinds are rejected on machine links, and the auth-disabled path being unchanged.
- [ ] **Step 7: `gh pr checks --watch`.**
