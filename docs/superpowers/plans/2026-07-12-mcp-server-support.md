# MCP server support — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the session agent call remote MCP servers server-side (horsie is the MCP client), starting with GitHub via the existing GitHub App connection.

**Architecture:** An `McpToolbox` implements the existing `horsie_agentcore::Toolbox` trait and composes into the agent's toolbox next to the sandbox-backed runtime tools, so MCP calls run in the server process and never touch the runtime/sandbox. A small `mcp-client` crate speaks MCP over Streamable HTTP behind a transport seam.

**Tech Stack:** Rust (async-trait, reqwest, serde_json, thiserror), fluorite wire types, sqlx (SQLite) config store, axum HTTP, React/Bun web UI.

Spec: `docs/superpowers/specs/2026-07-12-mcp-server-support-design.md`.

## Global Constraints

- Rust toolchain **1.96.0**; CI runs `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --all-features -- -D warnings`, `cargo test --locked --workspace --all-features`, `cargo deny check`, and a TS drift check (`clients/ts` generated must match).
- Production code denies `unwrap_used`, `expect_used`, `panic`, `wildcard_enum_match_arm`; test modules opt out with `#![cfg_attr(test, allow(...))]` or a `#[cfg(test)] mod tests` `#[allow(...)]` block, per the repo convention.
- `--locked`: commit an updated `Cargo.lock`. Use only deps already in the workspace tree (reqwest, serde, serde_json, async-trait, thiserror) to avoid new supply-chain entries.
- Protocol/wire types use fluorite under `models/fluorite/`; never for storage rows. Secrets use `horsie_agentcore::Secret`. Config views never return secrets (`has_*` flags only).
- Work on branch `mcp-servers`; land as a sequence of green PRs (below). Commit messages: no AI attribution.

## PR sequence

- **PR 1 — MCP client core (this plan, detailed below).** New `mcp-client` crate + `McpToolbox` + `CompositeToolbox`, unit-tested. Purely additive; no existing behavior changes; Rust-only (no fluorite/UI), so ts-drift is unaffected.
- **PR 2 — Session + config wiring.** `mcp_servers` table + `mcp.fl` + CRUD API + regenerated TS types; `McpService`; extend `ToolboxFactory::for_agent` with MCP toolboxes; `ServerDeps.mcp`; `AgentSettings.mcp_servers`; `ensure_agent` builds MCP toolboxes; new-session modal + Settings section. `none`/`bearer`/`github_app` auth; GitHub "Enable MCP" toggle + smoke test; `GithubService::user_token()` (+ refresh).
- **PR 3 — OAuth 2.1.** Discovery (RFC 9728/8414) + DCR (RFC 7591) + PKCE + callback + refresh, for generic servers and as the GitHub fallback.

---

## PR 1 tasks

### Task 1: `mcp-client` crate — errors + types + transport trait

**Files:**
- Create: `mcp-client/Cargo.toml`, `mcp-client/src/lib.rs`, `mcp-client/src/error.rs`, `mcp-client/src/types.rs`, `mcp-client/src/transport.rs`
- Modify: `Cargo.toml` (workspace `members`)

**Interfaces produced:**
- `McpError` (enum: `Transport(String)`, `Protocol(String)`, `Rpc { code: i64, message: String }`)
- `McpToolDef { name: String, description: String, input_schema: serde_json::Value }`
- `McpCallOutcome { is_error: bool, text: String }`
- `#[async_trait] trait McpTransport: Send + Sync { async fn request(&self, method: &str, params: Value) -> Result<Value, McpError>; async fn notify(&self, method: &str, params: Value) -> Result<(), McpError>; }`

- [x] Add `mcp-client` to workspace `members`. Crate `[package] name = "horsie-mcp-client"`, `[lints] workspace = true`, deps `serde`, `serde_json`, `async-trait`, `thiserror`, `reqwest` (workspace, `json`), `tokio` (workspace) — all workspace-versioned.
- [x] Define `McpError`, `McpToolDef`, `McpCallOutcome`, and the `McpTransport` trait. `request` returns the JSON-RPC `result` value (Err on rpc error / transport failure); `notify` sends a notification (no response).
- [x] `cargo build -p horsie-mcp-client` compiles.

### Task 2: `McpClient` over the transport

**Files:** Create `mcp-client/src/client.rs`; export from `lib.rs`.

**Interfaces produced:**
- `McpClient` with `new(Arc<dyn McpTransport>) -> Self`, `async initialize() -> Result<(), McpError>` (sends `initialize` then the `notifications/initialized` notification), `async list_tools() -> Result<Vec<McpToolDef>, McpError>` (parses `result.tools[]` → name/description/`inputSchema`), `async call_tool(name: &str, arguments: Value) -> Result<McpCallOutcome, McpError>` (parses `result.content[]` text blocks joined into `text`, `result.isError` → `is_error`).

- [x] Write a `MockTransport` (test-only) returning canned `result`s per method; unit-test `initialize`/`list_tools`/`call_tool` parsing (incl. `isError: true`).
- [x] Implement `McpClient`; `cargo test -p horsie-mcp-client` passes.

### Task 3: `HttpTransport` (Streamable HTTP)

**Files:** Add `HttpTransport` to `mcp-client/src/transport.rs`.

**Interfaces produced:**
- `HttpTransport::new(endpoint: String, bearer: Option<String>) -> Self` implementing `McpTransport`. POSTs JSON-RPC; `Accept: application/json, text/event-stream`; injects `Authorization: Bearer` when set; captures/echoes `Mcp-Session-Id`; parses either a JSON body or an SSE body (scan `data:` lines, take the first JSON-RPC object carrying `result`/`error`). Internal `AtomicU64` request ids.

- [x] Implement `HttpTransport` with an internal id counter and a `Mutex<Option<String>>` session id. Unit-test the SSE-body parser as a pure helper (`parse_sse_response(&str) -> Result<Value, McpError>`); the live HTTP path is covered by PR 2's smoke test.
- [x] `cargo test -p horsie-mcp-client` passes; `cargo clippy -p horsie-mcp-client --all-targets -- -D warnings` clean.

### Task 4: `McpToolbox` + `CompositeToolbox` in the workflow crate

**Files:**
- Create: `workflow/src/mcp_toolbox.rs`
- Modify: `workflow/src/lib.rs` (module + re-exports), `workflow/Cargo.toml` (add `horsie-mcp-client` path dep)

**Interfaces produced (from `horsie_workflow`):**
- `CompositeToolbox::new(Vec<Arc<dyn Toolbox>>) -> Self` — `specs()` concatenates; `execute()` routes to the first box whose `specs()` contains `name`, else `ToolCallError::InvalidInput`.
- `McpToolbox` — `new(server: String, client: Arc<McpClient>, tools: Vec<McpToolDef>)` and `async connect(server: String, client: Arc<McpClient>) -> Result<Self, McpError>` (calls `initialize` + `list_tools`). `specs()` namespaces each tool `mcp__<server>__<tool>`; `execute()` strips the prefix, calls `call_tool`, maps `is_error`→`ExecutionFailed`, success→`Value::String(text)`, transport error→`ExecutionFailed`.

- [x] Write failing unit tests: a `CompositeToolbox` over two mock toolboxes routes by name and reports the union of specs; an `McpToolbox` built on a `MockTransport` lists namespaced specs and executes a tool (success + `is_error`).
- [x] Implement both; export from `lib.rs`.
- [x] `cargo test -p horsie-workflow` passes.

### Task 5: Gate + commit + PR

- [x] `cargo fmt --all` then `cargo fmt --all -- --check`.
- [x] `cargo clippy --locked --all-targets --all-features -- -D warnings`.
- [x] `cargo test --locked --workspace --all-features`.
- [x] `cargo deny check advisories bans licenses sources` (if `cargo-deny` installed; else note it's unverified locally).
- [x] Commit (`Cargo.lock` included); push `mcp-servers`; open PR describing PR 1 scope + the PR 2/PR 3 follow-ups.

## PR 2 tasks (session + config wiring — user-visible)

> Auth kinds in PR 2: `none` / `bearer` / `github_app`. **OAuth is deferred to PR 3** (its own migration + fluorite variant + flow), so PR 2 has no dead columns/stubs. Follows the **GitHub-service** pattern (dedicated `mcp/` module + `http/mcp.rs` + `mcp.fl`), not the monolithic config store.

### Task 6: fluorite `models/fluorite/mcp.fl` + TS codegen

- [x] `mcp.fl`: `enum McpAuthKind { None, Bearer, GithubApp }` (informational); tagged unions `#[type_tag="kind"] McpAuthView { None(McpNoAuth), Bearer(McpBearerView), GithubApp(McpGithubAppAuth) }` and `McpAuthInput { None(McpNoAuth), Bearer(McpBearerInput), GithubApp(McpGithubAppAuth) }` — newtype variants wrapping structs (empty structs allowed, mirroring `NetworkPolicy::Block(BlockNetwork)`). `McpNoAuth {}`, `McpGithubAppAuth {}`, `McpBearerView { has_token: bool }`, `McpBearerInput { token: Option<String> }` (omit=keep, ""=clear). `McpServerView { name, url, enabled, auth: McpAuthView, tool_count: Option<u32>, last_error: Option<String> }`; `McpServerInput { name, url, auth: McpAuthInput }`; `McpConnectResult { ok: bool, tool_count: Option<u32>, error: Option<String> }`.
- [x] Append `../../models/fluorite/mcp.fl` to `clients/web/package.json` `generate-types` only (mirrors `github.fl`; keeps `clients/ts` drift untouched). Regenerate `clients/web/src/generated`.

### Task 7: migration `server/migrations/0003_mcp.sql`

- [x] `CREATE TABLE mcp_servers (name TEXT PK, url TEXT NOT NULL, enabled INTEGER NOT NULL DEFAULT 0, auth_kind TEXT NOT NULL, bearer_token TEXT, tool_count INTEGER, last_error TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL)`. `github_app` stores no token (minted from `GithubService` at use).

### Task 8: `server/src/mcp/` — store + service

- [x] `store.rs`: `McpStore { pool }`, `McpServerRow`, `list()/get(name)/upsert(McpServerInput)/delete(name)/set_status(name, enabled, tool_count, last_error)`. Bearer secret via the `resolve_secret(input, existing)` idiom (omit=keep, ""=clear). Row→`McpServerView` projection (redacts token → `has_token`). Unit-test CRUD + secret resolution against an in-memory sqlite pool with the migration applied.
- [x] `service.rs`: `McpService { store, github: Arc<GithubService> }`. `list()/upsert()/delete()`; `bearer_for(row) -> Option<String>` (`none`→None, `bearer`→stored token, `github_app`→`github.user_token()?`); `test(name) -> McpConnectResult` (build `HttpTransport`+`McpClient`, `McpToolbox::connect`, persist enabled/tool_count/last_error); `toolboxes_for(names: &[String]) -> Result<Vec<Arc<dyn Toolbox>>, String>` for `ensure_agent` (connect per enabled server). `mod.rs` re-exports.

### Task 9: `GithubService::user_token()` + refresh

- [x] `GithubApi::refresh_token(client_id, client_secret, refresh_token) -> ExchangedToken` (POST `/login/oauth/access_token`, `grant_type=refresh_token`), mirroring `exchange_code`.
- [x] `GithubService::user_token() -> Result<Option<String>, String>`: load credentials; if `expires_at` is within a skew window and a `refresh_token` exists, refresh via the App's client_id/secret and persist; return `access_token`. Pure `needs_refresh(expires_at)` helper unit-tested.

### Task 10: HTTP API `server/src/http/mcp.rs`

- [x] Handlers: `GET /api/mcp/servers` → `Vec<McpServerView>`; `PUT /api/mcp/servers/:name` (`McpServerInput`) → `McpServerView`; `DELETE /api/mcp/servers/:name`; `POST /api/mcp/servers/:name/test` → `McpConnectResult`. Register routes in `http/mod.rs`; add `mcp: Arc<McpService>` to `AppState`. Errors: `Api::unprocessable`/`Api::internal`.

### Task 11: toolbox factory + session wiring

- [x] `ToolboxFactory::for_agent` gains `mcp: Vec<Arc<dyn Toolbox>>`; `DefaultToolboxFactory` composes `CompositeToolbox([runtime, ...mcp])` as the pre-`FilteredToolbox` base (allowlist still gates MCP tools). Update **all** callers (grep `for_agent`) to pass `vec![]` unless wiring MCP. Unit-test the composed+filtered shape.
- [x] `AgentSettings.mcp_servers: Vec<String>` (storage `sessions/spec.rs`) + `session.fl AgentSettings.mcp_servers: Option<Vec<String>>` + `session_api.fl CreateSessionRequest.mcp_servers: Option<Vec<String>>`; `settings_from_wire` maps it (default `vec![]`). `ServerDeps.mcp: Option<Arc<McpService>>`.
- [x] `ensure_agent`: if `settings.mcp_servers` non-empty, `deps.mcp` must be `Some`; call `toolboxes_for(&settings.mcp_servers)` and pass into `for_agent`; empty → `vec![]`.
- [x] `cli/src/serve.rs`: construct `McpService` (share `Arc<GithubService>`), put in `AppState.mcp` + `ServerDeps.mcp`. Update test `ServerDeps`/`AppState` builders (`None`/wired) + e2e.

### Task 12: web UI

- [x] `api/client.ts` `mcp` methods + `useMcp.ts` hook (list/upsert/delete/test, React Query). Settings page "MCP servers" `Section` (name, url, auth kind none/bearer, bearer token write-only, Test button → tool_count/last_error), mirroring `VelosRow`. GitHub section "Enable GitHub tools (MCP)" toggle → upsert `github` row (`github_app`) + Test. New-session modal multi-select of `enabled` servers → `CreateSessionRequest.mcp_servers`.

### Task 13: gate + PR

- [x] `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --all-features -- -D warnings`; `cargo test --locked --workspace --all-features`; `cargo deny check` (if installed). `clients/web`: `bun run generate-types` + typecheck + build. Commit (`Cargo.lock` + generated TS); push; open PR.

## Self-review

- **Spec coverage (PR 1 subset):** `Toolbox` composition seam (Task 4), server-side MCP client (Tasks 1-3), Streamable HTTP transport (Task 3), tool namespacing + error mapping (Task 4). Config/DB/UI/OAuth/GitHub-reuse are explicitly PR 2/PR 3.
- **Placeholders:** none; each task lists exact files + interfaces + tests.
- **Type consistency:** `McpToolDef`/`McpCallOutcome`/`McpTransport` defined in Task 1 are consumed unchanged in Tasks 2-4; `Toolbox`/`ToolSpec`/`ToolCallError` match `agentcore/src/{tool,error}.rs`.
