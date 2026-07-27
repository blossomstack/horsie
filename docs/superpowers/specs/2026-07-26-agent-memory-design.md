# Agent memory: server-persisted memories with a CRUD tool set

Date: 2026-07-26
Status: approved, ready for planning

## Problem

Horsie session agents start every session with no recollection of prior ones. Anything the user explained last week — an operational gotcha, a preference about how work should be done, a fact about an external system that is not written down in any repo — has to be explained again.

This design adds **memories**: durable, agent-managed notes persisted server-side in the session server's SQLite database, surfaced to the agent as a compact index in the system prompt, loaded on demand, and managed by the agent through a small tool set (create / update / delete / load / list) and by the user through a web page.

## Constraints discovered in the codebase

Three properties of the current server shape the design:

**There is no user concept.** The session server is single-tenant with no authentication. `github_app` and `github_credentials` are single-row tables (`CHECK (id = 1)`), and `server/migrations/0002_github.sql` states the intent outright: no user/tenant layer. So "per-user memory" is equivalent to "deployment-global memory", and any scoping must be invented rather than derived from an identity.

**There is no project concept.** Sessions have a vendor, an optional list of repos (`RepoConfig { url, git_ref, dir }`), and exactly one workspace, always named `main`. Repo URL is the only latent project-like key, and it is absent for sessions that check out nothing.

**Server-executed tools are an established pattern.** `McpToolbox` (`workflow/src/mcp_toolbox.rs:47`) and `TaskListToolbox` (`workflow/src/agent_actor.rs:1163`) both execute in the server process and never reach the sandboxed runtime. A memory toolbox follows the same shape, so no runtime, executor, or wire-protocol changes are needed.

One constraint bounds the design: `CompositeToolbox::execute` calls `specs()` on every composed toolbox for every tool call (`mcp_toolbox.rs:33`). Tool specs must therefore be static and cheap — no database reads. Anything dynamic, such as the list of existing memories, must travel through the system prompt instead, which is assembled asynchronously once per turn.

## Design decisions

Four decisions were settled during brainstorming. Each rejected the alternatives listed.

### Scope: named memory spaces, selected per session

A **memory space** is a named, human-curated namespace holding a flat set of memories. Sessions select zero or more spaces at creation, using the same multi-select UI pattern as plugins and MCP servers.

Rejected: global-only (memories from unrelated work pollute every session's index); global + per-repo inferred from `RepoConfig.url` (implicit, and useless for sessions with no repo).

Spaces are deployment-global because there is no user to own them. If authentication ever lands, the space is the natural thing to attach ownership to.

### Retrieval: index in the prompt, bodies on demand

The system prompt carries a compact index — one line per memory, giving its address and a one-line description. The agent calls `memory_load` to pull the full bodies of the memories it judges relevant.

Rejected: inlining every body every turn (token cost grows linearly and is paid forever); a search tool with no index (the agent does not know anything exists, so it never searches — the documented failure mode of archival-only memory); pinned bodies plus indexed rest (an extra concept and UI toggle for a problem we do not have yet).

At the scale this will realistically reach — tens to low hundreds of memories — a model selecting from a listed index outperforms embedding recall and costs nothing to build. No vector store, no embeddings.

### Structure: flat list per space, free-text links

Memories are a flat set within a space. A body may reference another memory as `[[space/name]]`; the agent follows the reference by calling `memory_load` again. There is no link table and no referential integrity — a dangling link is just text.

Rejected: an explicit graph with a link table and traversal (multi-hop retrieval is not a need agent coding memory actually has, and the surface area is large); hierarchical paths (forces a placement decision models get wrong, and renders poorly in a compact index); typed tags (risk of the agent inventing inconsistent tags over time; the space already supplies one level of grouping).

### Write path: model tool-call, no approval gate

The agent writes when it judges something is durable and non-obvious, typically because the user asked it to remember. Writes land immediately. Curation is after the fact, in the web UI.

Rejected: a pending-approval workflow (a whole state machine and UI, and the agent cannot rely on a memory it just wrote); background extraction from the conversation (an extra LLM call per turn, plus a dedup and contradiction decision, and it is the main source of memory pollution in systems that do it).

Writes are not silent: every one appears as a tool call in the session transcript.

## Data model

New migration `server/migrations/0008_memory.sql`:

```sql
CREATE TABLE memory_spaces (
    name        TEXT PRIMARY KEY,
    description TEXT NOT NULL DEFAULT '',
    created_at  TEXT NOT NULL,              -- unix epoch seconds
    updated_at  TEXT NOT NULL
);

INSERT INTO memory_spaces (name, description, created_at, updated_at)
    VALUES ('default', 'Default memory space', strftime('%s','now'), strftime('%s','now'));

CREATE TABLE memories (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    space       TEXT NOT NULL,
    name        TEXT NOT NULL,
    description TEXT NOT NULL,
    content     TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    UNIQUE (space, name)
);

CREATE INDEX idx_memories_space ON memories(space);
```

Timestamps follow the established convention: `TEXT` columns holding unix epoch seconds, generated in the service layer and passed to the store as strings, exactly as `server/src/plugins/store.rs` and `0003_mcp.sql` do.

**`memories.space` is deliberately not a SQL foreign key.** No migration in this schema uses `REFERENCES`, and `PRAGMA foreign_keys` is never enabled in `open_pool` (`server/src/config/store.rs`), so SQLite would silently ignore a declared constraint — an `ON DELETE CASCADE` that never fires is worse than no constraint at all. Instead the store enforces the relationship explicitly, each of these in a single transaction:

- **Delete a space** — delete its memories, then the space row.
- **Rename a space** — insert the new space row, `UPDATE memories SET space = ?` for every child, then delete the old row. (A bare `UPDATE memory_spaces SET name = ?` would orphan every memory in it.)
- **Create a memory** — verify the space exists before inserting.

Turning the pragma on globally was considered and rejected: it changes behaviour for every existing table at once, which is a larger blast radius than this feature warrants.

A memory is addressed as **`space/name`** in the prompt index and in every tool argument that refers to an existing memory. The two renderings are identical, so the agent can copy an address straight out of the index. `space` and `name` are slugs; the store validates them against `^[a-z0-9][a-z0-9._-]*$` and rejects anything else, so an address never contains an ambiguous `/`.

Migration `0008` seeds exactly one space, `default`. The seeded row is an ordinary row: the user may rename or delete it like any other.

`MemoryStore { pool: SqlitePool }` and `MemoryService` follow the pattern of `server/src/plugins/{store,service}.rs` — runtime `sqlx::query` with `bind`, `Result<_, String>` errors, a manual `row_to_*` mapper, and in-file `#[tokio::test]` coverage against a temporary SQLite database with `sqlx::migrate!()` applied.

## Session wiring

`memory_spaces: Vec<String>` is added to **`AgentSettings`**, not to `CreateSessionRequest` directly. This mirrors `mcp_servers` exactly, which is the closest existing analogue: a set of server-side toolboxes selected by name. `SessionContextProvider` already captures `settings`, so `provide()` needs no new plumbing.

- Wire: `models/fluorite/session.fl`, `AgentSettings.memory_spaces: Option<Vec<String>>`.
- Storage: `server/src/sessions/spec.rs`, `AgentSettings.memory_spaces: Vec<String>` with `#[serde(default)]`. The default is mandatory — `SessionSpec` is journaled and old rows must still deserialize.
- Translation: `settings_from_wire` in `server/src/http/handlers.rs`.

`ServerDeps` and `AppState` each gain `memory: Option<Arc<MemoryService>>`, following how `mcp` and `plugins` are already optional. Both test fixtures — `server/src/http/mod.rs` `test_state` and `server/src/sessions/supervisor.rs` `test_deps` — pass `None`.

Space selection is **immutable after session creation**, consistent with how plugins and MCP selections are rendered as `LockedChip`s post-create in `SessionConfigBar`.

Spaces are created, renamed, and deleted **only through the web UI**. There is no tool for the agent to create a space, so the namespace stays human-curated and does not accumulate near-duplicates.

## Tool surface

The memory toolbox is exposed **only when the session selected at least one space**. It wraps outside `DefaultToolboxFactory`, in the same position as `AskUserToolbox` (`server/src/sessions/session_actor.rs:888`).

This means memory tools **bypass `allowed_tools`**. That is deliberate: space selection is already an explicit opt-in, and having `allowed_tools` act as a second, silent gate would mean a session that sets an exhaustive allowlist loses its memory tools without any signal. One gate, not two.

| Tool | Arguments | Behaviour |
| --- | --- | --- |
| `memory_load` | `refs: [string]` | Returns full bodies. Batched, so several memories cost one round-trip. An unknown ref yields an error entry in the result rather than failing the whole call. |
| `memory_create` | `space?`, `name`, `description`, `content` | `space` may be omitted when exactly one space is selected; it is required otherwise, and omitting it then is an error naming the available spaces. A name that already exists is an error pointing at `memory_update`. |
| `memory_update` | `ref`, `description?`, `content?` | Full replacement of the supplied fields. Memories are small enough that patch semantics are not worth the failure modes. |
| `memory_delete` | `ref` | Removes the memory. |
| `memory_list` | `space?` | Re-reads the index. Needed because the prompt index is built at turn start and goes stale after a write within the same turn. |

Tool specs are static `serde_json::json!` blobs, matching `ToolSpec` in `agentcore/src/tool.rs`. The toolbox captures the selected space names and an `Arc<MemoryService>` at construction, which happens once per turn inside `provide()` — `Toolbox::execute` receives only `(name, input)` and has no other context available.

All writes are restricted to the session's selected spaces. A `space` argument naming an unselected space is an error, so a session cannot reach outside its declared scope.

## Prompt injection

The prompt contribution splits by what is static and what is dynamic.

**Rules** live in the static `server/src/sessions/system_prompt.md`, as a `## Memories` section alongside the existing `## Skills` section. It states: save only what is durable and non-obvious; do not save what the repo already records; prefer updating an existing memory over adding a near-duplicate; memories are point-in-time observations, so verify anything a memory claims about code before asserting it as fact.

**The index** is appended in `SessionContextProvider::provide()`, immediately after the `compose_system_prompt(...)` call at `server/src/sessions/session_actor.rs:896`. Appending in the session layer keeps the change out of the `workflow` crate, so workflow agents are unaffected.

```
# Memories

Saved notes from earlier sessions. Load one with the memory_load tool before relying on it.

## default

- default/prod-key-rotation — rotation is manual, and why the automated path was abandoned
- default/homelab-deploy-order — velos must be up before the server, or provisioning fails
```

Cost is one indexed `SELECT` per turn, on the latency path of every user message. Descriptions are capped at 200 characters at write time; the rendered index is capped at 200 entries, and when it truncates, the prompt says so explicitly rather than cutting silently.

When the session selected spaces but they contain no memories, the section still renders with a note that no memories exist yet, so the agent knows the facility is available.

## HTTP API and web UI

Routes are kept flat to avoid the axum `matchit` conflict between a path parameter and a static sibling that previously forced the plugin artifact route to live at `/api/plugin-artifacts/:file`:

- `GET /api/memory-spaces`, `POST /api/memory-spaces`
- `PUT /api/memory-spaces/{name}`, `DELETE /api/memory-spaces/{name}`
- `GET /api/memories?space=<name>`, `POST /api/memories`
- `GET /api/memories/{id}`, `PUT /api/memories/{id}`, `DELETE /api/memories/{id}`

Handlers stay thin, in a new `server/src/http/memory.rs`, following `server/src/http/plugins.rs`: `State(state)`, `Path`, `Json` of a fluorite type, returning `Result<Json<View>, Api>` and mapping service `String` errors onto `Api::not_found` / `Api::unprocessable` / `Api::internal`.

Wire types go in a new `models/fluorite/memory.fl`, with a module include in `models/src/lib.rs`. The file must also be added to the explicit `-i` lists in `clients/web/package.json`, `clients/ts/package.json`, and `make ts-types` in the `Makefile` — those lists are enumerated, not globbed.

Web UI, following the `SkillsPage.tsx` shape:

- `api.memory.*` in `clients/web/src/api/client.ts`
- `clients/web/src/hooks/useMemory.ts`
- `clients/web/src/pages/MemoryPage.tsx` — space list with create/rename/delete, memory list per space, and inline edit and delete of a memory
- a `/memory` route in `App.tsx` and a sidebar nav entry
- a memory-spaces multi-select chip in `SessionConfigBar` and a `memorySpaces: Set<string>` field in `useSessionDraft`, alongside the existing `skills` and `mcp` sets

## Testing

- `MemoryStore` unit tests against a temporary SQLite database with migrations applied: CRUD, the `UNIQUE (space, name)` constraint, slug validation, that deleting a space removes its memories, that renaming a space carries its memories across and leaves none orphaned, and that creating a memory in a nonexistent space is rejected.
- Memory toolbox unit tests over a real store: each tool's happy path, the omitted-`space`-with-multiple-selected error, the duplicate-name error, the unselected-space rejection, and `memory_load` with a mix of known and unknown refs.
- HTTP CRUD test in `server/src/http/mod.rs`'s `mod tests`, matching `plugins_install_list_artifact_delete_over_http`.
- Session-level test asserting that the index reaches the assembled system prompt for a session with selected spaces, that it is absent when no space is selected, and that a `memory_create` tool call round-trips into the store.

Full gate before the PR: `cargo fmt --check`, `clippy --workspace --all-targets --all-features -D warnings`, `cargo test --workspace --all-features`, `cargo deny`, ts-drift for `clients/ts`, and a `clients/web` build.

Note that CI runs nightly rustfmt with import-wrapping settings that local stable `cargo fmt` silently accepts. Formatting must be verified through CI, not a local nightly run.

## Accepted limitations

**Journal replay is not deterministic for memory tools.** Replaying a session re-plays tool calls whose recorded results may no longer match the current database. This is identical to the existing behaviour of MCP tools, which are also outside the actor journal. Documented, not fixed.

**No user scoping.** Spaces are deployment-global because the server has no user concept. Whoever can reach the server can read and write every memory. This matches the server's existing trust model, where the database file is the trust boundary.

**No dedup or contradiction resolution.** The agent is instructed to prefer updating an existing memory over adding a near-duplicate, but nothing enforces it and no merge pass runs. Duplicates and stale memories are cleaned up by the user in the web UI. The "verify before asserting" instruction in the prompt is the mitigation for staleness.

**No search.** Retrieval is the agent choosing from the index. If the corpus ever grows past a few hundred memories the index will need paging or filtering, at which point a search tool becomes worth adding. Not now.
