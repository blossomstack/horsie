# Task list tool for horsie server agents

## Context

Multi-step sessions (and workflow agents) currently have no way to externalize
a plan. The model either tracks steps implicitly in its own reasoning (easy to
drop or lose track of after a long tool-call sequence) or narrates a plan in
prose, which the user can't see update as work progresses. Requested: a
built-in tool that lets an agent create a list of tasks, insert new tasks at a
specific position, and mark one or more tasks done (or otherwise update their
status) — usable the same way Claude Code's own todo-list tool is used by the
agent driving this session — plus a mention in the session system prompt
telling the agent when to reach for it.

## Existing tool architecture (relevant pieces)

- `agentcore/src/tool.rs`: `Tool` (single tool: `spec()` + `execute()`) and
  `Toolbox` (a named set of tools) traits. `ToolboxImpl` is a generic
  `Toolbox` built by `.add()`-ing `Tool` impls.
- `runtime-client/src/tools/*`: the 8 sandbox-backed tools (`bash`,
  `read_file`, ...), each a small `Tool` impl proxying to the sandboxed
  runtime process. Composed via `add_runtime_tools` (`runtime-client/src/tools/mod.rs:74`).
- `workflow/src/context.rs` `DefaultToolboxFactory::for_agent`: builds the
  toolbox for one agent spawn — runtime tools + optional MCP toolboxes,
  narrowed by the agent's `allowed_tools` allowlist (`FilteredToolbox`), then
  wrapped in `AgentToolbox` which layers on `conclude`, `skill`, and
  `inspect_workspace` (the latter two always present, bypassing the
  allowlist — they're workspace-introspection primitives every agent needs).
- `server/src/sessions/session_actor.rs` `ensure_agent`: builds an
  `AgentRunDef` for the interactive session, calls
  `DefaultToolboxFactory::for_agent`, wraps the result in `AskUserToolbox`.
  This runs once per live agent (guarded by `self.agent.is_some()`), rebuilt
  from scratch if the session actor cold-restarts.
- `server/src/sessions/system_prompt.md`: the session agent's baseline
  prompt, with a "Doing the work" section covering tool-usage guidance
  (`grep`/`glob`/`list_files` over `bash`, `find_and_replace` vs
  `replace_lines`, batching independent calls).

## Decision

**One new tool, `task_list`, implemented as a plain `Tool`** (not a
standalone `Toolbox`), added into the same `ToolboxImpl` chain as the runtime
tools in `DefaultToolboxFactory::for_agent`:

```rust
let runtime = add_runtime_tools(ToolboxImpl::new(), runtime_client)
    .add(TaskListTool::new());
```

This makes it available to both workflow agents and interactive sessions
uniformly, and — unlike `skill`/`inspect_workspace` — it flows through the
existing `allowed_tools` allowlist exactly like a runtime tool. Rationale: a
task list is a working-memory aid, not something every agent strictly needs
to function (unlike workspace introspection), so a tightly-scoped workflow
sub-agent should be able to opt out of it via `allowed_tools` the same way it
opts out of `bash`.

**State lives in the tool instance, not the actor.** `TaskListTool` owns
`Mutex<TaskListState>` (id counter + ordered `Vec<Task>`), constructed fresh
each time `for_agent` runs (once per live agent spawn — the same lifetime as
the `RuntimeClient` handle the runtime tools already capture). This mirrors
the precedent of `AgentToolbox` holding plain fields directly rather than
threading new state through the event-sourced actor.

This intentionally does **not** follow the timers precedent (`AgentDomainEvent::TimerArmed`,
journaled and replayed via `apply_event`). Timers must survive a process
restart because a timer that silently stops firing is a functional bug. A
task list is a planning aid: every mutation's result is a rendered snapshot
of the whole list, and that snapshot is *itself* journaled as ordinary
`ToolComplete` message content (the agent's conversation history already
records every past tool result). So on a warm session the state is exactly
right, and on a cold restart the model still sees the last-known list in its
context — the in-memory copy resets, but nothing the model already said
becomes stale or contradicts what it can see. Threading task-list mutations
through `AgentCommand`/`AgentDomainEvent` (as timers do) would double the
size of this change for a durability guarantee this tool doesn't need.

**Single tool, action-tagged input** (mirrors the `kind`-tagged `conclude`
schema in `context.rs`), rather than four separate tools — keeps the tool
count an agent sees low and groups a cohesive capability under one name:

```jsonc
{
  "action": "create" | "insert" | "update_status" | "list",
  "tasks": ["..."],           // create, insert: task text, in order
  "position": 0,               // insert only; 0-based; omitted = append
  "ids": [1, 3],                // update_status only
  "status": "pending" | "in_progress" | "completed"  // update_status only
}
```

- `create` — replace the whole list with the given tasks (all `pending`),
  resetting ids to `1..N`. Used to start or fully re-plan.
- `insert` — insert one or more new `pending` tasks at `position` (default:
  end of list); ids continue from the current max.
- `update_status` — set `status` on one or more tasks by `id`. Covers "mark
  done" (`status: "completed"`) and, since the same primitive is basically
  free, also `in_progress`/back to `pending` — useful for signaling "working
  on this now," which is common practice for this kind of tool.
- `list` — read-only snapshot, no mutation.

Every action (including mutations) returns the full current list rendered as
text, e.g.:

```
Tasks (1/3 done):
[x] 1. Set up project skeleton
[>] 2. Implement API client
[ ] 3. Add tests
```

so the model never needs a separate `list` call just to see the effect of a
mutation. No `remove`/`delete` action — not requested, and out of scope for
this pass; `create` already covers "the plan changed, start over."

**Validation is atomic and rejects, not clamps.** `insert` with
`position > len` is an error (not silently clamped) so the model gets an
unambiguous signal to check the list first, rather than tasks landing
somewhere it didn't expect. `update_status` validates every id exists before
applying any change — a partially-applied batch would be a confusing state to
recover from. `create`/`insert` reject an empty `tasks` array.

## System prompt

Add a short paragraph to the "Doing the work" section of
`server/src/sessions/system_prompt.md`, next to the existing tool-usage
bullets, telling the agent to reach for `task_list` on multi-step work so
progress is visible to the user as it happens (not just narrated at the end),
and to mark a task `in_progress` when starting it and `completed` right after
finishing it rather than batching updates to the end of the turn.

## Testing

- Unit tests co-located in the new `workflow/src/task_list.rs` (per
  `CLAUDE.md` test-co-location convention): each action's happy path, the
  full-list-in-response behavior, and every validation error (unknown
  action, empty `tasks`, out-of-range `position`, empty `ids`, unknown `id`,
  missing `status`).
- `workflow/src/context.rs`: extend the existing
  `toolbox_includes_conclude_and_filters_runtime_tools`-style tests to assert
  `task_list` is present by default and honors `allowed_tools` filtering like
  a runtime tool.
- Run the standard pre-PR gate: `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo fmt --check`, `cargo test --workspace`.
