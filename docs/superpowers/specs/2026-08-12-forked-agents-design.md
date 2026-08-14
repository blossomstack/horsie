# Forked agents

Branch a conversation without leaving the session it belongs to.

## The problem

A session is one conversation with one agent. When that conversation reaches a
point where two directions are worth trying — or where the context is full of
work that is now settled and the next thing is different — there is nowhere to
go. The options today are both bad: keep going in the same thread and carry
every earlier message into every later prompt, or start a fresh session and lose
the workspace, the checkout, and everything the agent already learned.

Subagents are not the answer. A subagent is delegated work: it owes its parent a
report, it cannot ask the user anything, and it is not a place a person can go
and have a conversation. `SUBAGENT_PROMPT_SUFFIX` says so outright — *"You
cannot ask the user or rename the session; if you are blocked, report that
instead."*

What is missing is a **fork**: a second conversation, inside the same session,
that starts from where the first one had got to and then goes its own way.

## Approach

A fork is a **fourth kind of agent** hosted by the session, beside the main
agent, subagents, and workflow steps. It is not a new session.

That choice is what makes the whole feature small. The session already hosts
several agents and already knows how to give each one the sandbox under its own
identity — `scoped_client` in `session_actor/context.rs` hands every non-main
agent the session's runtime with `with_agent_id`, so it shares the filesystem
and the checkout but gets its own working directory and environment. A fork
needs nothing new there. It also inherits the answer to the question that would
otherwise dominate the design: there is no second runtime to provision, no
second runtime to hibernate, and no refcount to keep straight, because there is
only ever one runtime and the session owns it.

A fork differs from a subagent in exactly the ways a conversation differs from a
delegated task:

| | subagent | fork |
|---|---|---|
| owes a result to | the agent that spawned it | nobody |
| `ask_user` | no | **yes** |
| renames | nothing | **itself** |
| spawns subagents | yes | yes |
| appears in | its parent's transcript | the session list |

The toolbox is already layered by agent kind (`context.rs:733`), so "a fork gets
what the main agent gets" is a match arm, not a mechanism.

## What a fork is made of

### A fourth agent kind

`SessionAgentKind::Fork(Uuid)` and `AgentKey::Fork(Uuid)`, alongside `Main`,
`Sub` and `Step`. Its journal is `agent/<uuid>` like every other agent's; it is
addressed as `?aid=<uuid>` like every other agent; and it reports its terminal
outcome to the session, which uses it only to update the fork's own status.

Toolbox layering takes `Main`'s arms: `AskUserToolbox`, a title layer, and
`SubAgentToolbox`. It roots its own subagent tree — its spawns are that tree's
`SubAgentParent::Main` — exactly as a workflow step does.

The title layer is the one substitution. `set_session_title` called by a fork
renames **the fork**, not the session. Same tool name, same schema: the model
should not have to know what kind of conversation it is in to name it. The
toolbox is already built per agent kind, so the layer is constructed knowing
which fork it serves and sends a fork-targeted command rather than
`CoreCommand::SetTitle` — which keeps `ForkTitled` an event `ForkedAgents`
authors, not one `SessionCore` writes into somebody else's slice.

### A roster, not a tree node

Forks live in `SessionState.forks`, separate from `SubAgentTree`. That tree's
entire vocabulary — `notified`, `TreeOwner`, `owed_deliveries` — exists to make
sure a parent eventually receives a child's result. A fork has no such debt, and
putting it in that structure would mean carrying fields that must always be
inert and could always be read wrong.

```rust
/// The agent a fork was taken from: the session's main agent, or another fork.
pub enum ForkParent { Main, Fork(Uuid) }

/// How the fork's history was seeded.
pub enum ForkMode { Copy, Summary }

pub struct ForkRecord {
    pub parent: ForkParent,
    /// The parent's log seq the fork was taken at — the branch point.
    pub source_seq: u64,
    pub mode: ForkMode,
    pub title: Option<String>,
    pub status: AgentStatus,
    pub created_at_ms: u64,
}
```

`AgentStatus` is reused verbatim. It already carries exactly the states a
conversation moves through — `Provisioning | Running | Idle | AwaitingInput |
Failed | Cancelled` — and documents `Completed` as *"Only a subagent or a step
reaches it: a conversation is never done."* A fork is a conversation, so it
never reaches `Completed` either. No new status type.

Forks nest arbitrarily: a fork taken from a fork records `parent: Fork(id)`, and
the sidebar renders the real lineage. There is no depth cap. A human types
`/fork`, so there is no runaway to guard against, and `MAX_SUBAGENT_DEPTH`
exists to bound a machine that can spawn in a loop.

### A component

`ForkedAgents`, in `session_actor/fork.rs`, with the pure data in
`sessions/forks.rs` — mirroring how `subagent.rs` and `subagents.rs` split.

It earns three of the four `Component` hooks, which is the test for whether
something deserves to be one (`Read` and `Hooks` are `SessionCommand` variants
that are *not* components):

- **`apply`** — folds `ForkCreated` / `ForkSeeded` / `ForkTitled` /
  `ForkStatusChanged` / `ForkDeleted` into `state.forks`. `SessionActor::apply_event`
  matches every variant explicitly and routes each to one component, so adding
  these forces a compile error there, which is where classification belongs.
- **`on_load`** — a fork left `Provisioning` by a dead process has nobody to
  finish it. Unlike a turn, which the *agent* reports as interrupted from its
  own recovery, seeding is session-owned work with no journal of its own. This
  is `RuntimeLifecycle`'s situation, and re-attempting is safe for the identical
  reason it gives: `Provisioning` is precisely the state in which no turn has
  run.
- **`busy`** — a fork mid-seed must keep the session loaded, or the idle sweep
  unloads it out from under an in-flight summariser call.

Not `actions`. A fork becoming runnable needs nothing started at the session's
turn boundary: the seed and the fork's first message land in **one write** on the
fork's own journal, and that write drains the fork exactly as an ordinary
`Enqueue` does. An `actions` arm would be a second thing deciding when a fork may
run, disagreeing with the first.

Message routing stays in `Turns`. `Turns` recognises `/fork` where it already
recognises `/compact`, and hands the work to `ForkedAgents`, which owns
`state.forks` and is the only thing that writes it. `Turns` *reads*
`state.forks` to resolve `?aid=<fork-id>`, the same way it already reads the
subagent forest to find results owed a turn. Reading across is allowed; writing
across is what the `Component` trait exists to prevent.

## The two commands

Both join `BUILTINS` in `crates/support/src/plugin/builtins.rs`, so they are
offered by the `/` typeahead even in a session with no plugins, and an installed
bundle cannot shadow them. Both are resolved in `turns.rs` before the text is
treated as a prompt, and neither ever reaches the model.

```
/fork <message>          — copy this conversation into a new fork, then send <message>
/summary-n-fork <message> — summarise this conversation into a new fork, then send <message>
```

Both **require** a message. A bare `/fork` is rejected: a fork with nothing to
do is a fork nobody will come back to.

Both are rejected in a subagent's or a workflow step's composer, with a reason
saying why. Only a conversation forks — the main agent, or another fork.

`/summary-n-fork` is named for what the fork *receives*. `/compact-n-fork` was
rejected because `/compact` already means "rewrite this conversation in place",
which is the one thing this does not do.

### `/fork` — copy

The source agent's state is copied and scrubbed:

| field | fate | why |
|---|---|---|
| `log`, `next_seq` | carried | the conversation is the point |
| `context_tokens` | carried | the fork's prompt really is that big |
| `task_list` | carried | the working state the copied messages refer to |
| `inbox` | dropped | a message queued on the source is the source's to answer |
| `asks`, `timers`, `parked`, `turn_in_flight` | dropped | defensive; see below |
| `usage_total`, `last_turn_usage` | reset | the source's spend must not be counted twice |

Only `inbox` is load-bearing. Carried, both conversations answer the same
message. The rest are defensive and worth saying so plainly: `asks` survives
being carried because the fork's own queued message is a person speaking, and
that overrides a park by design; `timers`/`parked` are always empty for a
conversation, which never gets the timer tools; `turn_in_flight` would be
cleared by the fork's own first turn before anything read it. They are dropped
because a fork inheriting them is never *right*, not because something breaks
today.

A dangling `tool_use` in the copied log — from a parked ask, or a turn the fork
was taken mid-way through — is repaired by the sanitization every turn start
already runs, so it never reaches a provider.

The result is saved as the fork's initial snapshot, and its own events number on
from there.

**The copy is cut at the branch point**, not at the log's current end. Journaling
the fork writes a `Forked` entry onto the source's own log, and a source that is
mid-turn goes on appending while the seed is being built — so copying to the end
hands the fork its own creation marker plus whatever else landed in that window.
`source_seq` is read when the fork is asked for and is what the cut uses; it is
also on the record, so a re-seed after a crash cuts in the same place.

### `/summary-n-fork` — summarise

The summary is **the source agent's own turn**, queued on its inbox like
`/compact` is.

The first version ran it as a detached task the session owned, with the source
left `Idle` throughout and the client redirected into the fork at once. Both
halves were wrong. The source stayed interactive while a summary of it was being
taken, so a reply sent in that window landed *after* the `Forked` marker in the
source's transcript but *inside* the fork's summary — `SummariseAll` read
`prompt_messages()` whenever the mailbox reached it and cut at nothing, while
`/fork`'s copy had been cutting at `source_seq` since the branch-point fix. And
the fork the user was dropped into was empty for as long as the provider call
took, with nothing saying why.

Queueing it fixes both at once, and reuses machinery that already exists:

- `Incoming::Fork` is a fifth queue item. `Turns` enqueues it on the source in
  the same breath as journaling `ForkCreated`, so "the command was accepted" and
  "the source is busy" are one event. Nothing can slip between them.
- The source drains it as an ordinary turn: `begin_turn` journals `TurnBegan`,
  the session hears `AgentOutcome::Started` and marks it `Running`, and anything
  typed meanwhile queues behind it. The status is truthful because the agent
  genuinely is running a turn — no second way for an agent to be busy, and no
  fourth classifier branch of the kind that once left a fork reading `RUNNING`
  for ever.
- **The branch point is enforced by the queue, not by an argument.** A turn in
  flight cannot append, so there is no `at_seq` to remember to pass.

A turn carries `Option<Summarise>` rather than `/compact`'s old
`Option<Option<String>>`, because there are now two things a summary can be for:

```rust
enum Summarise {
    /// `/compact`: fold it back behind a boundary in this agent's own history.
    Compact(Option<String>),
    /// `/summary-n-fork`: not this conversation's to keep — it seeds `fork`,
    /// and this history is left exactly as it was.
    Fork(Uuid),
}
```

The summary reaches the session on `RunReport`, beside the run's outcome, and is
delivered as a **non-terminal** `AgentOutcome::ForkSummary` — the shape `Started`
and `UsageRecorded` already have, and skipped by both "how did the turn end"
reads for the same reason. Carrying it on the report rather than in the outcome
is what lets `drain` stay total: a turn that also carries a typed message still
summarises first and answers second, exactly as a queued `/compact` does.

The summariser itself is unchanged — `summary_prompt()` and the policy's
`carried_state()` — and still **rewrites nothing**. Folding it back into the
source would make `/summary-n-fork` do two things, only one of which was asked
for.

`Provisioning` still means "nothing may run yet, the message waits in the
queue", and a failure still lands the row in `Failed`, which is retryable. A
re-seed after a crash re-enqueues the same item, so the first attempt and the
retry go down one path rather than two.

`/fork` keeps its instant seed: it copies, makes no provider call, and never
queues anything on the source. The redirect below is shared, which for a copy
resolves on the next frame.

### Where the user waits

On the source, which is visibly `Running`, until the fork is ready.

`SessionAck.forked_agent` still carries the id at once — the record is journaled
up front, so the `Forked` marker appears in the source's transcript immediately
and a crash mid-summary has a `Provisioning` row to re-drive. But the composer no
longer navigates on the ack. It holds the id and navigates when that fork leaves
`Provisioning` on the global feed, so the transcript it opens is already there.

Creating the row up front is the one place this departs from "create the fork
once the summary is done". The cost is a sidebar row that is not ready yet; the
alternative loses the branch marker and loses the whole command on a crash.

**Still open:** clicking that row before it is ready opens the ordinary
empty-transcript state, which renders the hint for the *session's* status rather
than the fork's — so it says nothing useful about a summary in progress. Making
that panel agent-aware is its own change; the redirect gate above means nobody
reaches it by following a fork command.

### The seed message

Both modes end the seeded log with one synthetic `Role::User` message carrying a
`fork:` id — the same device compaction already uses for `compaction:{n}`, so
`prompt_messages` needs no change and the UI special-cases an id prefix it
already knows how to special-case.

In `Copy` mode it carries only the framing. In `Summary` mode it carries the
summary and the carried state as well, in the same layout `boundary_text`
produces, so the model reads exactly what it would read after a compaction.

It also carries the title instruction:

> This conversation was forked from "<source title>". The message that follows
> sets a new direction — call `set_session_title` once it is clear.

One-shot, in the log, never in the system prompt. A system-prompt section would
be re-sent every turn and would go on nagging long after the fork was named.

## What the source sees

A new `LifecycleEvent` variant, written to the log of the agent the fork was
taken from, at the point it was taken:

```
struct ForkLifecycle { id: String, title: Option<String>, mode: String }
```

This has direct precedent. `SubAgentLifecycle` is documented as *"Recorded on
the parent, because the parent is what a viewer is reading when it matters"* —
the same reasoning applies, and the same routing carries it:
`lifecycle_routing::route` maps the `ForkCreated` event to the source agent's
key, and `record_lifecycle` writes it.

Two properties fall out:

- **The model is not told.** `prompt_messages` drops every `Lifecycle` body, so
  the source agent's model never sees the entry. That is deliberate — forks are
  for the user, not for the model — and it means forking does not disturb the
  source's prompt cache.
- **It marks the branch point.** The entry sits between the two messages the
  fork was taken between, so scrolling the source transcript shows where each
  fork left, and each is a link into that fork.

The main agent gets no `subagent_status` equivalent for forks. It is not
supervising them and has nothing to do with the answer.

## The session list

`SessionSupervisorCommand::List` is documented *"Loads nothing: the record is
durable"* — that is the entire reason status is mirrored onto `SessionRecord`.
Fork rows are mirrored the same way, journaled by the supervisor on create,
retitle, status change and delete.

This is not an optimisation. Deriving the sidebar from session state would wake
every session that has ever been forked, every time someone opens the app.

Each row badges its own status: the session row is the main agent, each fork row
is itself. There is no rollup. A derived "something in here is running" status
is a second thing that can disagree with the durable one after a crash, which is
the failure the current design's comments say it was built to avoid.

## Client

Nothing new is needed to *view* a fork. `sessions/:id/agents/:agentId` already
routes to `SessionView` and already renders a composer for a named agent.

- `SessionAck` gains `forked_agent: Option<String>`; the composer navigates to
  `/sessions/<sid>/agents/<forkId>` when it is present. Server-side resolution
  means the CLI gets `/fork` without a second implementation.
- The sidebar renders forks nested under their session, at real depth, from the
  mirrored rows on the list response.
- The `Forked` lifecycle entry renders in the transcript as a branch marker
  linking to the fork.

## Removal

Nothing is ever removed automatically. No sweep, no pruning of quiet forks, no
cascade the user did not ask for.

`DELETE /api/sessions/:id/agents/:forkId` drops the roster row and the fork's
journal when the user asks for it. Deleting a session takes its forks with it,
because that is the user deciding about the session.

## Not doing

- **No new session per fork.** Considered first and rejected: it forced
  `runtime_id` to stop being the session id, an attach-or-create provisioning
  path, and a durable refcount for hibernate and delete — all to reach a place
  where two sessions share one filesystem, which is what a fork inside the
  session gives for free.
- **No cross-fork turn serialisation.** Two agents in one session can already
  run tool calls against one filesystem concurrently; subagents have always been
  able to. A fork is not a new risk and does not get a new lock.
- **No fork of a subagent or a workflow step.** A subagent's conversation is a
  delegated task, and a step's belongs to the run. Only conversations fork.

## Cleanup, deferred

`copy_snapshot` in `crates/server/src/db/journal.rs` has no call site and says
in its own doc comment that it exists so *"the caller forks a session from this
snapshot"*. An actual fork cannot use it — it must scrub state a verbatim copy
carries over, and a summary fork copies nothing at all — so it stays dead.

It is **not** removed here. It is a required method on the `Journal` trait in
the published `horsie-actor` crate, so deleting it means releasing a new actor
version, and a feature should not wait on a dependency release for a tidy-up.
Worth doing next time `horsie-actor` ships a breaking change.

## Testing

- **Pure fold** — `sessions/forks.rs` against a hand-built roster: nesting,
  parent resolution, status transitions, delete.
- **Component** — `ForkedAgents::actions` / `on_load` / `busy` against a
  hand-built `SessionState`, no actor and no journal, as every other component's
  tests are written.
- **Scrub** — the copied state drops asks, timers and an in-flight turn, and
  zeroes usage. Written against a source state that has all of them.
- **Recovery** — a fork left `Provisioning` is re-seeded on load; a fork mid-seed
  keeps the session from unloading.
- **End-to-end** (`session_server_e2e.rs`) — `/fork` in the composer produces a
  fork that answers its message; `/summary-n-fork` produces one whose prompt is
  the summary and whose log does not contain the source's messages; a message to
  `?aid=<fork>` reaches it; the source's transcript holds the branch entry.
  Waiting on *reply text*, never on a status, per the trap this suite documents.
- **Web** — the sidebar nests forks; the composer redirects on `forked_agent`.
