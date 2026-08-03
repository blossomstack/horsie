# Agent Presets — Server CRUD, Invoke, CLI, and Web UI

Date: 2026-08-02
Status: Approved design, pre-implementation

## Summary

Agents are a new first-class resource in horsie: named, DB-backed presets that
capture everything the new-session draft captures today (runtime vendor, model,
repos, skill/plugin bundles, MCP servers, memory spaces, thinking effort).
Invoking an agent with a message creates a session server-side and returns the
session id immediately — the turn runs in the background.

Three surfaces ship together:

- **Server**: agents CRUD API + `POST /api/agents/:name/invoke`.
- **CLI**: `horsie agent list|get|invoke`, plus `horsie session list|status` so
  an invoked session's execution can be checked.
- **Web UI**: an Agents section in the sidebar and an `/agents` management page
  (list + create/edit/delete), reusing the new-session pickers.

Out of scope: `horsie agent create` (agents are authored in the web UI for
now); invoking an agent from the browser; CLI commands to list
vendors/models/memories (dropped — agents capture those choices).

## Approach

Server-side invoke (chosen over client-side assembly and over a preset field on
`CreateSessionRequest`): preset→session assembly and validation live in one
place on the server, the CLI stays a thin REST client, and invoke is one HTTP
round-trip.

## Server

### Wire types — new `models/fluorite/agents.fl`

```
AgentView {
    name: String,                    // slug, id of record
    description: String,
    vendor: Option<String>,          // absent → server default vendor at invoke
    model: String,                   // configured model alias
    repos: Vec<RepoConfig>,          // reuses session_api.RepoConfig
    plugins: Vec<String>,            // skill bundle names
    mcp_servers: Vec<String>,
    memory_spaces: Vec<String>,
    thinking_effort: Option<String>,
    created_at: String,              // epoch seconds, like MemoryView
    updated_at: String,
}

AgentInput { /* same fields minus timestamps */ }   // create + full-replace update

AgentInvokeRequest {
    message: String,
    name: Option<String>,            // optional session title
}

AgentInvokeResponse { session: SessionSummary }
```

### Storage

New `agents` table in the settings DB (new migration under
`server/migrations/`), with `AgentStore` (sqlx) + `AgentService` in a new
`server/src/agents/` module mirroring the `memory` module's shape. Per the
project's protocol-types-are-not-storage-types rule, the store owns its row
type and maps to/from the fluorite wire types at the HTTP boundary.

### Save-time validation (422)

- `name` must be a slug (lowercase letters, digits, `.`, `_`, `-`, starting
  with a letter or digit — same rule as memory spaces).
- `model` must reference a configured model alias.
- `thinking_effort`, if set, must be offered by that model.

Vendor, plugins, MCP servers, and memory spaces are **not** validated at save —
they are live/external rosters that can legitimately change after the preset is
stored; they are validated at invoke.

### HTTP API — new `server/src/http/agents.rs`, routes in `http/mod.rs`

| Route | Behavior |
|---|---|
| `GET /api/agents` | List all agents → `Vec<AgentView>` |
| `POST /api/agents` | Create; 201 + `AgentView`; 409 on duplicate name |
| `GET /api/agents/:name` | 200 + `AgentView`; 404 unknown |
| `PUT /api/agents/:name` | Full replace from `AgentInput`; 404 unknown |
| `DELETE /api/agents/:name` | 204; 404 unknown |
| `POST /api/agents/:name/invoke` | Create session from preset + queue message → 201 + `AgentInvokeResponse` |

### Invoke flow

1. Load the agent → 404 if unknown.
2. Reject empty/whitespace `message` → 422.
3. Invoke-time validation: the resolved vendor (agent's, else server default)
   must be in the live connected roster → 422 naming it; the model must still
   be configured → 422.
4. Assemble a `SessionSpec` from the preset. This reuses the exact construction
   logic of `create_session` (repos → provision steps, capability defaulting,
   thinking-effort resolution, plugins imply `use_plugins`), factored into a
   shared helper so the two paths can never drift. Stale plugin/space/MCP
   references follow existing session-creation behavior rather than adding new
   checks.
5. `SessionSupervisorCommand::Create` → session id.
6. `SessionSupervisorCommand::UserMessage` with the invoke message →
   accepted/queued; the turn runs in the background. A queue during an
   in-flight turn is accepted, never rejected.
7. Return 201 + `AgentInvokeResponse { session }` immediately — no waiting for
   provisioning or the first turn.

If step 6 fails after the session exists (e.g. unrecoverable session state),
the error is surfaced as-is; the session already exists and is visible in the
list, same as if the message had been sent a moment later from the UI.

### Error handling

All errors use the existing `ApiError` envelope. 409 duplicate name; 404
unknown agent; 422 as above; 500 only for genuinely internal failures
(supervisor mailbox closed).

## CLI

New top-level command group (in `cli/src/main.rs`), plus two additions to the
existing `session` group. All take `--server` (default
`http://127.0.0.1:3789`), the same convention as `session tail`.

```
horsie agent list                       # table: NAME MODEL VENDOR PLUGINS MEMORY DESCRIPTION
horsie agent get <name>                 # full preset detail (repos, mcp, thinking effort, …)
horsie agent invoke <name> -m <message> # prints session id + link
horsie session list                     # table: ID NAME STATUS CREATED LAST-ERROR
horsie session status <session-id>      # status, model, vendor, last error,
                                        # pending question, queued inbox
```

Invoke output — two lines, immediately after the 201:

```
session 3f8a2c1e-…
http://127.0.0.1:3789/sessions/3f8a2c1e-…
```

The id on its own line keeps it script-friendly; the link is clickable in most
terminals. Exit codes: 0 success, 1 server error (the server's `ApiError`
message goes to stderr), 2 local/usage errors — matching existing CLI
conventions. `session status` is a point-in-time snapshot; live progress
streaming remains `session tail`'s job.

### Implementation

New `cli/src/server_client.rs` — a small reqwest REST client (get/post helpers
+ typed methods `list_agents`, `get_agent`, `invoke_agent`, `list_sessions`,
`get_session`) mapping non-2xx responses into `CliError::Server` via the
`ApiError` envelope. Separate from the unix-socket `client.rs` (daemon
protocol) and from `session.rs`'s SSE code. Wire types come straight from
`horsie_models` (`agents`, `session_api`) — no hand-rolled JSON shapes.

## Web UI

**Sidebar** (`Sidebar.tsx`): a new "Agents" section above the session list — an
Agents nav-link to `/agents` in the existing Settings/Admin link style, with
the session list below as the second section. Both live under the existing
`SessionsLayout`.

**Agents page** (`/agents`, new `pages/agents/`):

- List view: one row per agent — name, description, model, vendor, and count
  badges (skills/memory/MCP). Delete with a confirm, consistent with existing
  destructive actions.
- Create/edit view (`/agents/new`, `/agents/:name/edit`): a form reusing the
  pickers the new-session draft uses — model select, vendor select, repo
  picker (GitHub), skills, MCP servers, memory spaces, thinking effort — and
  their data hooks (`useSettings`, `usePlugins`, `useMcp`, `useMemorySpaces`,
  `useGithubStatus`). Fields map 1:1 onto `AgentInput`. Save → POST/PUT
  `/api/agents`, then back to the list.
- Data layer: `api/agents` in `client.ts` + a `useAgents` hook (react-query),
  mirroring `useSessions`/`useMemory` patterns.

Generated TS types regenerate from `agents.fl` via the existing fluorite
codegen for `clients/ts` and `clients/web`.

Not in the UI this iteration: invoking an agent from the browser.

## Testing

- **Server** (HTTP `oneshot` tests in the existing style with
  `FakeRuntimeVendor`): agent CRUD round-trip incl. 404/409/422 paths; invoke
  with the fake vendor → 201, session appears in `GET /api/sessions`, and the
  invoke message lands in the session inbox/history; invoke with a
  disconnected vendor → 422; invoke while a turn is in flight → message
  accepted, not rejected.
- **Models**: `agents.fl` types are generated; any hand-written helpers get
  unit tests in `models/src/lib.rs`.
- **CLI**: unit tests for arg→request mapping and output formatting (id line +
  link line); server-error → exit 1 with message. Follow the
  `cli/tests/connect_e2e.rs` pattern for whether a live e2e against
  `horsie-server` is practical.
- **Web UI**: follow the repo's existing web test setup (`clients/web/e2e/`) —
  agents CRUD happy path at minimum.
- Pre-PR gates: `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo fmt --check`, `cargo test --workspace`, plus the web UI's own
  lint/test commands.
