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

> **Amendment (2026-07-21):** the first version of this tool held state in the
> `Tool` instance itself (reset on an actor restart) on the reasoning that the
> model would always see the last-known list from its journaled conversation
> history anyway. The user asked for it to be persisted along with session
> state instead — i.e. durable across a restart the same way timers are, not
> just recoverable-in-spirit from message history. The sections below describe
> the corrected, durable design; the superseded ephemeral version is kept out
> of this doc entirely rather than left as dead narrative.

**`task_list` is durable agent state, journaled exactly like timers.**
`TaskListState` (an ordered `Vec<TaskRecord>` + an id counter) lives on
`AgentState` (`workflow/src/agent_actor.rs`) next to `messages` and `timers`.
Every mutation (`create`/`insert`/`update_status`) persists one
`AgentDomainEvent::TaskListChanged { snapshot }` event carrying the *whole*
resulting state (not a delta — mirrors how `MessageComplete`/`ToolComplete`
carry full content, not diffs), folded into `AgentState` by `apply_event` on
recovery. This is the same mechanism `TimerArmed`/`TimerCancelled`/`TimerFired`
already use, so a cold-restarted actor reconstructs the exact task list a live
one would have, not just "the model can infer it from past messages."

**The tool executes by `ask`ing the owning actor, never the sandboxed
runtime** — the same pattern as `set_timer`/`list_timers`/`cancel_timer`
(`TimerToolbox` in `agent_actor.rs`). A new `TaskListToolbox` wraps the
agent's toolbox in `AgentActor::start_run`, adding the `task_list` spec and
routing calls to a new `AgentCommand::TaskListOp { action, reply }`. The
command handler clones `state.task_list`, calls `TaskListState::apply`
(pure, data-only — no `ToolCallError`/JSON coupling, so it's cheap to unit
test and to fold on recovery), and either persists `TaskListChanged` +
replies with the rendered list, or replies with an error and persists
nothing (an invalid mutation leaves no trace, matching how a rejected
`cancel_timer` with an unknown id is simply a no-op).

**It's wrapped unconditionally, not gated by an `allow_task_list` flag or
`allowed_tools`.** Unlike timers (opt-in per agent via `allow_timers`, since
arming timers has real resource/scheduling cost), `task_list` is layered on
every agent the same way `skill`/`inspect_workspace` always are — it's a
working-memory aid with no cost to leaving on, so there's no reason to make
it a per-agent toggle. This does mean it bypasses `allowed_tools` entirely,
same as `skill`/`inspect_workspace`/timers already do; a tightly-scoped
workflow sub-agent still gets it.

Why not the original ephemeral design (state on the `Tool` instance, no
`AgentCommand`/`AgentDomainEvent` involvement)? It's simpler, but "the model
can still see the last list in its journaled message history after a
restart" isn't the same guarantee as "the state itself survived" — the very
next mutation after a cold restart (e.g. `update_status` on an id from before
the restart) would fail with "unknown task id" against an empty in-memory
list, even though the model just saw that id in its own context. Journaling
the state directly removes that gap.

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

- Unit tests co-located in `workflow/src/task_list.rs` (per `CLAUDE.md`
  test-co-location convention): each action's happy path against
  `TaskListState::apply` directly (no JSON/tool plumbing needed for these),
  the full-list-in-response behavior, and every validation error (unknown
  action, empty `tasks`, out-of-range `position`, empty `ids`, unknown `id`,
  missing `status`).
- `workflow/src/agent_actor.rs`: `task_list_events_fold_into_state` exercises
  `apply_event` directly, mirroring the existing `timer_events_fold_into_state`
  test.
- `workflow/tests/workflow_e2e.rs`: `task_list_persists_across_journal_reconstruction`
  drives a real agent through a mock LLM issuing `task_list` tool calls, then
  folds the journal fresh (the same helper `agent_session_history_reconstructs_from_journal`
  uses) and asserts the reconstructed state already has the mutation — the
  actual durability guarantee, not just a unit-level fold.
- Run the standard pre-PR gate: `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo fmt --check`, `cargo test --workspace`.
