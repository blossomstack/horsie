# Subagent Support for Interactive Sessions — Design

Date: 2026-08-02
Status: Approved (design), pre-implementation

## Goal

Let any agent in an interactive session — the main agent or a subagent — spawn
further subagents to work on delegated tasks concurrently. Subagents are
full `AgentActor`s owned by the session, share the session's runtime and agent
settings, and report their result back to the agent that spawned them. The
parent/child relations form a tree, persisted in the session's journal.

## Decisions (from brainstorming)

- **Async fire-and-forget spawn.** `spawn_agent` returns immediately with the
  subagent id; the parent is notified of completion as an injected message and
  may run several subagents in parallel.
- **The tree lives in the `SessionActor`'s persisted state** (the session
  journal), not the global `SessionSupervisor` — it is per-session data, and
  the session already owns the agents (`sub_agents` seam).
- **Subagents get the main agent's toolbox minus session-metadata tools**: no
  `set_session_title`, no `ask_user` (a subagent reports to its parent instead
  of pausing the session). Everything else is kept: runtime tools, MCP, memory,
  `skill`/`inspect_workspace`, `task_list`, and the spawn/status tools.
- **Guardrails:** max tree depth 4 (constant), max concurrently-active
  subagents per session — default 8, configurable at session creation.
- **Quiet by default:** only spawn/finish notifications appear on the session
  stream; subagent transcripts are readable on demand via the history API with
  an `agent_id` parameter.

Non-goals: synchronous (blocking) spawn, user-messaging of subagents, subagent
cancellation tools, cross-session subagents.

## Architecture

`SessionActor` is the single owner of all agents in a session. All spawning
flows through its mailbox, which is the one place that enforces limits,
persists tree events, and owns the live `ActorRef`s.

```
SessionActor (journal session/<id>)  — persists the subagent tree
 ├── main_agent   AgentActor (journal agent/<session-uuid>)
 └── sub_agents   AgentActor per subagent (journal agent/<subagent-uuid>)
                   each may itself be a parent in the tree
```

### New components (server crate)

**`server/src/sessions/subagents.rs`** — the persisted tree model, pure and
unit-testable without actors:

- `SubAgentParent` = `Main | SubAgent(Uuid)`.
- `SubAgentStatus` = `Running | Completed | Failed`.
- `SubAgentRecord { id: Uuid, parent: SubAgentParent, label: String,
  task: String, depth: u32, status: SubAgentStatus, output: Option<String>,
  error: Option<String>, notified: bool }`. `notified` records whether the
  parent was told the terminal result, so delivery survives an offload (see
  Completion).
- Pure helpers over `BTreeMap<Uuid, SubAgentRecord>`: `active_count()`,
  `depth_of(parent)`, `subtree_view(from)` (indented rendering for the status
  tool), `has_active()`.

**`server/src/sessions/spawn_tool.rs`** — `SubAgentToolbox`, a server-owned
toolbox wrapper (same pattern as `SessionTitleToolbox`) layered onto **every**
agent in the session. It carries the *calling* agent's id so spawns are
attributed to the right parent, and routes execution through the session
mailbox. Two tools:

- `spawn_agent { label, task }` → asks
  `SessionCommand::SpawnSubAgent { caller, label, task, reply }`; returns the
  new subagent id, or a limit-violation error string.
- `subagent_status { id? }` → asks
  `SessionCommand::SubAgentStatus { caller, id, reply }`. With `id`: that
  node's label, status, depth, and output/error when terminal. Omitted: the
  caller's subtree rendered as an indented tree. Answered purely from
  `SessionState` — subagent actors are never woken for this.

### SessionActor changes

- New commands: `SpawnSubAgent`, `SubAgentStatus` (both with `reply`),
  `SubAgentTree` (API read), and the internal `FinishSpawnSubAgent`
  (persist-then-spawn second phase) and `ReconcileSubAgents` (post-recovery).
- New persisted events (folded into `SessionState.subagents`):
  - `SubAgentSpawned { id, parent, label, task, depth }`
  - `SubAgentRunning { id }` — a terminal node started another run, woken to
    consume child results.
  - `SubAgentCompleted { id, output }`
  - `SubAgentFailed { id, error }`
  - `SubAgentNotified { id }` — the parent's notification was sent; persisted
    in the same effect as the send so a reload never double- or never-delivers.
- `sub_agents: HashMap<Uuid, ActorRef<AgentCommand>>` (the existing seam)
  holds the live refs. There is no notification buffer, durable or otherwise:
  owed notifications are *derived* — any terminal node with `notified ==
  false` is exactly the set of owed results, recomputed at every flush point
  (a subagent outcome, a main-agent turn start), so offload and reload need
  no bookkeeping beyond the tree.
- `on_agent_outcome` branches on `session_id`: the session's own uuid keeps
  today's main-agent flow; any other uuid is a subagent outcome (see below).

## Lifecycle & data flow

### Spawn

1. Agent calls `spawn_agent` → tool asks the session.
2. Session checks limits against current state: caller depth `<`
   `MAX_SUBAGENT_DEPTH` (4) and `active_count() <` the session's
   `max_concurrent_subagents`. Violations return a tool error naming the limit
   (`"max subagent depth 4 reached"`, `"8 subagents already active"`) so the
   model can retry later or rephrase.
3. Persist `SubAgentSpawned` first (ack'd, like `RenameSession`); on journal
   failure the tool errors and no actor exists.
4. Spawn the child `AgentActor` with journal id `agent/<new-uuid>`, params
   cloned from the main agent's def (same model, allowed tools, plugins, MCP,
   memory, thinking effort, max iterations/retries) with `interactive = true`,
   no `optional_handoff_tool` (plain-text end of turn), and a toolbox composed
   **without** `SessionTitleToolbox`/`AskUserToolbox` but **with**
   `SubAgentToolbox` (caller = the new uuid). Its `AgentOutcomeSink` targets
   the session, like the main agent's.
5. Immediately `AgentCommand::Run { input: task }`; reply the id to the tool.
6. Emit a quiet `Progression` frame (`stage = "subagent_spawned"`, detail =
   label) on the session stream.

### Completion

1. The subagent's outcome arrives at the session as
   `SessionCommand::AgentOutcome` with the subagent's uuid.
2. Session persists `SubAgentCompleted`/`SubAgentFailed` (usage outcomes are
   recorded under the subagent's id, so `session_total` already aggregates the
   whole tree) and emits a quiet progression frame
   (`subagent_completed`/`subagent_failed`).
3. The parent is **notified** with a synthetic message
   (`[subagent "<label>" completed]\n\n<output>`, or the error text).
   Delivery is derived, not queued — a terminal, un-notified node is owed to
   its parent, and each flush point delivers what is owed:
   - Subagent parent idle (Completed/Failed) → woken now: `SubAgentRunning`
     + `AgentCommand::Run { input: owed results }`. A woken parent is a
     multi-cycle node: it may conclude again, which re-owes its own parent.
   - Subagent parent mid-run → stays owed; the flush that follows the
     parent's own outcome wakes it (same turn-boundary discipline as the
     session inbox).
   - Parent is the main agent → owed results merge into the main agent's
     next turn input inside `drain` (appended after any queued user
     messages, `MERGE_SEPARATOR`-joined). If the session is Idle with an
     empty inbox, an owed result *starts* a turn. An `AwaitingInput` session
     only drains on user input, so a notification never answers the user's
     pending ask by itself; a `Failed` session is never chased.
   - Every actual send to a parent persists `SubAgentNotified { id }` in the
     same command effect, so the owed/delivered distinction is durable and
     exactly-once across offloads and restarts.

### Sub-spawning

Identical path: the subagent's own `SubAgentToolbox` carries its uuid as
caller, so depth and parenting resolve naturally. A node at depth 4 cannot
spawn (its would-be children exceed the limit).

### Recovery

On session load (`on_recovery_complete`): re-spawn a resident `AgentActor`
(journal replay only, no run) for every node in the tree so transcripts stay
pageable, re-attributing nothing. Any node still `Running` at crash folds to
`Failed { "interrupted by restart" }` — recorded via a `SubAgentFailed` event
sent as a self-command after recovery, so the transition is in the log. This
matches the session's "an interrupted turn is over" rule; subagents never
auto-resume. Recovery starts **no** runs: owed results (terminal,
un-notified nodes — including freshly reconciled failures) flush at the next
turn boundary, never on load, keeping a session open free of side effects.
The trade-off: a result owed to an idle subagent parent when the process
died waits for the session's next activity to be delivered.

### Offload, stop, delete

- `PrepareOffload` refuses while the session status is `Running` **or** any
  subagent is active — a hibernate must not kill a subagent's sandbox mid-run.
- `Stop` cancels only the main agent's turn (unchanged); running subagents
  continue and their completions stay owed until the next flush point.
- `Delete`/`stop_agents` cancel-then-stop every agent (main and subs) and
  drain `sub_agents` alongside the main agent.

## API & wire changes

- `models/fluorite/session.fl`: `AgentSettings` gains
  `max_concurrent_subagents: Option<u32>` (absent → server default 8). Storage
  twin in `sessions/spec.rs` gains the same field with `#[serde(default)]` so
  old journal rows load.
- `GET /api/sessions/:id/history` gains an optional `agent_id` query param
  (default `main`); the session dispatches `GetHistory` to that agent's
  resident actor. Unknown id → 404.
- `GET /api/sessions/:id/subagents` (new): the tree as
  `Vec<SubAgentView { id, parent, label, depth, status, error? }>` for client
  tree rendering; output is deliberately excluded (read transcripts via
  history).
- Session SSE: spawn/finish surface as `Progressed` frames with the stable
  stage keys above — no new union variants, no journaled streaming events for
  subagents.

## Error handling

| Case | Behavior |
|---|---|
| Depth/concurrency limit hit | Tool error naming the limit; nothing persisted |
| Journal write fails on spawn | Tool error; no actor spawned (persist-then-spawn) |
| Subagent run fails (provider/tool) | `SubAgentFailed` node; parent notified with error text; session status unaffected |
| Crash with subagents running | Nodes fold to `Failed("interrupted by restart")` at load; parents notified at the next turn boundary |
| Notification owed when the session offloads | Rebuilt from terminal nodes with `notified == false`; delivered with the parent's next turn — exactly once, no replay storm |
| `subagent_status` on unknown id | Tool error listing nothing sensitive: `"no such subagent: <id>"` |

## Testing

- **Pure fold tests** (`subagents.rs`, `session_actor.rs`): tree events fold
  correctly; depth/active-count helpers; interrupted-on-recovery fold.
- **Tool tests** (`spawn_tool.rs`): spec shape; both tools route through the
  mailbox; limit and unknown-id errors surface as tool errors.
- **Toolbox composition tests**: a subagent's specs lack `set_session_title`
  and `ask_user`, and include `spawn_agent`/`subagent_status`; the main
  agent's include all four.
- **Actor tests** (fake vendor + scripted mock provider, existing patterns):
  - spawn → async completion → parent receives the notification as a new turn;
  - notification stays owed while the parent is mid-run and flushes at its
    turn boundary; stays owed while `AwaitingInput` and merges into the next
    user turn;
  - depth-4 and concurrency-limit rejections;
  - `PrepareOffload` refuses with an active subagent;
  - recovery: `Running` nodes become `Failed("interrupted by restart")` and
    parents are notified; terminal nodes reload pageable transcripts;
  - exactly-once notification: an owed notification (`notified == false`)
    survives offload + reload and is delivered with the parent's next turn,
    and a delivered one is never re-sent.

## Out of scope (future)

- Synchronous spawn, subagent cancellation/`Stop` cascading, steering a running
  subagent with follow-up messages, per-subagent model overrides, streaming
  subagent activity to the SSE feed.
