# MCP server support (remote MCP servers, server-side)

- **Date:** 2026-07-12
- **Status:** Design — awaiting review
- **Branch (planned):** `mcp-servers`

## Summary

Let the session agent call **remote MCP servers** so it can use third-party
toolsets (starting with GitHub: create PRs, search issues, …) without horsie
implementing each tool natively. The MCP call happens **server-side**: horsie
itself is the MCP client. The agent loop and tool-dispatch already run in the
horsie **server** process; MCP tools plug in next to the sandbox-backed tools as
another `Toolbox`, so MCP calls and their credentials **never enter the
runtime/sandbox**.

The first server is GitHub, and it **reuses the existing GitHub App connection**
(#103) for auth rather than a separate OAuth dance.

## Goals

- The agent can invoke tools on a remote MCP server during a session turn.
- MCP tool execution runs in the server process; the sandbox never sees MCP
  credentials or network egress for MCP.
- A single, uniform `mcp_servers` store (like the generic `vendors` table), with
  a tagged auth model that supports servers with "special automated setup"
  (GitHub) alongside generic remote servers in the same table.
- GitHub MCP is set up **from within the existing GitHub settings section**,
  reusing the GitHub App connection to authenticate.
- Full OAuth 2.1 (discovery + dynamic client registration + PKCE + refresh) for
  arbitrary remote servers, and as the GitHub fallback if the reused App token
  is refused.
- Runtime-editable via the Settings UI (mirrors providers/models/vendors), and
  selectable per session (like model/vendor in the new-session modal).

## Non-goals (v1)

- Local/stdio MCP servers. **Remote, Streamable-HTTP only.**
- MCP prompts and resources. **Tools only.**
- Running MCP servers *inside* the sandbox.
- A general secrets vault. Secrets reuse the existing redacting `Secret` type.

## Background — why this is a small change

Confirmed by reading the code:

- **The agent loop runs server-side.** `SessionActor` (server crate) spawns the
  `AgentActor` (workflow crate) in the server process
  (`server/src/sessions/session_actor.rs:288`). The `AgentActor` owns the
  `provider` (LLM) and the `toolbox` and runs the whole call → tool → call loop
  in-process.
- **Tool *dispatch* is already server-side; only *execution* of sandbox tools is
  delegated.** `DefaultToolboxFactory::for_agent` builds the toolbox with a
  `runtime_client` (`workflow/src/context.rs:112`). Runtime tools (`bash`,
  `read_file`, …) forward to the sandbox over that client; a tool that does not
  use `runtime_client` executes entirely in the server.
- **`Toolbox` is a two-method trait that composes.** `Toolbox`
  (`horsie_agentcore`) is `specs() -> Vec<ToolSpec>` + `async execute(name,
  input) -> Result<Value, ToolCallError>`. Existing impls already layer:
  `AgentToolbox` wraps a `base`; `FilteredToolbox` applies the `allowed_tools`
  allowlist; `add_runtime_tools` registers the sandbox tools
  (`workflow/src/context.rs`).
- **The GitHub App connection already mints the tokens MCP needs.**
  `GithubService` (`server/src/github/service.rs`) exposes
  `mint_token_for(repo_urls)` (a short-lived **installation** token scoped to the
  session's repos, `service.rs:155`) and holds the connected account's
  **user OAuth token** (`credentials.access_token`, `service.rs:82`). GitHub's
  remote MCP server authenticates with a GitHub token as `Authorization:
  Bearer`, so GitHub MCP can reuse this connection.

## Verification — remote GitHub MCP server auth

The remote GitHub MCP server (`https://api.githubcopilot.com/mcp/`) accepts:

1. **GitHub's own OAuth 2.1 + PKCE** — GitHub is the authorization server using
   *GitHub's* app credentials (not a third-party App).
2. **A GitHub token via `Authorization: Bearer <token>`** — documented as a PAT;
   the server gets the scopes the token grants.

**Undocumented:** whether the remote endpoint accepts a *third-party GitHub
App's* installation (`ghs_`) or user (`ghu_`) token. The self-hostable
`github-mcp-server` binary *explicitly* accepts an arbitrary
`GITHUB_PERSONAL_ACCESS_TOKEN`.

**Consequence:** reusing horsie's App token is *probably* fine but must be
**smoke-tested**, not assumed. v1 tries the reused user token and falls back to
GitHub's own OAuth 2.1 if refused (decision below).

## Decisions (from brainstorming)

- **Architecture:** horsie server is the MCP client (Option A). MCP tools plug in
  as a `Toolbox`; the sandbox is never involved.
- **Storage:** one `mcp_servers` table; auth is a **tagged union** so a
  special-setup server (GitHub) and generic servers share the table.
- **Generic-server auth:** full OAuth 2.1 (discovery + DCR + PKCE + refresh).
- **GitHub MCP auth (v1):** `GithubApp` variant — mint a **user token** from the
  existing GitHub connection, use it as the remote server's Bearer, **gated by a
  live `initialize` + `tools/list` smoke test**; if refused, fall back to
  GitHub's own OAuth 2.1 (the generic `OAuth` variant pointed at GitHub). Set up
  from within the existing GitHub settings section.
- **Transport:** Streamable HTTP only (v1).
- **MCP client:** hand-rolled minimal client behind a transport trait (consistent
  with the velos `ContainerApi` seam), mock-testable. `rmcp` crate considered as
  an alternative (see Alternatives).
- **Scope:** per-session opt-in — a session selects which enabled servers it
  uses (like model/vendor), default none.
- **Tool-list caching:** connect + `tools/list` per session at `ensure_agent`
  for v1; cache per server later.

## Architecture

```
AgentActor (server process)
  toolbox = AgentToolbox { conclude / skill / inspect }
    └─ base = FilteredToolbox(allowed_tools)?          ← allowlist still applies to MCP
          └─ CompositeToolbox
                ├─ runtime tools (bash, read_file, …) ── runtime_client ─▶ SANDBOX
                ├─ McpToolbox("github")  ── McpClient(Bearer) ─▶ REMOTE MCP SERVER
                └─ McpToolbox("<other>") ── McpClient(...)     ─▶ REMOTE MCP SERVER
```

- `McpToolbox` implements `Toolbox`. `specs()` returns each MCP tool namespaced
  as `mcp__<server>__<tool>`; `execute()` strips the prefix and issues
  `tools/call`, mapping `McpError` → `ToolCallError`.
- A new `CompositeToolbox` fans `specs()`/`execute()` across `[runtime, mcp…]`,
  routing by a name→toolbox map built from `specs()`. It becomes the pre-filter
  `base`, so MCP tools are subject to `allowed_tools` and visible to the agent
  exactly like runtime tools.

### Crate / module layout

- **`mcp-client`** (new crate, mirrors `runtime-client`): `McpClient` over a
  `McpTransport` trait. Streamable HTTP transport (JSON-RPC over POST; parse a
  JSON or SSE response; carry `Mcp-Session-Id`). Surface: `initialize()`,
  `list_tools() -> Vec<McpToolDef>`, `call_tool(name, args) -> Value`. Auth via a
  pluggable `Authorization` header provider so the token can be refreshed.
  `McpToolDef = { name, description, input_schema: Value }`. Mock transport for
  tests.
- **`McpToolbox`** — in the `workflow` crate, next to `AgentToolbox`; depends on
  `mcp-client` + `horsie_agentcore::Toolbox`. Holds `Arc<McpClient>`, the server
  name (namespace), and the pre-fetched tool list.
- **`McpService`** — in the `server` crate, next to `GithubService`. Owns the
  `mcp_servers` store, builds `McpClient`s with the right auth, runs the OAuth
  flow and the smoke test, and (for `GithubApp`) delegates token minting to
  `GithubService`. Exposes `async toolboxes_for(servers, repo_urls) ->
  Result<Vec<Arc<dyn Toolbox>>, String>` for `ensure_agent`.

### Wiring into `ensure_agent`

- Extend `ToolboxFactory::for_agent(..)` with an `mcp: Vec<Arc<dyn Toolbox>>`
  param; `DefaultToolboxFactory` composes `runtime + mcp` into the
  `CompositeToolbox` base (before `FilteredToolbox`), then `AgentToolbox` wraps
  as today.
- `ServerDeps` gains an `mcp: Arc<McpService>`.
- In `ensure_agent` (`session_actor.rs`), before building the toolbox: read the
  session's `agent.mcp_servers` names, call `mcp.toolboxes_for(names,
  session_repo_urls)`, pass the result into `for_agent`. Connect + `tools/list`
  happen here (alongside the existing `scan_workspace` / `run_session_start`), so
  MCP tools reflect the live server on each turn's spawn.

## Storage & config

New migration `server/migrations/0003_mcp.sql`, table `mcp_servers`:

| column | notes |
|---|---|
| `name` TEXT PK | stable id, namespace prefix |
| `url` TEXT | server endpoint |
| `enabled` INTEGER | connected/usable |
| `auth_kind` TEXT | `none` \| `bearer` \| `oauth` \| `github_app` |
| `bearer_token` TEXT (Secret) | `bearer` only |
| `oauth_client_id` TEXT | `oauth`; from DCR or manual |
| `oauth_client_secret` TEXT (Secret) | `oauth` |
| `oauth_access_token` TEXT (Secret) | `oauth` |
| `oauth_refresh_token` TEXT (Secret) | `oauth` |
| `oauth_expires_at` INTEGER | `oauth` |
| `oauth_meta` TEXT (JSON) | discovered AS metadata / endpoints |
| `last_error` TEXT | last connect/smoke-test error, for the UI |
| `created_at` / `updated_at` | |

For `github_app`: **no tokens stored on the row** — the token is minted from
`GithubService` at use time (reuse of the existing connection). Secrets follow
the existing `Secret` redaction; write-only inputs (omit=keep, ""=clear,
value=set) as in the settings/vendor work.

### fluorite `models/fluorite/mcp.fl`

- `McpAuthView` union (tag `kind`): `None`, `Bearer { has_token }`,
  `OAuth { connected, client_id: Option, has_client_secret }`, `GithubApp`.
- `McpAuthInput` union: `None`, `Bearer { token: Option }`,
  `OAuth { client_id?, client_secret?, manual_endpoints? }`, `GithubApp`.
- `McpServerView { name, url, enabled, auth: McpAuthView, tool_count: Option<u32>, last_error: Option<String> }`.
- `McpServerInput { name, url, auth: McpAuthInput }`.
- `McpConnectResult { ok, tool_count: Option<u32>, error: Option<String> }` — the
  smoke-test / connect result.

TS codegen into `clients/ts` + `clients/web/src/generated/mcp/` (regen in both
`generate-types` scripts), matching the existing convention.

## Auth flows

### GitHub MCP (`github_app`) — the reuse path

Setup, from the **existing GitHub settings section** (once the App is connected):

1. User flips **"Enable GitHub tools (MCP)"**.
2. horsie upserts the `github` row (`url =
   https://api.githubcopilot.com/mcp/`, `auth_kind = github_app`) and runs a
   **live smoke test**: `McpService` gets a user token from `GithubService`,
   builds an `McpClient` with `Authorization: Bearer <token>`, and issues
   `initialize` + `tools/list`.
3. On success → `enabled = true`, `tool_count` recorded, done — no separate
   OAuth.
4. On failure → surface the error and offer the **fallback**: GitHub's own OAuth
   2.1 (switch this row to the generic `oauth` variant pointed at the same URL).

At session use, `McpService` mints/refreshes the user token per turn and injects
it as the Bearer. (Requires adding a `user_token()` accessor to `GithubService`
that returns `credentials.access_token`, refreshing via the stored
`refresh_token` when expired — see Risks.)

### Generic remote servers (`oauth`) — full OAuth 2.1

For arbitrary servers and as the GitHub fallback:

1. **Discovery:** hit the server; on `401 WWW-Authenticate` read protected-
   resource metadata (RFC 9728) → authorization-server metadata (RFC 8414) →
   endpoints. Store in `oauth_meta`.
2. **Dynamic Client Registration (RFC 7591):** register horsie to get a
   `client_id` (+ secret) automatically. Manual fields are the fallback when the
   AS has no DCR endpoint.
3. **Authorization code + PKCE:** build the authorize URL (`state`, PKCE), open
   in the browser; callback to `GET /api/mcp/servers/:name/oauth/callback`;
   exchange `code` + verifier for access/refresh tokens; store (`Secret`).
   Transient per-request state (verifier, `state`, server name) held server-side,
   keyed by `state`.
4. **Refresh:** before connecting and on a `401` during a `tools/call`, refresh
   using the stored refresh token.

`bearer` and `none` are trivial variants (paste a token / public server).

## HTTP API (`server/src/http/mcp.rs`)

- `GET  /api/mcp/servers` → `Vec<McpServerView>`
- `PUT  /api/mcp/servers/:name` → upsert (`McpServerInput`) → `McpServerView`
- `DELETE /api/mcp/servers/:name`
- `POST /api/mcp/servers/:name/connect` → for `oauth`: returns authorize URL; for
  `github_app`/`bearer`: runs the smoke test → `McpConnectResult`
- `GET  /api/mcp/servers/:name/oauth/callback` → generic OAuth code exchange
- `POST /api/mcp/servers/:name/test` → smoke test (`initialize` + `tools/list`) →
  `McpConnectResult`

## Web UI (`clients/web`)

- **Settings page:** a new "MCP servers" section — CRUD of generic servers
  (name, url, auth kind, connect/authorize, tool count, last error), mirroring
  the providers/models/vendors sections.
- **GitHub settings section:** an **"Enable GitHub tools (MCP)"** toggle that
  creates/enables the `github` row (`github_app`) and shows the smoke-test result
  (and the OAuth-fallback affordance on failure).
- **New-session modal:** a multi-select of enabled MCP servers →
  `createSessionRequest.mcp_servers`. `useMcp` hook + `api/client.ts` methods,
  a sidebar/Settings nav entry.

## Session wiring

- `session_api` `CreateSessionRequest` gains `mcp_servers: Vec<String>`.
- `SessionSpec.agent` (`AgentSettings`) gains `mcp_servers: Vec<String>`.
- `ensure_agent` uses it as above.

## GitHub PR flow (end to end)

1. Agent edits files in the workspace (**runtime/sandbox**).
2. Agent runs `git push` of a branch (**runtime**) — authenticated by the
   short-lived **installation** token already wired by the provision pipeline
   (#101/#103), independent of MCP auth.
3. Agent calls `mcp__github__create_pull_request` → `McpToolbox` (**server**) →
   `tools/call` to the GitHub MCP server with the reused GitHub token → PR
   created. The token never entered the sandbox.

## Security

- MCP credentials live only in the server process (DB via `Secret`, or minted
  on demand for `github_app`); the sandbox never receives them and has no MCP
  egress.
- Views never return secrets (`has_*` flags only), matching the existing config
  store.
- `github_app` least-privilege: the reused token carries only the App's granted
  permissions; note this is *account/user*-scoped (user token) rather than the
  per-repo installation scoping used for git — see Risks.
- OAuth `state` + PKCE guard the callback; `oauth_meta`/tokens are per-server.

## Phasing

- **Phase 1 (core + reuse):** `mcp-client` crate, `McpToolbox` +
  `CompositeToolbox`, `for_agent`/`ensure_agent` wiring, `mcp_servers` table +
  `mcp.fl` + CRUD API + Settings section, per-session selection in the modal,
  `none`/`bearer`/`github_app` auth, the GitHub "Enable MCP" toggle + smoke test,
  `GithubService::user_token()` (+ refresh). End-to-end GitHub PR works when the
  reused token is accepted.
- **Phase 2 (OAuth 2.1):** discovery + DCR + PKCE + callback + refresh — for
  generic servers and as the GitHub fallback. Heaviest piece; likely its own
  plan.

## Alternatives considered

- **Anthropic MCP connector** (`mcp_servers` + `mcp_toolset` in the Messages
  API): almost no dispatch code, but Anthropic-only (horsie is provider-agnostic
  via `LlmProvider`), requires the server be publicly reachable from Anthropic,
  and needs `async-llm` support. Rejected — server-side client is
  provider-agnostic and keeps credentials/egress local.
- **`rmcp` (official Rust MCP SDK)** instead of a hand-rolled client: less code
  to own, but a dependency and less control; the codebase favors small
  hand-rolled clients behind a seam (velos). Open for the plan to reconsider.
- **Self-hosted `github-mcp-server`** fed the minted installation token:
  guaranteed reuse + per-repo scoping, but adds a running component. Kept as the
  Phase-2+ fallback if the reused-token and GitHub-OAuth paths both disappoint.
- **Native GitHub tools via the existing App** (no MCP): zero new dependency, but
  reimplements what MCP provides and isn't the general capability asked for.

## Risks / open questions

- **Remote-server token acceptance (primary risk):** the remote GitHub MCP
  endpoint may reject a third-party App token. Mitigated by the smoke-test gate +
  OAuth fallback; validate early in Phase 1.
- **User-token refresh:** the existing connection stores a `refresh_token` but no
  refresh routine is visible. Phase 1 must add refresh (GitHub user-to-server
  tokens expire in ~8h when the App has token expiration enabled; may be
  non-expiring otherwise). Confirm the App's setting.
- **Token scope for MCP:** v1 uses the *user* token (account-scoped), not the
  per-repo installation token, because the user token is the documented Bearer
  shape. If least-privilege per-repo scoping is required, that points to the
  self-hosted fallback (installation token).
- **Streamable HTTP SSE responses:** the client must handle both a plain JSON
  response and an SSE stream for a single `tools/call`. Keep the transport seam
  narrow and test both.
- **Per-turn connect latency:** connecting + `tools/list` at every `ensure_agent`
  adds first-turn latency; acceptable for v1, cache per server later.
