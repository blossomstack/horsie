# Runtime working-directory and environment tools

## Context

The runtime's tool set (`bash`, `read_file`, `write_file`, `find_and_replace`,
`replace_lines`, `list_files`, `glob`, `grep`) is stateless: every call
resolves its base directory fresh from the `workspace` field, and every `bash`
command inherits the runtime process environment unchanged. An agent that
wants to work in a subdirectory or with adjusted environment must repeat
`cd ... && ...` / `VAR=... ...` prefixes on every single command, and file
tools have no way to follow at all.

Requested: two new tools — `set_working_dir` and `set_env` — that mutate
shell-like state held by the runtime process, so all future tool calls respect
the new working directory and future `bash` commands respect the env changes.

A runtime process is not always private to one session: the local-daemon
vendor (`server/src/vendor/local.rs`) lets multiple sessions attach to one
runtime, and the roadmap has an agent and its subagents sharing one runtime.
The state must therefore be **isolated per caller**: a session identity is
plumbed through the tool-call protocol and the runtime keys its cwd/env state
by it.

## Existing architecture

- `models/fluorite/runtime.fl` defines the tool protocol: one input struct per
  tool, a `ToolCall` union whose tag doubles as the tool name, and
  `ToolCallRequest { call_id, call }` — the envelope every tool invocation
  arrives in. Codegen exposes the types as `horsie_models::runtime::*`.
  `executor.fl` reuses `ToolCallRequest` inside `ToolCallCmd`, so a field
  added there flows through the server → executor → runtime chain.
- `runtime/src/tools/mod.rs` `dispatch(registry, call)` resolves the call's
  `workspace` field to a root directory via `WorkspaceRegistry::resolve` (the
  single name→path translation site), runs the tool's `exec(working_dir,
  input)`, and clamps output.
- `runtime/src/main.rs` `run_loop` spawns each tool call as a concurrent task
  sharing an `Arc<WorkspaceRegistry>`; calls may interleave.
- `runtime/src/tools/bash.rs` spawns `bash -o pipefail -c <command>` with
  `current_dir(working_dir)`; the child inherits the runtime process env.
- File tools take the same `working_dir` and resolve relative paths against
  it, so passing a different base dir changes their resolution transparently.
- `runtime-client/src/client.rs` `RuntimeClient` is the per-session handle
  agents invoke tools through; it delegates to a `RuntimeTransport`
  (`executor/src/socket_transport.rs` in-process,
  `executor-client/src/ws_transport.rs` relayed, `MockTransport` in tests),
  which builds the `ToolCallRequest`.
- `runtime-client/src/tools/` implements the agent-facing `Tool`s (name,
  description, JSON schema) and forwards calls via `RuntimeClient::invoke`;
  `add_runtime_tools` registers them all. `with_workspace` injects the
  standard optional `workspace` property.
- `DefaultToolboxFactory::for_agent` (`workflow/src/context.rs`) is the single
  place runtime tools are assembled; its one production caller is
  `SessionContextProvider::provide` (`server/src/sessions/session_actor.rs`),
  which knows the `session_id`.

## Decisions (from brainstorming)

- **Cwd applies to all tools.** Once set, it is the base dir for relative
  paths in every tool, not just `bash` — most shell-like.
- **Global cwd, not per-workspace.** Within a caller's state there is one
  cwd; once set it applies regardless of the `workspace` field. Resetting is
  done by calling `set_working_dir` without `path`.
- **Env applies to `bash` only.** File tools are in-process Rust; the overlay
  is applied to spawned child processes.
- **State isolated per session.** `ToolCallRequest` gains an optional
  `session_id`; the runtime keys cwd/env state by it. `RuntimeClient` carries
  the id and stamps every request; the server stamps its session id when
  building the agent's toolbox. Calls without an id (older clients, direct
  test invokes) share a default bucket. A future subagent sharing the runtime
  stamps its own id for isolation, or its parent's to share deliberately.

## Design

### Protocol (`models/fluorite/runtime.fl`)

```fl
struct SetWorkingDirInput { path: Option<String>, workspace: Option<String> }
struct SetEnvInput { name: String, value: Option<String> }

union ToolCall {
    ...existing variants...,
    SetWorkingDir(SetWorkingDirInput),
    SetEnv(SetEnvInput),
}

// session_id keys the runtime's per-caller cwd/env state; absent = default bucket.
struct ToolCallRequest { call_id: String, session_id: Option<String>, call: ToolCall }
```

`executor.fl` embeds `ToolCallRequest` unchanged, so no executor-protocol
edit is needed — but every constructor of `ToolCallRequest` (executor's
`socket_transport.rs`, executor-client's `ws_transport.rs`, tests) gains the
field, sourced from the transport's new parameter.

### Session-identity plumbing (runtime-client + transports)

- `RuntimeTransport::invoke` gains a `session_id: Option<&str>` parameter
  (before `call`). Implementations thread it into `ToolCallRequest`.
- `RuntimeClient` gains a `session_id: Option<String>` field (default `None`)
  and a builder:

  ```rust
  /// Stamp every invoke with this caller identity; the runtime keys its
  /// per-caller cwd/env state by it. Cheap — shares the inner Arcs.
  #[must_use]
  pub fn with_session_id(self, session_id: String) -> Self
  ```

  `invoke` passes `self.session_id.as_deref()` to the transport.
- `MockTransport` records received session ids alongside invocations so tests
  can assert the stamp reaches the wire.
- `SessionContextProvider::provide` stamps the session's id:
  `self.runtime_client.clone().with_session_id(self.session_id.to_string())`
  when calling `DefaultToolboxFactory::for_agent`. This is the one production
  assembly point, so every interactive session agent gets isolation; future
  subagents stamp their own id (or their parent's, to share).

### Runtime state (`runtime/src/state.rs`, new)

```rust
/// Per-caller shell-like state: cwd override + env overlay, keyed by the
/// session id stamped on each tool call (`None` = default bucket shared by
/// unidentified callers). Entries live for the runtime process's lifetime —
/// bounded by the number of distinct callers attaching to it.
pub struct RuntimeState {
    sessions: Mutex<HashMap<Option<String>, SessionEnv>>,
}

#[derive(Default)]
struct SessionEnv {
    /// Working-directory override; `None` = resolve per call from `workspace`.
    cwd: Option<PathBuf>,
    /// Env overlay for spawned commands: `Some(v)` = set, `None` = unset.
    env: HashMap<String, Option<String>>,
}

/// Named fields over a tuple so call sites can't swap sets and unsets.
pub struct EnvOverlay {
    pub sets: Vec<(String, String)>,
    pub unsets: Vec<String>,
}
```

One `std::sync::Mutex` guards the map — critical sections are tiny and never
held across `.await`; lock poisoning is recovered with
`unwrap_or_else(PoisonError::into_inner)`, matching the existing pattern.
Methods (each taking `session: &Option<String>`):
`effective_dir(session, fallback)`, `set_cwd(session, dir: Option<PathBuf>)`,
`apply_env(session, name, value)`, `env_overlay(session) -> EnvOverlay`.

`run_loop` creates one `Arc<RuntimeState>` per connection and passes it, plus
the request's `session_id`, into
`dispatch(registry, state, session_id, call)`.

### Dispatch changes (`runtime/src/tools/mod.rs`)

- Dispatch handles the two state-mutating variants first: they run against
  `registry` + `state` + the call's session key and return a confirmation
  `ToolOutput`. `workspace_of` keeps covering only the eight dir-based tools;
  `SetEnv` has no `workspace` field, so the helper gains an explicit
  `ToolCall::SetEnv(_) => &NONE` arm over a `const NONE: Option<String>`
  (wildcards are lint-denied).
- For ordinary tools the base dir becomes
  `state.effective_dir(session, registry.resolve(workspace)?)` — the override
  wins when set, otherwise today's per-call resolution is unchanged.
- `bash::exec` gains an `env: &EnvOverlay` parameter applied after spawn
  setup: `command.envs(&env.sets)` then `command.env_remove(each unset)`.

### `set_working_dir` semantics

Input `{ path: Option<String>, workspace: Option<String> }`:

- `path` present: absolute path used as-is; relative path resolved against the
  caller's *current* effective cwd (its cwd override if set, else the resolved
  workspace root). The target must exist and be a directory — otherwise a
  `ToolError` and the state is unchanged. The path is canonicalized before
  storing so later relative resolutions are stable.
- `path` absent: reset — clear the caller's override so resolution returns to
  per-call `workspace` handling. `workspace` is validated through
  `registry.resolve` first, so naming an unknown workspace errors, and with
  several workspaces an absent `workspace` errors listing the options
  (reusing existing `resolve` behavior); with a single workspace,
  `set_working_dir {}` resets to its root.
- Success returns the new effective directory on stdout (the resolved path,
  or the workspace root after a reset).

### `set_env` semantics

Input `{ name: String, value: Option<String> }`:

- `value` present → the caller's future `bash` children get `name=value`;
  absent/null → the variable is removed from its future children even if the
  runtime process has it (`env_remove`).
- `name` must be non-empty and contain no `=` or NUL; `value` must contain
  no NUL. Violations are `ToolError`s and change nothing.
- Success returns a one-line confirmation (`set NAME` / `unset NAME`); values
  are not echoed, so secrets don't land in the conversation history.

### Concurrency

Tool calls run as concurrent tasks. Mutations are last-write-wins *within a
caller*; callers holding different session ids never observe each other's
state. A call racing a mutation from the same session sees old or new state
depending on interleaving — matches a shared shell within one session.

### Client tools (`runtime-client/src/tools/`)

- `SetWorkingDirTool` — name `set_working_dir`. Custom schema (not
  `with_workspace`): `path?: string`, `workspace?: string` described as
  "reset resolution to this workspace's root". Description spells out:
  persists for all future tool calls of every kind in this session, relative
  `path` resolves against the current cwd, omit `path` to reset, other
  sessions on the same runtime are unaffected. Returns the new working
  directory.
- `SetEnvTool` — name `set_env`. Schema `name: string` (required),
  `value?: string` (omit to unset). No `workspace` property. Description
  spells out: applies to this session's future `bash` commands only,
  persists until changed, other sessions are unaffected.
- Both registered in `add_runtime_tools`.

### Sandbox interaction

No new escape surface: the nono/Landlock sandbox confines the process
fail-closed, so a cwd pointing outside the allowed directories simply makes
subsequent calls fail with OS errors. `set_working_dir` validates existence
only; it does not restrict to workspace roots.

## Testing

Unit tests alongside source, per repo convention:

- `state.rs`: per-session isolation (two sessions don't see each other's
  cwd/env), default bucket for `None`, effective-dir fallback/override, env
  overlay set/unset.
- `set_working_dir.rs`: absolute set; relative resolution against current
  cwd; nonexistent target errors and preserves state; reset with named
  workspace; reset via sole workspace; unknown workspace errors.
- `set_env.rs`: invalid names rejected; set then `bash` observes the
  variable; unset removes an inherited variable; confirmation output.
- `tools/mod.rs` dispatch: after `set_working_dir`, `read_file` with a
  relative path resolves against the new cwd; two session ids through the
  same dispatch stay isolated; before any set, behavior is unchanged.
- `runtime-client`: `with_session_id` stamps the transport (via
  `MockTransport`'s recorded session ids); schema/spec shape and input
  forwarding for the two new tools, following the existing per-tool test
  pattern.

Pre-PR: `cargo clippy --all-targets --all-features -- -D warnings`,
`cargo fmt --check`, `cargo test --workspace`.
