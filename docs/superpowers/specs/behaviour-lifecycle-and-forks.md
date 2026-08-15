# Behaviour: `RuntimeLifecycle` and `ForkedAgents`, exactly as they are today

A forensic record of what these two files do, so a rewrite has something to preserve
against instead of re-deriving it:

- `crates/server/src/sessions/session_actor/lifecycle.rs` — `RuntimeLifecycle`
- `crates/server/src/sessions/session_actor/fork.rs` — `ForkedAgents`

Everything below describes the code as it stands. Nothing here is a proposal, and
nothing here describes `sessions/runners/`.

Two conventions used throughout:

- **"Reply immediately"** means the `ReplyTo` is fired inside the handler body, before
  `handle_command` returns, and therefore *before* the `CommandEffect`'s events are
  written. **"Reply on the persist ack"** means the reply is chained off
  `CommandEffect::and_ack(...)`, which the runtime fires only after a successful
  durable write.
- `file:line` references are against the tree at commit `4b582f61` on branch
  `feat/session-actor-runners`.

## 0. The frame both components sit in

### 0.1 The `CommandEffect` contract

`CommandEffect` comes from `horsie-actor` 0.12
(`~/.cargo/registry/src/*/horsie-actor-0.12.0/src/actor.rs:26-90`). It carries four
things: `events`, `snapshot`, `ack`, `stop`.

`Persistent::handle` (`.../src/persistent.rs:62-125`) runs them in a fixed order:

1. `handle_command` returns the effect.
2. `persist_events` writes and folds. **A failed write folds nothing.**
3. `on_events_persisted(&persisted, &state)` — only on success, only if non-empty.
4. `snapshot`, if asked and the write succeeded and `stop` is false.
5. **`ack.send(result)`** — so an `and_ack` reply is strictly after the fold and after
   `on_events_persisted`.
6. `Flow::Stop` if `stop` or if the write hit `JournalError::Conflict`.

This ordering is the reason `ForkCommand::Create` can hand `FinishCreate` a state that
already contains the `ForkCreated` it just wrote.

### 0.2 What the session does after every persisted batch

`SessionActor::on_events_persisted` (`mod.rs:1152-1156`) does three things, in order:

1. `record_lifecycle(events, state)` (`core.rs:160-184`) — routes each event through
   `sessions/lifecycle_routing.rs::route` and `tell`s an `AgentCommand::RecordLifecycle`
   to each named resident agent. A key with no resident agent logs
   `"no resident agent to record a session event on; it will be missing from the log"`
   and is dropped.
2. `report_forks(state)` (`mod.rs:409-439`) — the whole `ForkRow` roster to the
   supervisor as `ForksChanged`, suppressed if unchanged or if both old and new are empty.
3. `report_status(state)` (`mod.rs:441-453`) — `SessionStatusChanged`, suppressed if
   unchanged.

The routings that matter here (`lifecycle_routing.rs:53-211`):

| Event | Lands on | As |
| --- | --- | --- |
| `ProvisioningStarted` | session-wide (`Main`, or the step in flight) | `Runtime{status: Acquiring, detail: None}` |
| `ProvisioningProgress` | session-wide | `Runtime{status: Acquiring, detail: Some(detail)}` |
| `ProvisioningSucceeded` | session-wide | `Runtime{status: Ready, detail: None}` |
| `ProvisioningFailed` | session-wide | `Runtime{status: Failed, detail: Some(error)}` |
| `SessionFailed` | **every** agent — main/step, every subagent, every fork | `SessionFailed{reason}` |
| `ForkCreated` | the *parent* conversation (`Main` or `Fork(pid)`) | `Forked{id, title: None, mode}` |
| `ForkTurnEnded` | the **fork itself** | `TurnEnded{outcome}` |
| `ForkSeeded`, `ForkTitled`, `ForkStatusChanged`, `ForkDeleted` | nowhere | — |

Note the asymmetry: a run with no step started yet has no log at all, so every
session-wide provisioning entry in that window is dropped
(`lifecycle_routing.rs:41-52`, pinned by `a_run_with_no_step_yet_has_nowhere_to_record`).

### 0.3 `Component` — what the trait offers and what each component takes

`component.rs:51-94` declares four associated functions, all defaulted, all with no
`self`:

- `apply(&mut SessionState, &SessionDomainEvent)` — default no-op. Must be pure.
- `actions(&ActionCx, &SessionState) -> Vec<AgentAction>` — default empty.
- `on_load(&ActionCx, &SessionState) -> Option<SessionCommand>` — default `None`.
- `busy(&SessionState) -> bool` — default `false`.

`ActionCx` (`component.rs:46-49`) is `{ id: Uuid, spec: &SessionSpec }`.

`handle` is deliberately *not* on the trait (`component.rs:31-34`); each component has an
inherent `async fn handle(&mut SessionActor, &SessionState, <its command>, &ActorContext)`.

## 1. `RuntimeLifecycle` (`lifecycle.rs`)

A unit struct (`lifecycle.rs:22`). It owns the session's sandbox and four state fields:
`status`, `last_error`, `provisioned_at_ms`, and (indirectly) nothing else.

### 1.1 `RuntimeLifecycle::ready` — the one gate

`lifecycle.rs:33-38`:

```rust
!matches!(
    state.status,
    SessionStatus::Provisioning | SessionStatus::ProvisioningFailed { .. }
)
```

Two callers:

- `SessionActor::next_actions` (`mod.rs:916-918`) returns an empty action list when
  `!ready`, which is the single gate for every component's `actions`.
- `SessionActor::runnable` (`mod.rs:1073-1076`) is `ready(state) && !Unrecoverable`, and
  is what an agent is handed as `AgentRuntimeContext::ready` at spawn (`mod.rs:561`).

**Surprise 1 — `ready` is `true` for `Unrecoverable`.** `ready` only excludes
`Provisioning` and `ProvisioningFailed`. `runnable` is the one that also excludes
`Unrecoverable`. So `next_actions` — and therefore `flush_then_drain` — *will* run
subagent deliveries and workflow-step starts on a session whose runtime is
terminally gone. Only the per-agent `ready` flag stops turns.

### 1.2 `LifecycleCommand::Provision` (`lifecycle.rs:49-117`)

**Senders.** `supervisor.rs:715` (once, at session creation, immediately after
`RecordSpec`); `turns.rs:495-498` (self-send when a user message arrives at a session in
`ProvisioningFailed`); `lifecycle.rs:188-194` (`on_load`). Carries no reply.

**Guard** (`lifecycle.rs:62-69`): acts only when `status` is one of

```rust
SessionStatus::Idle | SessionStatus::Provisioning | SessionStatus::ProvisioningFailed { .. }
```

Anything else returns `CommandEffect::none()` — **ignored silently**, no reply, no log
line. `Unrecoverable`, `Running` and `AwaitingInput` all fall here.

The code's own comment (`lifecycle.rs:58-61`) flags the weakness: `Idle` is also every
healthy session's resting status, so the guard holds only because the supervisor sends
`Provision` exactly once. Nothing enforces that.

**Captured before the spawn** (`lifecycle.rs:70-80`): `runtimes` handle, `session`
(the id as a string), `vendor` (`spec().vendor`), `spec` (cloned), `me(ctx)`,
and

```rust
let at_ms = now_ms();
let incarnation = at_ms.to_string();
```

The clock is read **once**: the incarnation string handed to the vendor and the `at_ms`
journaled are the same number, by construction.

**`tokio::spawn`** (`lifecycle.rs:85-115`). The detached task:

1. Calls `runtimes.create(&session, &incarnation, &vendor, &spec)`
   (`runtime_manager.rs:324-343`). That resolves the vendor link, builds the
   `RuntimeSpec`, calls `link.create(...)`, and maps the vendor's *first* progress
   report to `Option<String>` narration (`runtime_manager.rs:350-360`: only
   `Starting{detail}` and `Provisioning{detail}` narrate; `Requested`, `Ready`,
   `Stopping`, `Stopped`, `Gone` narrate nothing). It does **not** wait for `Ready`.
2. Splits the result into `(error, terminal, detail)`:
   - `Ok(detail)` → `(None, false, detail)`
   - `Err(RuntimeError::Gone(_))` → `(Some(e.to_string()), true, None)` — the **only**
     terminal case.
   - `Err(RuntimeError::Unavailable(_) | RuntimeError::Provision(_))` →
     `(Some(e.to_string()), false, None)`.

   The `Display` strings come from `runtime_manager.rs:35-48` and are what the user
   sees verbatim:
   - `"runtime vendor unavailable: {0}"`
   - `"runtime is gone: {0}"`
   - `"runtime provisioning failed: {0}"`
3. If `detail` is `Some`, `tell`s
   `SessionCommand::Lifecycle(LifecycleCommand::NarrateProvisioning { detail })` first.
4. Unconditionally `tell`s
   `SessionCommand::Lifecycle(LifecycleCommand::FinishProvisioning { error, terminal })`.

Both sends are `tell`, both `let _ =` — a closed mailbox is swallowed.

**Events** (`lifecycle.rs:116`): `persist(vec![ProvisioningStarted { at_ms }])`. One
event, no ack, no snapshot, no stop.

**Reply:** none — `Provision` has no `ReplyTo`.

**Surprise 2 — the spawn is started *before* the persist, contradicting the module
doc.** `lifecycle.rs:7-9` says:

> "The create is journaled *before* the vendor is called and runs off the mailbox, so an
> interrupted create is discoverable at load…"

and `lifecycle.rs:82-84` repeats it:

> "Off the mailbox: a real create runs for minutes… The status it just journaled is what
> holds the turn back meanwhile."

The `tokio::spawn` is at line 85; the effect returns at line 116 and is only written
after `handle_command` returns. So the vendor's `create` can begin before
`ProvisioningStarted` is durable. What *is* guaranteed is the **event** ordering:
`NarrateProvisioning`/`FinishProvisioning` come back as mailbox commands and cannot be
handled until the current command's persist has completed. The uncovered window is a
process death between the spawn and the write, which leaves a real sandbox with no
journal entry naming it.

### 1.3 `LifecycleCommand::NarrateProvisioning { detail }` (`lifecycle.rs:118-129`)

**Sender:** only the detached create task above.

**Guard** (`lifecycle.rs:122-124`): acts only when `status == SessionStatus::Provisioning`.
Otherwise `CommandEffect::none()` — **ignored silently**. This is the arm that drops a
vendor's word arriving after the outcome.

**Events:** `persist(vec![ProvisioningProgress { at_ms: now_ms(), detail }])`. Note
`now_ms()` is read here, not carried from the vendor.

**Reply:** none.

**No spawn.**

### 1.4 `LifecycleCommand::FinishProvisioning { error, terminal }` (`lifecycle.rs:130-146`)

**Sender:** only the detached create task.

**Guard: none.** This arm acts in every state, including `Idle`, `Running` and
`Unrecoverable`.

**Surprise 3 — the unguarded `FinishProvisioning` is asymmetric with the guarded
`NarrateProvisioning` right above it.** The narration arm exists precisely to stop a
late report from rewriting a settled session, but the outcome arm — which actually
moves `status` — has no such check. A second `Provision` whose first create is still
outstanding therefore produces two `FinishProvisioning`s, both journaled, last one wins.

**Events** (`lifecycle.rs:131-145`), in this order:

1. Exactly one of:
   - `error == None` → `ProvisioningSucceeded { at_ms: now_ms() }`
   - `error == Some(error)` → `ProvisioningFailed { at_ms: now_ms(), error, terminal }`
2. Then `events.extend(actor.flush_then_drain(&next, ctx).await)`, where `next` is a
   **local fold** of the state one step early:
   `SessionActor::apply_event(state.clone(), event.clone())` (`lifecycle.rs:139`).

`flush_then_drain` (`mod.rs:931-946`) asks `next_actions` for work, performs each action
against the running fold, and returns whatever events those produce
(`SubAgentNotified`, `StepStarted`, `RunFinished`, `RunFailed`, …).

The whole list is returned as one `CommandEffect::persist`, so the outcome event and the
drain's events land in one durable batch.

**Reply:** none.

**Surprise 4 — "A failure drains nothing" is true only for a *retryable* failure.**
`lifecycle.rs:141-143` says:

> "The runtime landed, so whatever queued behind it starts now. A failure drains
> nothing: the messages stay owed…"

For `terminal: false` the fold gives `ProvisioningFailed`, `ready()` is false, and
`next_actions` returns empty — the comment holds. For `terminal: true` the fold gives
`Unrecoverable` (see 1.8), `ready()` is **true** (Surprise 1), and `flush_then_drain`
runs the full action list. `Turns::actions` is empty (`turns.rs:512-514`), but
`SubAgents::actions` will deliver owed results and `WorkflowRun::actions` will try to
start a step, on a session whose runtime is gone for good.

### 1.5 `LifecycleCommand::PrepareOffload { reply: ReplyTo<bool> }` (`lifecycle.rs:147-167`)

**Senders:** `supervisor.rs:585` (the idle sweep, via `ask`) and `supervisor.rs:1094`
(shutdown, via `ask` — the answer is discarded and the session forgotten either way).

**Guard** (`lifecycle.rs:152-155`): `actor.busy(state)`. `SessionActor::busy`
(`mod.rs:900-905`) is:

```rust
RuntimeLifecycle::busy(state)      // status == Provisioning
    || Turns::busy(state)          // status == Running            (turns.rs:535-537)
    || WorkflowRun::busy(state)    // run.current().is_some()      (run.rs:387-389)
    || SubAgents::busy(state)      // subagents.has_active()       (subagent.rs:304-306)
```

On a refusal: `reply.send(false)` **immediately**, then `CommandEffect::none()`. Nothing
is touched — no cancel, no hibernate, no stop. The supervisor treats `Ok(false)` as
"restart the idle clock" (`supervisor.rs:592-595`).

**On acceptance**, in order (`lifecycle.rs:156-166`):

1. `actor.stop_agents().await` (`mod.rs:1079-1089`) — takes `self.agents`, and for every
   resident agent `tell`s `AgentCommand::Cancel { ack: None }` then
   `AgentCommand::Shutdown`. Cancel first, deliberately.
2. `actor.deps().runtimes.hibernate(&id.to_string(), &spec().vendor).await`
   (`runtime_manager.rs:609-615`) — forgets the cached client, then best-effort
   `link.hibernate(...)` on a throwaway progress channel.
3. `reply.send(true)` — **immediately**, as the actor's last act, before returning.
4. `CommandEffect::stop()`.

**Events: none.** Offloading journals nothing at all, so `on_events_persisted` never
fires and neither `report_status` nor `report_forks` runs on the way out.

**No spawn.** Note the offload does *not* call `cancel_in_flight`; it relies entirely on
`busy` having said no.

### 1.6 `LifecycleCommand::Delete { reply: ReplyTo<()> }` (`lifecycle.rs:168-178`)

**Sender:** `supervisor.rs:846`, via `tell` with a oneshot the supervisor then awaits.

**Guard: none.** Delete acts in every state, `busy` or not.

In order:

1. `actor.cancel_in_flight(state).await` (`mod.rs:782-792`) — cancels
   `AgentKey::Step(agent)` for the run's current step, else `AgentKey::Main`.
   `cancel_agent` (`mod.rs:757-777`) first tells the *cached* runtime client
   `cancel_in_flight()`, then sends `AgentCommand::Cancel { ack: Some(..) }` and waits
   up to `CANCEL_TIMEOUT` = 5 s (`mod.rs:84`), logging
   `"cancelled run did not finish within {CANCEL_TIMEOUT:?}; proceeding"` on timeout.
2. `actor.stop_agents().await`.
3. `runtimes.delete(&id.to_string(), &vendor).await` (`runtime_manager.rs:618-624`).
4. `reply.send(())` — immediately.
5. `CommandEffect::stop()`.

**Events: none.** A deleted session journals nothing about its own deletion; the
supervisor's own log is the record. Note also that `cancel_in_flight` never reaches
forks — only `Main` or the current step — so a fork mid-turn is only stopped by
`stop_agents`'s blind `Cancel`+`Shutdown`.

### 1.7 `Component` for `RuntimeLifecycle`

**`actions`** — not implemented, so the default empty `Vec` (`component.rs:73-75`).
`RuntimeLifecycle` never asks for anything to be started; it only gates
(`mod.rs:916-918`).

**`on_load`** (`lifecycle.rs:188-194`):

```rust
matches!(state.status, Provisioning | ProvisioningFailed { .. })
    .then_some(SessionCommand::Lifecycle(LifecycleCommand::Provision))
```

Wired at `mod.rs:359`, inside `adopt`, which collects the `on_load` of
`RuntimeLifecycle`, `SubAgents`, `WorkflowRun` and `Turns` and `tell`s each to itself
(`mod.rs:354-370`). `adopt` runs from `on_recovery_complete` (`mod.rs:1205-1208`) when
the log has a spec, and from `CoreCommand::RecordSpec` otherwise.

**`busy`** (`lifecycle.rs:196-198`): `status == Provisioning`. So a create in flight
blocks an offload; a *failed* create does not.

**`apply`** (`lifecycle.rs:211-246`), routed from `mod.rs:1108-1113` for exactly four
variants:

| Event | Effect on state |
| --- | --- |
| `ProvisioningStarted { at_ms }` | `status = Provisioning`; `provisioned_at_ms = Some(at_ms)`. **`last_error` is not cleared.** |
| `ProvisioningProgress { .. }` | nothing at all |
| `ProvisioningSucceeded { .. }` | `status = Idle`; `last_error = None` |
| `ProvisioningFailed { error, terminal, .. }` | `status = Unrecoverable { reason: error.clone() }` if `terminal`, else `ProvisioningFailed { reason: error.clone() }`; `last_error = Some(error)` in both cases |

The arm ends with `other => unreachable!("RuntimeLifecycle was handed {other:?}")` under
`#[allow(clippy::wildcard_enum_match_arm)]` (`lifecycle.rs:210, 244`). The comment
(`lifecycle.rs:206-209`) explains it is unreachable by construction because
`SessionActor::apply_event` matches every variant explicitly.

`provisioned_at_ms` is read in exactly one place: `spawn_agent` (`mod.rs:507-518`) turns
it into the `incarnation` string the runtime provider addresses, defaulting to `""` when
`None` — which the acquisition then fails on rather than silently addressing another
sandbox.

### 1.8 The `SessionStatus` values this component produces

- `Provisioning` — from `ProvisioningStarted`. Blocks `ready`, blocks `busy`-offload.
- `Idle` — from `ProvisioningSucceeded`. Also the `SessionState::default()`.
- `ProvisioningFailed { reason }` — retryable. Blocks `ready`. Distinct from the `Failed`
  a failed *turn* leaves: this one has no runtime at all.
- `Unrecoverable { reason }` — terminal. `ready` is still true (Surprise 1); `runnable`
  is false.

`SessionFailed` also produces `Unrecoverable`, but it is `Turns`'s event
(`mod.rs:1114-1120`), not this component's — `a_gone_runtime_is_terminal`
(`lifecycle.rs:809`) lives in this file but exercises `Turns::apply`.

## 2. `ForkedAgents` (`fork.rs`)

A unit struct (`fork.rs:32`). It owns `state.forks`, a `ForkRoster`
(`sessions/forks.rs:78-210`).

A fork is neither a subagent nor a session (`fork.rs:1-8`): it owes nobody a result, it
has `ask_user`, it names itself, and it shares the one runtime the session owns under its
own agent id.

### 2.1 `ForkCommand::Create { parent, mode, message, reply: ReplyTo<Result<Uuid, String>> }` (`fork.rs:42-82`)

**Sender:** `turns.rs:368-388` only, from a detached task, via `ask`. `Turns` has already
established (`turns.rs:325-363`) that the text parsed as `/fork` (→ `ForkMode::Copy`) or
`/summary-n-fork` (→ `ForkMode::Summary`), that the message is non-empty, that
`state.run.is_none()`, and that the addressed agent is `Main` or `Fork(_)`.

**First act — the log-head read** (`fork.rs:50`):
`actor.source_log_head(state, ctx, parent).await`.

`source_log_head` (`fork.rs:419-430`) resolves `fork_source(parent)` and then
`agent.ask(|reply| AgentCommand::LogHead { reply }).await.ok()`. The agent answers
`state.next_seq` (`agent_actor.rs:2572-2575`).

`fork_source` (`fork.rs:406-416`):
- `ForkParent::Main` → `self.agent()` (`mod.rs:698-703`) — `None` on a workflow session.
- `ForkParent::Fork(id)` → `spawn_fork_actor(ctx, state, id)`.

**Guard failure:** if `source_log_head` is `None`,
`reply.send(Err("the conversation to fork is not available".to_string()))` — **replied
immediately** — and `CommandEffect::none()`. `Turns` wraps it as
`UserMessageError::Rejected` (`turns.rs:384`).

**Surprise 5 — `source_log_head` blocks the session mailbox on an `ask`.** The whole
point of `Turns::start_fork` running off-mailbox (`turns.rs:364-366`) was that "`Create`
reads the source agent's log head and then waits on its own write". `Create` itself still
performs a synchronous round-trip to the source agent from inside the session's
`handle_command`. It is fast in practice because the agent runs turns on a detached task,
but it is a mailbox-held await.

**On success:** `let id = Uuid::new_v4();` (`fork.rs:55`) and one event
(`fork.rs:56-63`):

```rust
SessionDomainEvent::ForkCreated {
    at_ms: now_ms(), id, parent, source_seq, mode, message: message.clone(),
}
```

**`tokio::spawn`** (`fork.rs:65-80`): a oneshot `(tx, rx)` is made; the task awaits `rx`
and maps a closed channel to
`Err(JournalError::Backend("fork ack channel closed".to_string()))`; then it `tell`s
`SessionCommand::Fork(ForkCommand::FinishCreate { id, reply, persisted })` back to the
session. **The `reply` oneshot is moved into the detached task** — `Create` itself never
answers on success.

**Effect** (`fork.rs:81`):

```rust
CommandEffect::persist(vec![created]).and_ack(ReplyTo::from_sender(tx))
```

So: persist-then-spawn. The write lands, `on_events_persisted` routes a `Forked` entry
into the *parent's* transcript and reports the new roster to the supervisor, and only
then does the ack fire and `FinishCreate` get queued. The state `FinishCreate` sees
already contains the fork.

### 2.2 `ForkCommand::FinishCreate { id, reply, persisted }` (`fork.rs:83-106`)

**Sender:** the detached task above, only.

**Guard 1** (`fork.rs:88-91`): `if let Err(e) = persisted` →
`reply.send(Err(format!("persist fork: {e}")))`, **immediately**, then
`CommandEffect::none()`. Nothing is spawned, nothing is seeded.

**Guard 2** (`fork.rs:92-95`): `actor.spawn_fork_actor(ctx, state, id).is_none()` →
`reply.send(Err("could not start the fork".to_string()))`, immediately, then
`CommandEffect::none()`. The `ForkCreated` is already durable at this point, so the fork
survives in the roster as `Provisioning` with no actor.

**On success:**

1. `actor.start_seeding(ctx, state, id);` (`fork.rs:100`)
2. `reply.send(Ok(id));` (`fork.rs:104`) — **immediately**, and deliberately: the client
   redirects to a fork that is visibly building itself.
3. `CommandEffect::none()` — **no events**.

The message is *not* enqueued here (`fork.rs:96-99`): it rides into the same write as the
seed, because a fork with a message and no history would drain it and answer a
conversation it has not been given.

**Surprise 6 — `FinishCreate` replies `Ok(id)` even when seeding never started.**
`start_seeding` (`fork.rs:444-458`) returns silently after a
`tracing::warn!(fork = %id, "no record to seed a fork from")` if `state.forks.get(id)` is
`None`. The reply is on the next line regardless. The caller is told the fork exists; it
sits at `Provisioning` for ever.

### 2.3 `ForkCommand::Summarised { forks, result }` (`fork.rs:107-129`)

**Sender:** `mod.rs:1007-1015` — `on_agent_outcome` intercepts
`AgentOutcome::ForkSummary` before any turn-end routing, because the summarising turn may
still be running (`types.rs:678-683`, `types.rs:693-698`). It never arrives as a
`SessionCommand::Fork`.

`forks` is a **list** because every fork queued into one drain shares one provider call
(`inbox.rs:270-292`).

**Per-id guard** (`fork.rs:111-114`): `if !state.forks.contains(id) { continue; }` —
**silently dropped**, documented as "the user having changed their mind".

**Per-id action:**
- `Ok(summary)` → `actor.finish_seeding(ctx, state, id, summary.clone())`
  (`fork.rs:489-497` → `seed_fork_with(.., Some(summary))`).
- `Err(error)` → `actor.me(ctx).tell(SessionCommand::Fork(ForkCommand::SeedFailed { id, error: error.clone() }))`
  — a self-send, one mailbox hop later, not a direct persist.

**Events: none.** **Reply: none.** `CommandEffect::none()`.

### 2.4 `ForkCommand::Seeded { id }` (`fork.rs:130-147`)

**Sender:** the detached seeding task in `seed_fork_with` (`fork.rs:549`).

**Guard** (`fork.rs:131-133`): `!state.forks.contains(id)` → `CommandEffect::none()`,
**silently**.

**Events:** via `persist_and_advance` (`mod.rs:956-968`), not a bare persist:

```rust
vec![SessionDomainEvent::ForkSeeded { at_ms: now_ms(), id }]
```

plus whatever `flush_then_drain` produces against the locally folded next state. All in
one batch.

**Reply:** none. **No spawn.**

**Surprise 7 — the stated reason for `persist_and_advance` here is wrong.**
`fork.rs:134-137` says:

> "Through `persist_and_advance` rather than a bare persist: the fork becoming ready is
> what releases the message queued behind it, and that release is an action."

Nothing in `next_actions` releases a fork's message. The fork's first message is released
by the **agent itself**: `AgentCommand::SeedFrom` persists `Seeded` + `Received` and
`tell`s itself `AgentCommand::Drain` in the same handler (`agent_actor.rs:2580-2620`).
`forks.rs:113-128` states this explicitly and is why `apply_seeded` only moves
`Provisioning → Idle` — the fork is usually already `Running` by the time `ForkSeeded` is
journaled. What `persist_and_advance` actually buys here is a subagent-delivery flush at
a turn boundary.

### 2.5 `ForkCommand::SeedFailed { id, error }` (`fork.rs:148-158`)

**Senders:** the detached seeding task (`fork.rs:550`) and the `Summarised` error branch
(`fork.rs:118-124`).

**Guard** (`fork.rs:149-151`): `!state.forks.contains(id)` → `CommandEffect::none()`,
silently.

**Side effect:** `tracing::warn!(fork = %id, error, "seeding a fork failed")`.

**Events:** a **bare** persist (not `persist_and_advance`):

```rust
vec![SessionDomainEvent::ForkStatusChanged { at_ms: now_ms(), id, status: AgentStatus::Failed }]
```

**Reply:** none. The `error` string is journaled nowhere — only the `Failed` status
survives, and `ForkStatusChanged` routes to no transcript (`lifecycle_routing.rs:197-200`).
The reason exists only in the log line.

Because the fork leaves `Provisioning`, `has_seeding()` goes false and the fork is never
a re-seed candidate again.

### 2.6 `ForkCommand::SetTitle { id, title, reply: ReplyTo<Result<String, String>>, .. }` (`fork.rs:159-180`)

**Sender:** `title_tool.rs:159` — a fork's own `set_session_title` tool call.

**Note the `..`**: the `agent: AgentId` field declared on the command
(`types.rs:262-268`) is destructured away and never read. Routing is entirely by `id`.

**Guard 1 — normalization** (`fork.rs:162-169`):
`crate::sessions::title_tool::normalize_session_title(&title)` (`title_tool.rs:44-58`).
On `Err(e)`, `reply.send(Err(e.to_string()))` **immediately** and `CommandEffect::none()`.
The three strings (`title_tool.rs:28-39`) are:

- `"session title must not be empty"` — empty after `trim()`
- `"session title must be a single line"` — contains `\n` or `\r`
- `"session title must be at most 60 characters"` — over `SESSION_TITLE_MAX_CHARS = 60`
  Unicode chars (`title_tool.rs:19`)

**Guard 2 — existence** (`fork.rs:170-173`): `!state.forks.contains(id)` →
`reply.send(Err(format!("no such fork: {id}")))` immediately, `CommandEffect::none()`.

Guard order matters: a malformed title on a nonexistent fork reports the title error.

**Reply then persist** (`fork.rs:174-179`): `reply.send(Ok(normalized.clone()))` fires
**before** the write, then

```rust
persist(vec![SessionDomainEvent::ForkTitled { at_ms: now_ms(), id, name: normalized }])
```

So the tool is told the rename succeeded before it is durable.

### 2.7 `ForkCommand::Delete { id, reply: ReplyTo<Result<(), String>> }` (`fork.rs:181-192`)

**Sender:** `supervisor.rs:792`, routed from `SessionSupervisorCommand::DeleteFork`.

**Guard** (`fork.rs:182-185`): `!state.forks.contains(id)` →
`reply.send(Err(format!("no such fork: {id}")))` immediately, `CommandEffect::none()`.
Same string as `SetTitle`'s.

**On success**, in order:

1. `actor.retire_fork_actor(id).await` (`fork.rs:571-576`) —
   `self.agents.remove_sub(id)` (`mod.rs:164-169`, the only caller of `remove_sub`) and
   `agent.actor.stop().await`. Best-effort: a non-resident fork returns early.
2. `reply.send(Ok(()))` — **immediately, before the write**.
3. `persist(vec![SessionDomainEvent::ForkDeleted { at_ms: now_ms(), id }])`.

**Surprise 8 — the reply precedes the durable delete.** If the `ForkDeleted` write fails,
the supervisor has already been told `Ok(())` while the fork is still in the journal. On
reload it comes back — in whatever status it last held — with its actor gone. Contrast
`ForkCommand::Create`, which goes to real trouble (`and_ack` + a detached task) to answer
only after the write.

Deleting a parent fork orphans nothing: children keep their own records
(`forks.rs:353-373`).

### 2.8 `ForkCommand::ReseedInterrupted` (`fork.rs:193-204`)

**Sender: nobody.** See Surprise 9.

**Body:** for each `id` in `state.forks.seeding()` (`forks.rs:194-200` — every fork whose
status is `Provisioning`):

- `spawn_fork_actor(ctx, state, id)`; if `None`,
  `tracing::warn!(fork = %id, "could not restart a fork to re-seed it")` and `continue`.
- else `actor.start_seeding(ctx, state, id)`.

**Events: none. Reply: none.** `CommandEffect::none()`.

Re-seeding is idempotent by construction: `seed_fork_with` reads the branch point,
parent and message off the durable `ForkRecord` (`fork.rs:519-527`), the summariser item
id is `format!("fork-summarise:{id}")` (`fork.rs:480`), the queued message id is
`format!("fork-message:{id}")` (`fork.rs:545`), and `SeedFrom` returns `Ok(())` without
rewriting when the fork's log is already non-empty (`agent_actor.rs:2586-2593`).

### 2.9 `SessionActor::on_fork_outcome` (`fork.rs:284-358`)

Not a `ForkCommand` arm, but the other half of this component's behaviour. Reached from
`on_agent_outcome` (`mod.rs:1030-1032`) — **before** the subagent forest is consulted,
because a fork is not in that forest.

It answers all five `TurnEnd` variants (`types.rs:631-653`):

| `TurnEnd` | Journals | Via |
| --- | --- | --- |
| `Concluded { output }` | `ForkTurnEnded { outcome: TurnOutcome::Ended(EmptyOutcome{}) }` — **`output` is discarded** | `persist_and_advance` |
| `Asked` | `ForkStatusChanged { status: AwaitingInput }` — an early return, *not* a turn boundary; the question is journaled into the fork's own log by the agent | `persist_and_advance` |
| `Failed { error, terminal: true }` | `SessionFailed { reason: error }` — **session-wide**, not fork-scoped: forks share the one runtime | `persist_and_advance` |
| `Failed { error, terminal: false }` | `ForkTurnEnded { outcome: TurnOutcome::Failed(FailedOutcome { error }) }` | `persist_and_advance` |
| `Parked` | `ForkTurnEnded { outcome: Failed(FailedOutcome { error: "agent parked; timers are not supported in sessions" }) }` — the string is built here, verbatim (`fork.rs:331-333`) | `persist_and_advance` |
| `Interrupted` | guarded: only if `state.forks.get(id).status == Running`, else `CommandEffect::none()` silently; then `ForkTurnEnded { outcome: Interrupted(EmptyOutcome{}) }` | `persist_and_advance` |

No replies anywhere; no spawns. The mirror of `on_agent_started` (`mod.rs:1060-1066`),
which journals `ForkStatusChanged { status: Running }` when a fork's turn begins.

### 2.10 `Component` for `ForkedAgents`

**`actions`** — not implemented; default empty (`component.rs:73-75`). Forks are never
started by the turn boundary; only `SubAgents`, `Turns` and `WorkflowRun` are asked
(`mod.rs:923-928`).

**`on_load`** (`fork.rs:217-222`):

```rust
state.forks.has_seeding()
    .then_some(SessionCommand::Fork(ForkCommand::ReseedInterrupted))
```

**`busy`** (`fork.rs:227-229`): `state.forks.has_seeding()`.

**Surprise 9 — neither `ForkedAgents::on_load` nor `ForkedAgents::busy` has a production
caller.** Verified by grep over the whole tree: outside `fork.rs` the only references to
`ForkedAgents` are `mod.rs:45` (the `use`), `mod.rs:1008` (`Summarised`), `mod.rs:1137`
(`apply`) and `mod.rs:1172` (`handle`). Concretely:

- `adopt` (`mod.rs:358-366`) collects exactly four `on_load`s —
  `RuntimeLifecycle`, `SubAgents`, `WorkflowRun`, `Turns`. `ForkedAgents` is not in the
  list, so `ReseedInterrupted` is **never sent by anything**, and a fork abandoned
  mid-seed stays `Provisioning` across every reload.
- `SessionActor::busy` (`mod.rs:900-905`) ORs exactly four `busy`s. `ForkedAgents::busy`
  is not among them, so a session **can** be offloaded out from under an in-flight
  summariser call.

Both are directly contradicted by their own doc comments and by prose elsewhere:

`fork.rs:10-13`:
> "a crash between the two replays as a fork still `Provisioning`, which
> [`ForkedAgents::on_load`] re-seeds — strictly better than an untracked agent."

`fork.rs:210-216`:
> "A fork left `Provisioning` by a dead process. Nothing else can finish one: seeding is
> session-owned work with no journal of its own…"

`fork.rs:224-226`:
> "A summariser call is provider time with nothing durable behind it. Unloading the
> session mid-seed loses it and leaves a fork that only a reload repairs."

`types.rs:555-558`:
> "a crash between the two replays as a fork still `Provisioning`, which
> `ForkedAgents::on_load` re-seeds."

`forks.rs:188-192` and `forks.rs:202-204` repeat both claims.

The two unit tests that appear to cover this (`fork.rs:672`, `fork.rs:683`) call the
associated functions **directly**, so they pass while the wiring is absent. `git log -S`
shows the strings `ForkedAgents::busy` / `ForkedAgents::on_load(` have only ever appeared
inside `fork.rs` itself — this was never wired and later removed; it was never wired.

**`apply`** (`fork.rs:236-269`), routed from `mod.rs:1132-1137` for six variants. Every
arm delegates to `ForkRoster`:

| Event | Roster call | Effect (`forks.rs`) |
| --- | --- | --- |
| `ForkCreated { id, parent, source_seq, mode, message, at_ms }` | `apply_created` | inserts a `ForkRecord` with `title: None`, `status: Provisioning`, `created_at_ms = last_activity_ms = at_ms` (`forks.rs:85-111`) |
| `ForkSeeded { id, .. }` | `apply_seeded` | `Provisioning → Idle` **only**; any other status is left alone, and `last_activity_ms` is not touched (`forks.rs:120-128`) |
| `ForkTitled { id, name, .. }` | `apply_titled` | `title = Some(name)` (`forks.rs:130-134`) |
| `ForkStatusChanged { at_ms, id, status }` | `apply_status` | sets `status` and `last_activity_ms = at_ms` (`forks.rs:138-143`) |
| `ForkTurnEnded { at_ms, id, outcome }` | `apply_status` with a **derived** status | `Failed(_) → AgentStatus::Failed`; `Ended`/`Stopped`/`Interrupted` → `AgentStatus::Idle` (`fork.rs:257-265`) |
| `ForkDeleted { id, .. }` | `apply_deleted` | removes the entry (`forks.rs:145-147`) |

Every roster mutator is a no-op on a missing id, so events for a deleted fork cannot
resurrect it (`forks.rs:378-393`).

The status is derived from the outcome rather than carried beside it, deliberately
(`fork.rs:253-256`): "a second field saying so is a second thing that can disagree with
the first."

Same `unreachable!("ForkedAgents was handed {other:?}")` fallthrough (`fork.rs:267`)
under `#[allow(clippy::wildcard_enum_match_arm)]`.

## 3. Fork seeding, end to end

### 3.1 The branch point

`Branch.source_seq` is `AgentState::next_seq` of the source agent, read by
`AgentCommand::LogHead` (`agent_actor.rs:2572-2575`) **before anything is written**
(`fork.rs:48-54`). It is stamped onto `ForkCreated` and durably held on the `ForkRecord`
(`forks.rs:51-52`), which is what makes a first attempt and a re-seed cut at the same
place.

Reading it first is load-bearing: `on_events_persisted` writes a `Forked` lifecycle entry
into the source's own log (`lifecycle_routing.rs:181-193`). A copy taken at the log's
*end* would hand the fork a marker pointing at itself — asserted by
`a_fork_does_not_take_over_a_message_queued_on_the_source` (`fork.rs:1358-1361`).

### 3.2 Dispatch by mode

`start_seeding` (`fork.rs:444-458`) reads `rec.mode` off the record:

- `ForkMode::Copy` → `copy_into_fork` → `seed_fork_with(.., summary = None)`.
- `ForkMode::Summary` → `ask_source_to_summarise`.

`ask_source_to_summarise` (`fork.rs:464-486`) resolves `fork_source(parent)` — warning
`"no conversation to summarise for a fork"` and returning silently if there is none —
then `tokio::spawn`s a task that does exactly one thing:

```rust
source.tell(AgentCommand::Enqueue {
    item: Incoming::Fork { id: format!("fork-summarise:{id}"), fork: id },
    ack: None,
}).await
```

**`ack: None`.** Nothing observes whether the enqueue was written.

The summary is therefore **the source's own turn**, not a detached read of it
(`fork.rs:432-443`). The agent's drain treats a `Fork` item as a `Summarise::Fork`
(`inbox.rs:279-293`) that wins over any queued `/compact`, contributes no text to the
turn (`inbox.rs:304-322`), and is taken **before** the turn says anything to the model
(`agent_actor.rs:3303-3323`). If nothing else is in the drain, `summarise_only`
short-circuits the turn with an empty `Completed` (`agent_actor.rs:3326-3334`). The
result travels back as `AgentOutcome::ForkSummary { agent, forks, result }`
(`agent_actor.rs:1716-1720`), which `on_agent_outcome` routes to
`ForkCommand::Summarised`.

Doing it out of band was the original design and the reason for the change
(`fork.rs:437-443`): the source stayed `Idle` and answering, so a reply sent in that
window landed *after* the branch marker and *inside* the summary.

### 3.3 The handover

`seed_fork_with` (`fork.rs:512-554`) is the single path for both modes.

Preconditions, both of which return silently after a warning:

- `state.forks.get(id).cloned()` is `None` → `"no record to seed a fork from"`.
- either `fork_source(parent)` or the resident fork actor is missing →
  `"no agents to seed a fork between"`.

It then computes `source_title` (`fork.rs:559-565`): the session's `spec().name` for
`ForkParent::Main`, the parent fork's `title` for `ForkParent::Fork(id)`, falling back to
the literal `"the conversation before this one"`.

Then `tokio::spawn` (`fork.rs:540-553`): builds

```rust
Incoming::User { id: format!("fork-message:{id}"), text: message }
```

and calls `seed_fork(&source, &fork, summary, source_seq, &source_title, queued)`,
mapping the result to `ForkCommand::Seeded { id }` or
`ForkCommand::SeedFailed { id, error }` and `tell`ing it back to the session.

`seed_fork` (`fork.rs:585-627`):

1. **Copy** (`summary == None`): `source.ask(AgentCommand::ForkSeed { at_seq: source_seq })`
   → `state.scrub_for_fork(at_seq)` (`agent_actor.rs:763-783`), which keeps only
   `log` entries with `seq < at_seq`, sets `next_seq = at_seq`, keeps `context_tokens`
   and `task_list`, and **empties** `inbox`, `asks`, `timers`, resets `nudges`, `parked`,
   `turn_in_flight`, `usage_total` and `last_turn_usage`. Errors become
   `format!("read the conversation to fork: {e}")`.
   **Summary** (`summary == Some(s)`): the state is `Box::new(AgentState::default())` —
   the history is not copied at all.
2. Builds one synthetic `Message` (`fork.rs:610-618`):
   `id: format!("fork:{}", Uuid::new_v4())`, `role: Role::User`, one `TextPart` whose
   text is `fork_seed_text(source_title, &summary)`, `created_at_ms: now_ms()`,
   `started_at_ms: None`. The `fork:` prefix reuses the device compaction already uses
   for `compaction:{n}` (`fork.rs:581-584`).
3. `fork.ask(AgentCommand::SeedFrom { state, seed, message, reply })`. Transport errors
   become `format!("seed the fork: {e}")`; the inner `Result<(), String>` is returned as
   is.

`fork_seed_text` (`fork.rs:634-644`) produces, verbatim:

```
This conversation was forked from "{source_title}". The message that follows sets a new direction — call set_session_title once it is clear.
```

and, only when `summary` is non-empty, appends:

```


# Summary of the conversation this was forked from

{summary}
```

The title instruction rides in the seed rather than the system prompt on purpose
(`fork.rs:629-633`): a prompt section is re-sent every turn and would nag long after the
fork was named.

`AgentCommand::SeedFrom` on the agent side (`agent_actor.rs:2580-2620`):

- If `!state.log.is_empty()`, replies `Ok(())`, tells itself `Drain`, and persists
  nothing — the honest answer for a re-seed after a crash.
- Otherwise persists **two** events in one batch, `Seeded { state, seed }` then
  `Received { item: message, at_ms }`, with `and_ack` (reply errors:
  `format!("persist the fork's history: {e}")`, or
  `"the fork's history was never written"` on a dropped channel) and `and_snapshot()`.
- Tells itself `Drain` *before* returning the effect.

The message rides in the same write for two reasons learned the hard way
(`agent_actor.rs:201-204`): enqueued first, the fork answers before it has a history;
enqueued after, a crash in between leaves a seeded fork with nothing to do.

### 3.4 What "seeding failed" means

Any of: the fork actor could not be resolved (silent, no event); `ForkSeed` failed;
`SeedFrom` failed to reach the fork; the fork's write failed; or the summariser's
provider call returned `Err`.

The first is silent — the fork sits at `Provisioning` for ever. The rest converge on
`ForkCommand::SeedFailed`, which journals `ForkStatusChanged { status: Failed }` and
nothing else (2.5). There is no automatic retry: `Failed` is not `Provisioning`, so
`seeding()` no longer returns it — and `ReseedInterrupted` has no sender anyway
(Surprise 9).

### 3.5 How a fork becomes runnable

Three separate things, in this order, and only the first two matter:

1. **The agent's own drain.** `SeedFrom` writes history + message and self-`Drain`s
   (`agent_actor.rs:2606`). This is what actually starts the fork's first turn.
2. **The session-level `ready` flag.** The fork's `AgentActor` was constructed with
   `ready: Self::runnable(state)` at spawn time (`mod.rs:561`) — computed against the
   state the spawn was decided on, never remembered; later changes reach it as `Runtime`
   records.
3. **`ForkSeeded`.** Journaled after the fact, by which point the fork is typically
   already `Running`. `apply_seeded` only moves `Provisioning → Idle`
   (`forks.rs:113-128`), precisely so a working fork is not moved backwards. This is why
   `ForkRoster::is_seeded` is `status != Provisioning` rather than `status == Idle`
   (`forks.rs:159-170`).

## 4. Provisioning, end to end

### 4.1 The path

```
supervisor Create            turns.rs (user message at ProvisioningFailed)      on_load
        │                              │                                          │
        └──────────────► LifecycleCommand::Provision ◄─────────────────────────────┘
                                       │
             guard: Idle | Provisioning | ProvisioningFailed   (else silently ignored)
                                       │
        at_ms = now_ms(); incarnation = at_ms.to_string()
                                       │
              tokio::spawn ────────────┴──────────► persist [ProvisioningStarted{at_ms}]
                    │
        RuntimeManager::create(session, incarnation, vendor, spec)
                    │
        ┌───────────┴───────────┐
   Ok(Some(detail))        Err(RuntimeError)
        │                       │
 tell NarrateProvisioning   Gone → terminal=true;  Unavailable|Provision → terminal=false
        │                       │
        └────────► tell FinishProvisioning { error, terminal }
                            │
        Succeeded → status=Idle, last_error=None    Failed → Unrecoverable | ProvisioningFailed
                            │
                 + flush_then_drain(next) in the same batch
```

### 4.2 The narration path

The vendor's own words, unedited, all the way through:

1. `link.create(...)` returns its **first** `RuntimeProgress`
   (`runtime_manager.rs:338-342`). The substrate finishes on a sink nothing waits on.
2. `RuntimeManager::narration` (`runtime_manager.rs:350-360`) keeps `Starting{detail}`
   and `Provisioning{detail}`; `Requested`, `Ready`, `Stopping`, `Stopped`, `Gone` all
   map to `None`.
3. `Some(detail)` → one `NarrateProvisioning` `tell`, sent **before** the
   `FinishProvisioning`.
4. The `NarrateProvisioning` arm guards on `status == Provisioning` and journals
   `ProvisioningProgress { at_ms: now_ms(), detail }`.
5. `on_events_persisted` routes it into the session-wide log as
   `Runtime{status: Acquiring, detail: Some(detail)}` — the status is deliberately
   unchanged, because narration describes the wait rather than ending it
   (`lifecycle_routing.rs:61-70`).

There is exactly **one** narration per create, by construction: the detached task looks
at one `Option<String>`. A vendor with nothing to say produces no `ProvisioningProgress`
at all.

### 4.3 The progress stages a reader sees

| Journal event | `SessionStatus` | Transcript entry |
| --- | --- | --- |
| `ProvisioningStarted` | `Provisioning` | `Runtime{Acquiring, detail: None}` |
| `ProvisioningProgress` (0 or 1) | unchanged | `Runtime{Acquiring, detail: Some(..)}` |
| `ProvisioningSucceeded` | `Idle` | `Runtime{Ready, detail: None}` |
| `ProvisioningFailed` | `Unrecoverable` or `ProvisioningFailed` | `Runtime{Failed, detail: Some(error)}` |

### 4.4 Terminal vs retryable

The split is made once, in the detached task (`lifecycle.rs:92-99`), and is exactly the
split `RuntimeManager::get` makes:

- **Terminal** — `RuntimeError::Gone`, "a live vendor says this session's runtime cannot
  be produced" (`runtime_manager.rs:41-44`). Folds to
  `SessionStatus::Unrecoverable { reason }`. `runnable` is false; `UserMessageError::Unrecoverable`
  is returned to any later message (`turns.rs:406-409`).
- **Retryable** — `RuntimeError::Unavailable` (vendor not registered or its socket is
  dead) and `RuntimeError::Provision` (a create that could not provision). Folds to
  `SessionStatus::ProvisioningFailed { reason }`. `ready` is false, so no turn starts;
  but a user message re-sends `Provision` (`turns.rs:494-499`), and `on_load` re-sends it
  on every load — which is the only retry a workflow run can get, since a run takes no
  messages.

### 4.5 `provisioned_at_ms`

It is *the identity of the current provision*: `at_ms` of the `ProvisioningStarted` that
began it, and the `incarnation` string the vendor was given for that same create.

- Set only by `RuntimeLifecycle::apply` on `ProvisioningStarted` (`lifecycle.rs:213-220`).
- Never cleared. A second provision overwrites it, deliberately: the leftover sandbox
  answers to a name nothing publishes to any more (`lifecycle.rs:284-292`).
- Read only by `spawn_agent` (`mod.rs:505-521`), where it becomes the
  `RuntimeClientProvider`'s incarnation. `None` becomes `""`, which the acquisition then
  fails on rather than silently addressing another sandbox.
- Documented as a durability contract on the state struct (`types.rs:742-754`): derived
  from the entry that began the provision rather than journaled separately, because
  recording it twice would be two sources for one answer.

The same `spawn_agent` call also passes `matches!(state.status, Provisioning)` as the
provider's `provisioning` flag — the journal is the only thing that can tell "the
substrate has not reported the object yet" from "there is nothing there", and that is the
difference between waiting for a runtime and declaring it gone.

## 5. Test inventory

### 5.1 `lifecycle.rs` — 17 tests (`lifecycle.rs:249-816`)

Module doc: "Getting and releasing the sandbox: what a create does, what an interrupted
one replays as, and what refuses an offload."

| Line | Test | What it pins |
| --- | --- | --- |
| 274 | `a_provision_is_named_by_the_entry_that_began_it` | `ProvisioningStarted{at_ms: 1234}` folds to `provisioned_at_ms == Some(1234)`. |
| 286 | `provisioning_again_gives_the_session_a_new_name` | Two `ProvisioningStarted` → the second `at_ms` wins. |
| 298 | `a_created_session_provisions_before_it_is_idle` | `Started` → `Provisioning`; `Started`+`Succeeded` → `Idle`. |
| 314 | `a_session_is_not_runnable_until_its_runtime_lands` | `ready` is false under `Provisioning` and true after `Succeeded`. |
| 335 | `a_retryable_create_failure_is_reported_verbatim` | **Exact string.** Asserts `status == ProvisioningFailed { reason: "runtime vendor unavailable: vendor 'local' is not connected" }` and `last_error.is_some()`. Pins that the fold copies the error verbatim into `reason`, and that this is *not* the `Failed` a failed turn leaves. |
| 361 | `a_failed_create_starts_no_turn` | `ready` is false for `ProvisioningFailed` — "the whole defect in #239". |
| 379 | `a_terminal_create_failure_ends_the_session` | `terminal: true` folds to `Unrecoverable`. |
| 399 | `a_message_arriving_mid_create_waits_for_the_runtime` (async) | Over `actor_fixture_blocking_creates`. A `UserMessage` sent during a held create queues; **exact-prefix assertion** that no signal `starts_with("get:")`. After `release_creates()`, a `TurnBegan` appears — asserted on the journal, not the status, because a fast turn ends before a poll. |
| 455 | `a_create_interrupted_by_a_restart_is_re_attempted_at_load` (async) | A journal seeded with only `ProvisioningStarted` re-attempts at load. **Exact string:** `signals()` contains `format!("create:{id}")`. |
| 490 | `a_message_after_a_failed_create_provisions_instead_of_dying` (async) | #239. Vendor removed → `ProvisioningFailed`; **substring assertion** `last_error.contains("unavailable")`; vendor restored; a user message drives a real create (`format!("create:{id}")`), a `TurnBegan` follows, and the status never reaches `Unrecoverable`. |
| 575 | `loading_a_session_whose_create_failed_re_attempts_it` (async) | `RuntimeLifecycle::on_load` fires for a seeded `ProvisioningFailed` journal; **exact string** `format!("create:{id}")`. The only retry a workflow run can get. |
| 612 | `what_the_vendor_says_about_a_create_is_journaled` (async) | Over `BootingVendor`. **Exact string:** the collected `ProvisioningProgress` details equal `vec![BOOTING_CREATE]` = `"the machine is booting"` (`testing.rs:1566`). Also pins ordering: the `ProvisioningProgress` index is `<` the `ProvisioningSucceeded` index. |
| 659 | `a_create_with_nothing_to_say_records_nothing` (async) | The default fake vendor narrates nothing → no `ProvisioningProgress` at all. |
| 685 | `narration_that_outlives_the_create_is_ignored` (async) | A `NarrateProvisioning` sent after `ProvisioningSucceeded` journals nothing. Round-tripped through a `ReadCommand::Snapshot` `ask` so the assertion is not racing the command. |
| 726 | `prepare_offload_refuses_while_a_run_is_in_flight` (async) | `BlockingProvider` holds a turn → `PrepareOffload` answers `false`; **exact-prefix assertion** that no signal `starts_with("hibernate:")`; and the actor still answers a `ReadCommand::UsageStats` afterwards, proving a refusal tears nothing down. |
| 787 | `prepare_offload_refuses_with_an_active_subagent` (async) | `SubAgents::busy` also blocks an offload; same `hibernate:` prefix assertion. |
| 809 | `a_gone_runtime_is_terminal` | `SessionFailed{reason}` folds to `Unrecoverable`. (Exercises `Turns::apply`, not this component's.) |

Gaps worth naming: nothing covers `LifecycleCommand::Delete`, the `Provision` guard's
"ignored silently" branch, `FinishProvisioning`'s missing guard, or the terminal-failure
drain (Surprise 4).

### 5.2 `fork.rs` — 18 tests (`fork.rs:646-1379`)

Three pure unit tests, one pure string test, and fourteen integration tests over real
actors. Helpers: `id(n)` (`fork.rs:652`), `state_with_fork(status)` (`fork.rs:656`),
`fork_via` (`fork.rs:762`), `transcript` (`fork.rs:778`), `turn_boundaries`
(`fork.rs:795`), `wait_for_turn_end` (`fork.rs:809`, exact turn count — a floor would
pass on a copy fork whose own turn never ends), `wait_for_any_turn_end` (`fork.rs:834`),
`main_turns_begun` (`fork.rs:1143`).

| Line | Test | What it pins |
| --- | --- | --- |
| 672 | `a_fork_mid_seed_keeps_the_session_loaded` | `ForkedAgents::busy` is true for a `Provisioning` fork, false for `Idle`, false for an empty roster. **Calls the function directly — the wiring it names does not exist (Surprise 9).** |
| 683 | `a_fork_left_mid_seed_is_reseeded_at_load` | `ForkedAgents::on_load` returns `Some(Fork(ReseedInterrupted))` for a `Provisioning` fork and `None` otherwise. **Same caveat: nothing calls `on_load`.** |
| 700 | `the_fold_tracks_a_fork_through_its_life` | `ForkedAgents::apply` over `ForkCreated` (→ `Provisioning`), `ForkSeeded` (→ `Idle`), `ForkTitled` (→ `Some("Other migration")`), `ForkDeleted` (→ gone). |
| 857 | `a_forks_turn_ends_in_its_own_log` (async) | A copy fork's log holds **exactly two** turns, both closed: the source's carried-over turn plus the fork's own answer, ending `TurnOutcome::Ended`. Without a `ForkTurnEnded` the page reads `RUNNING` for ever. |
| 891 | `a_forks_turn_moves_the_forks_status_and_not_the_sessions` (async) | After the fork settles to `Idle` with `last_activity_ms > 0`, `state.status == SessionStatus::Idle` — a fork working is not the session working. |
| 922 | `a_forks_failed_turn_says_so_in_its_own_log` (async) | With `FailOnNeedleProvider{needle: "the doomed branch"}`, the fork's second turn ends `TurnOutcome::Failed`. **Substring assertion** `failed.error.contains("bad key")` — the provider returns `LlmError::ApiError{status: 401, message: "bad key"}` (`testing.rs:1487-1490`). |
| 959 | `stopping_a_fork_cancels_that_forks_turn` (async) | `TurnCommand::Stop{agent_id: fork}` ends the fork's turn as `TurnOutcome::Stopped` while the session's own status stays `Running` — the source is deliberately held mid-turn so the copy carries an *open* turn and any end in that log is the fork's own. |
| 1013 | `stopping_an_unknown_agent_is_refused_but_an_idle_one_is_not` (async) | A random uuid is `Err`; `"not-even-a-uuid"` is `Err`; `MAIN_AGENT_ID` with nothing in flight is `Ok`. |
| 1046 | `a_fork_carries_the_conversation_and_answers_its_own_message` (async) | **Three substring assertions** on the fork's transcript: `"the original question"` (the copied history), `"forked from"` (the seed frame), `"try the other migration"` (its own message). |
| 1084 | `a_summary_fork_does_not_carry_the_source_messages` (async) | **Substring assertions:** the transcript does *not* contain `"a very long conversation about migrations"` but does contain `"forked from"`. |
| 1117 | `summarising_for_a_fork_is_a_turn_on_the_conversation_it_branches` (async) | The source's own `turns_begun` count strictly increases across a `/summary-n-fork`. The whole point of the redesign: out of band, the source stayed `Idle` and answering. |
| 1150 | `the_source_transcript_records_where_a_fork_left` (async) | The source's transcript eventually contains the fork's id — the `ForkCreated → Forked` routing. |
| 1172 | `only_a_conversation_can_be_forked` (async) | Forking a subagent is `UserMessageError::Rejected`. **Substring assertion** `m.contains("only a conversation")` against `turns.rs:359`'s `"only a conversation can be forked"`. |
| 1189 | `a_fork_needs_a_message` (async) | A bare `/fork` is rejected. **Substring assertion** `m.contains("needs a message")` against `turns.rs:339-341`'s `"/{name} needs a message saying what the new conversation should do"`. |
| 1204 | `a_fork_of_a_fork_records_the_fork_it_came_from` (async) | The second fork's `parent == ForkParent::Fork(first_id)`. |
| 1242 | `a_fork_of_a_parked_conversation_runs_rather_than_inheriting_the_question` (async) | A scripted `MockProvider` parks the source on `ask_user`; the fork still runs to an answer, and the question is **still readable** in its copied history (`t.contains("which migration?")`). The doc comment is explicit that this does *not* prove `asks` must be dropped — the fork's own queued message overrides a park anyway. |
| 1321 | `a_fork_does_not_take_over_a_message_queued_on_the_source` (async) | **Two negative substring assertions:** the fork's transcript contains neither `"Received"` nor `"QUEUED-FOR-THE-SOURCE\", "` (the source's queued message is not the fork's to answer), and does not contain `"Forked("` (a fork must not carry its own creation marker). The genuinely load-bearing drop. |
| 1369 | `the_seed_frames_the_source_and_carries_a_summary_only_when_there_is_one` | **Exact substrings** on `fork_seed_text`: a copy contains `forked from "Migrate the journal"` and `set_session_title` but not `# Summary`; a summary contains `# Summary` and `We chose sqlx::Any.`. |

Gaps worth naming: nothing covers `ForkCommand::Delete`, `ForkCommand::SetTitle` (either
guard or the reply-before-persist), `SeedFailed`, `FinishCreate`'s two failure branches,
`Create`'s `"the conversation to fork is not available"` refusal, or
`ReseedInterrupted` end to end.

## 6. The surprises, collected

For a reader skimming: nine places where the code and the prose beside it disagree, or
where behaviour is not what the surrounding design implies.

1. **`ready()` is true for `Unrecoverable`** (`lifecycle.rs:33-38` vs `mod.rs:1073-1076`).
   Only `runnable` excludes it, so `next_actions` still runs on a dead session.
2. **`Provision` spawns the vendor call before the persist** (`lifecycle.rs:85` vs the
   module doc at `lifecycle.rs:7-9`). Event ordering is safe; a crash in the window
   orphans a real sandbox.
3. **`FinishProvisioning` has no guard**, while `NarrateProvisioning` directly above it
   does (`lifecycle.rs:118-146`).
4. **"A failure drains nothing"** (`lifecycle.rs:141-143`) holds only for a retryable
   failure; a terminal one folds to `Unrecoverable`, which `ready()` permits.
5. **`ForkCommand::Create` holds the session mailbox** across an `ask` to the source
   agent (`fork.rs:50`), despite `Turns::start_fork` going off-mailbox specifically to
   avoid that (`turns.rs:364-366`).
6. **`FinishCreate` replies `Ok(id)` even when `start_seeding` did nothing**
   (`fork.rs:100-104`).
7. **`ForkSeeded` does not release the fork's message** (`fork.rs:134-137`); the agent's
   own `Drain` inside `SeedFrom` does (`agent_actor.rs:2606`), which is why
   `apply_seeded` must not overwrite a fork that is already `Running`
   (`forks.rs:113-128`).
8. **`ForkCommand::Delete` replies before its write** (`fork.rs:187-191`), unlike
   `Create`, which goes to real trouble to reply only after.
9. **`ForkedAgents::on_load` and `ForkedAgents::busy` have no callers at all**
   (`mod.rs:358-366`, `mod.rs:900-905`), contradicting `fork.rs:10-13`, `fork.rs:210-216`,
   `fork.rs:224-226`, `types.rs:555-558`, `forks.rs:188-192` and `forks.rs:202-204`. A
   fork abandoned mid-seed is never re-seeded, and a session can be offloaded mid-summary.
   Both unit tests call the associated functions directly and so pass regardless.
