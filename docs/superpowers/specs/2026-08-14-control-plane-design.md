# The control plane: one operation table, three surfaces — design

Date: 2026-08-14
Status: approved

## Intent

An agent should be able to manage the horsie server it is running on. Creating
a workflow, fixing a routine that fired wrong, adding an agent preset, reading
last night's failed session — all of it through tools, in a session, instead of
through the web UI.

The feature the user sees is one checkbox on an agent preset. Everything below
that is about making the tool surface impossible to forget to update.

Requirements (from the user):

- A **"control plane access"** checkbox when creating an agent preset. A
  session from that preset can manage horsie: agents, workflows, routines,
  runtimes, models, and the rest.
- The scope is **everything the web UI can do, minus credentials.**
- The tools are **not HTTP clients.** A tool and the matching route run the
  same code, in-process.
- Tools are **real tool specs with JSON schemas**, organised by resource.
- **The registration is automatic.** Operations are enumerated from one place
  and become tools when marked as such — not hand-listed in a second file that
  someone has to remember to edit.
- **No guardrails beyond the checkbox.** Enabling it is the authorisation. No
  confirmation prompts, no self-reference refusals.
- Generating the shipped CLI from the same table would be good, but is **not
  required.**

Out of scope: session-level control-plane opt-in (preset-level only for now),
and backward compatibility with anything this replaces.

## The problem this is really solving

Three surfaces want to expose the same management operations, and today two of
them are hand-written and already out of sync.

`crates/cli/src/main.rs:161` defines `AgentAction` as **list, get, invoke**.
The API offers list, get, create, replace, delete and invoke. The CLI is
missing three verbs, and nothing anywhere reports that.

A control toolbox written the same way — by hand, beside the routes — would
drift the same way, and would drift silently. So the design goal is not "add a
toolbox". It is: **make it structurally impossible for a management operation
to exist without every surface knowing about it.**

## The operation

One declaration per management operation lives in `crates/server/src/control/`.

```rust
pub struct Operation {
    /// Groups operations into one tool: "agents", "workflows", …
    pub resource: &'static str,
    /// The `action` value within that tool: "list", "create", "invoke", …
    pub action: &'static str,
    pub method: Method,
    /// axum path template. Every `{param}` must be a field of the input type;
    /// the HTTP adapter merges path and query params into the input object, so
    /// the tool and the route see one identical shape.
    pub path: &'static str,
    /// Written for the model, and doubles as the OpenAPI summary if we ever
    /// want one.
    pub summary: &'static str,
    pub expose: Expose,
    /// Derived from the input type, never hand-written.
    pub schema: Value,
    run: Run,
}
```

`run` is the whole implementation: an async fn from
`(Arc<UserServices>, Input)` to `Result<Output, ControlError>`. The HTTP
handler does not get a copy of it — it is a fold over this table, not a
sibling.

Input schemas come from `schemars` on the fluorite-generated types.
`crates/models/build.rs:19` already adds `schemars::JsonSchema` to every
generated type, so every request body in the server already has a JSON Schema
we are simply not using yet.

### Why this works without new plumbing

`UserServices` (`crates/server/src/users.rs:97`) is the per-account bundle
holding `agents`, `workflows`, `routines`, `environments`, `mcp`, `plugins`,
`memory`, `config_store`, `vendors` and the session supervisor. Both surfaces
already resolve the same `Arc<UserServices>`:

- HTTP, through the `Scope` extractor (`crates/server/src/http/mod.rs:89`).
- The session actor, through `services()`
  (`crates/server/src/sessions/session_actor/mod.rs:279`).

So "the tool and the route run the same code" needs no bridge, no in-process
HTTP dispatch, and no forged `Principal` past the auth layer
(`crates/server/src/http/mod.rs:367`). They call one function on one object
graph.

Account isolation is inherited rather than implemented: a tool holds its own
account's services, not a filtered view of everyone's, which is the same
property the routes already rely on.

### Errors

`ControlError` — `NotFound`, `Conflict { code, message }`, `Invalid`,
`Internal` — is the shared failure vocabulary, converting into `Api` for HTTP
and `ToolCallError` for the toolbox.

The per-module `api_err` functions (`http/agents.rs:22`, `http/workflows.rs:37`
and their siblings) become `From` impls on `ControlError` and stop being
duplicated per module.

## Completeness: two tables, one test

Every route under `/api` is a row in exactly one of two tables.

| Table | Holds |
|---|---|
| `control::operations()` | Every JSON request/response route. The router is folded out of it. |
| `control::http::NON_OPERATIONS` | Everything that cannot be JSON-in/JSON-out. |

`NON_OPERATIONS` is: the SSE stream (`/api/events`), the session message
stream, the two WebSocket upgrades (`/api/runtime/connect`,
`/api/vendor/connect`), plugin artifact bytes, and the OAuth and device-login
callbacks.

A test asserts the union has no duplicate `(method, path)` and that
`http::router()` is built from nothing else. A route that skips both tables is
not mounted, so it cannot exist without being classified — which is the whole
mechanism. Enumerating an `axum::Router` after construction is not possible;
classifying before construction is, and that is why the fold is the mounting
point rather than a parallel registry.

The unit of classification is the `(method, path)` pair, not the path: `POST
/api/sessions/{id}/messages` is an operation while `GET` on the same path is
the stream. Both tables therefore contribute method routers to one path, which
the fold must merge rather than route twice — axum panics when two routes claim
one path. Confirm `Router::merge` merges same-path method routers on the
pinned axum 0.8 before building on it; if it does not, the fold collects into a
`path -> MethodRouter` map first and mounts each path once.

### Expose

```rust
pub enum Expose {
    /// Route only.
    Api,
    /// Route and tool. The common case.
    ApiAndTool,
    /// Tool only, with no route of its own.
    ToolOnly,
}
```

`ToolOnly` exists for exactly one case, and it is a real seam worth naming.

`GET /api/sessions/{id}/messages` (`crates/server/src/http/messages.rs:74`) is
two things behind one route: a **page** form that returns and closes, and an
**SSE stream** form, selected by which query params are present — deliberately,
per the comment at `messages.rs:57`, with no mode flag and no `Accept`
negotiation. The stream cannot be an operation. The page can.

So the route stays in `NON_OPERATIONS`, `sessions.read` is a `ToolOnly`
operation, and **both call the same `page()` function**. There is still one
implementation. What is lost is the structural guarantee: nothing forces that
route to keep calling `page()`. It is one route, it is named here, and the
alternative was either no session reads or churning a wire contract the web UI
depends on.

## The toolbox

`ControlToolbox` **wraps** the inner toolbox rather than composing into it, for
the reason `MemoryToolbox` already documents at
`crates/server/src/memory/toolbox.rs:5`: a session that sets `allowed_tools`
must not silently lose its control tools.

One tool per resource, named `horsie_agents`, `horsie_workflows`, and so on.
The `horsie_` prefix keeps them from colliding with runtime tools and MCP
tools, which share the same namespace.

Input schema is `action` as an enum plus a `oneOf` whose branches pin `action`
to a `const` and carry that operation's derived schema. Nothing is
hand-written; the specs are a fold over the same table.

### The system-prompt index

Following the memory-index precedent (`session_actor/context.rs:120`), the
prompt gets a one-line command index so the model's first call is a real one:

```
Available: agents {list,get,create,replace,delete,invoke} · workflows {…} · …
```

A few hundred tokens for the whole control plane.

### If `oneOf` turns out to be a problem

It was worth checking rather than guarding against. Tool schemas are passed to
providers **verbatim** — `anthropic.rs:517` inserts `input_schema` as-is,
`openai.rs:173` sets `parameters: tool.input_schema.clone()`,
`responses.rs:164` the same — and there is no `strict` field anywhere in
`crates/llm-providers`. In non-strict mode neither API validates a tool schema
against a supported subset. **A `oneOf` cannot 400 on any provider horsie
speaks to today.**

That leaves two quiet failure modes: a provider silently degrading the schema,
or a model simply handling the union badly. Both show the same symptom —
repeated `ToolCallError::InvalidInput` because the model guessed field names.

The plan, in order:

1. **Build one renderer.** `fn specs(table) -> Vec<ToolSpec>` is a single seam.
2. **Instrument the quiet modes.** Count `InvalidInput` rejections per control
   tool. A tool rejecting five calls in a row is the tell; today that would
   read as a confused agent.
3. **Verify by hand before PR 2 ships** — one control-plane session per
   configured provider kind (anthropic, openai, chatgpt backend, deepseek,
   kimi) asking for an agent preset. This is the only way to catch silent
   degradation; no test on our side can see it.
4. **If it happens**, add a second renderer: `PerOperation` — one tool per
   operation (`horsie_agents_create`), flat object schema, no union. A flat
   object with typed properties is the one thing every provider has always
   accepted. It is roughly 30 lines against the same table.

`PerOperation` is not a degradation: per-operation tools are *more* accurate
for the model, and the only cost is context — order 7–20k tokens of specs per
request instead of a few thousand, which a session dedicated to control-plane
work can carry.

Deliberately **not** built: a per-resource flat union (`action` plus every
field optional). It loses required-ness, breaks when two actions on one
resource give a field name different types, and its only edge over
`PerOperation` is size.

If a switch is ever needed it goes next to `keep_thinking_signature` on
`ProviderView` (`crates/models/fluorite/settings.fl:19`) — a per-provider quirk
flag with an existing precedent and an existing home. No flag is added now.

## What is exposed

| Resource | Tool actions |
|---|---|
| agents | list, get, create, replace, delete, invoke |
| workflows | list, get, create, replace, delete, run, retry-step |
| routines | list, get, create, replace, delete, run |
| environments | list, get, create, replace, delete |
| mcp | list, upsert, delete, test, connect |
| plugins | list, builtins, install, update, delete |
| marketplaces | list, add, remove |
| models | list-aliases, put-alias, delete-alias, list-cards, list-providers |
| runtime-vendors | list, create, replace, delete, list-connected |
| sessions | list, get, create, stop, rename, annotate, delete, agents, read |
| memory-spaces | list, create, rename, delete |

`Api`-only, never a tool:

- **Writing a model provider.** `ProviderInput` carries the API key. Reading
  providers *is* exposed, because `ProviderView`
  (`crates/models/fluorite/settings.fl:8`) already redacts to
  `has_credential: bool` — and the agent needs it, to avoid pointing a new
  preset at a provider that cannot authenticate.
- All of `/api/auth` and `/api/device`, GitHub and ChatGPT login, runtime
  credentials.

### Session reads

`sessions.read` returns one agent's log, paged. The read API is already shaped
for this: agent-scoped via `aid` (main, a subagent, or a fork), paginated via
`before` + `max`, and `max` alone means "the latest N".

Two changes from the HTTP defaults:

- **Page size defaults to 20, capped at 100**, against the HTTP defaults of 50
  and 1000 (`messages.rs:44`). A model wanting more pages back through
  `before`, the same way a human scrolls the transcript.
- **Thinking signatures are stripped** via
  `wire_redact::strip_entry_signatures` (`crates/server/src/wire_redact.rs:15`),
  the same as the HTTP path. Per its own doc comment those are 37–46% of a
  typical history response and no client reads them; skipping this would nearly
  double the cost of every read for no information.

`sessions.agents` returns the roster, so `read` has something to name in `aid`.

## The gate

`control_plane: Bool` on `AgentPresetInput` and `AgentView`
(`crates/models/fluorite/agents.fl`), on `AgentSettings`
(`crates/models/fluorite/session.fl`), a column on the agents table, and one
line at `crates/server/src/sessions/session_actor/context.rs:121`, beside where
`MemoryToolbox` is already layered in.

**Main agent only.** Subagents, workflow steps and forks do not inherit it,
following the existing precedent that session-metadata tools are main-only
(`context.rs:129`).

Two consequences, recorded rather than defended against:

- **An agent editing the preset it runs under does not affect its own
  session.** Settings are snapshotted at spawn. It takes effect next session.
- **An ops agent can read every session in its account**, including whatever
  was typed into them, and can delete every workflow the account owns. That
  follows from the checkbox being the authorisation. It is worth knowing before
  ticking it on a preset that gets invoked casually.

## Keeping the CLI possible

Everything on `Operation` except `run` is plain data: `resource`, `action`,
`method`, `path`, `summary`, `expose`, `schema`. That is a constraint on this
design, held deliberately, and it is all the CLI would need.

`crates/cli` talks HTTP with `reqwest` and depends on `horsie-models`, not on
`horsie-server`, so it cannot see the `run` closures. Two ways to close that
later, neither requiring a redesign:

- Move the metadata half of `Operation` into a small shared crate; the server
  attaches the closures, the CLI builds its tree and dispatches to `method` +
  `path` over HTTP.
- Serve the metadata from `GET /api/control/operations` and have the CLI build
  its tree from a cached snapshot. Cheaper, but `--help` then depends on having
  talked to a server once.

Whichever is picked, it is what permanently closes the missing
`agent create`/`delete` verbs. Not built now.

## Testing

- **Classification test** — every route is in exactly one table, no duplicate
  `(method, path)`, and `http::router()` is built from nothing else.
- **Per-resource round-trip** — each action driven through the toolbox against
  a real SQLite-backed `UserServices`, asserting it reaches the same service
  the route does.
- **E2e** — a preset with `control_plane` on, a mock-LLM turn that calls
  `horsie_agents` with `action: "create"`, and the row asserted afterwards.
  Poll for the result rather than for a session status: a session reports
  `Idle` twice, once after provisioning and again when the turn ends.
- The service layers already carry their own unit tests; moving handler bodies
  into operations does not change what they cover.

## Staging

| PR | Contents |
|---|---|
| 1 | `control/` module, `Operation`, `ControlError`, the router fold. Migrate agents, workflows, routines, environments. Lift the `invoke_agent` (`http/agents.rs:102`) and `start_run` (`http/workflows.rs:112`) bodies out of axum — the only real lift in the whole change, and one the repo's own "handlers are adapters" rule already wants. |
| 2 | `ControlToolbox`, the `control_plane` field and migration, the system-prompt index, the web UI checkbox. End-to-end usable over those four resources. |
| 3 | Remaining resources, including `ToolOnly` `sessions.read`. `NON_OPERATIONS` shrinks to the named exceptions and the classification test lands. |
| 4 | CLI generation. Optional, and only worth doing if PRs 1–3 leave the metadata split feeling cheap. |

PR 2 delivers a working ops agent over the four resources that motivated the
feature, rather than waiting for the full inventory.
