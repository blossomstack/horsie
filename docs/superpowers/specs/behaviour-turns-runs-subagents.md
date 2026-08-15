# Behaviour of `Turns`, `WorkflowRun` and `SubAgents`, as of today

A forensic description of three session-actor components:

- `crates/server/src/sessions/session_actor/turns.rs` — `Turns`
- `crates/server/src/sessions/session_actor/run.rs` — `WorkflowRun`
- `crates/server/src/sessions/session_actor/subagent.rs` — `SubAgents`

It exists so a rewrite has something to preserve behaviour *against*, rather than re-deriving it from the same three files every time. It describes only what is there now. It says nothing about how any of it should be ported.

Every `file:line` is against the worktree at `docs/` commit time. Error strings are quoted verbatim because tests assert them character for character; each such string is flagged in the test inventory at the end.

---

## 0. Machinery the three arms are written in

You cannot read the handler arms without these five facts.

**`CommandEffect<SessionDomainEvent>`** (`horsie-actor` 0.12, `src/actor.rs:26`) is the return of every handler. It carries `events` (persisted and folded as *one* `journal.persist` call — `persistent.rs:179`, so a batch is atomic and either all of it lands or none), an optional `ack: ReplyTo<Result<(), JournalError>>` fired *after* the durable write, and `snapshot`/`stop` flags nothing here uses. `CommandEffect::none()` persists nothing. On a failed write the events are neither folded nor counted, and the `ack` carries the `JournalError`.

**`persist_and_advance`** (`mod.rs:956-968`) is the turn boundary. It folds the events locally *one step early* onto a clone of `state`, runs `flush_then_drain` against that folded state, and returns `CommandEffect::persist(events ++ drained)`. `flush_then_drain` (`mod.rs:931-946`) asks every component for `actions()` and performs each in order, re-folding after each so the next action sees the previous one's effect. `next_actions` (`mod.rs:913-929`) returns empty outright unless `RuntimeLifecycle::ready(state)` — i.e. unless the status is neither `Provisioning` nor `ProvisioningFailed` (`lifecycle.rs:33-38`). Order is `SubAgents::actions`, then `Turns::actions`, then `WorkflowRun::actions`.

The difference between `CommandEffect::persist(events)` and `persist_and_advance(state, events, ctx)` is therefore load-bearing everywhere below: only the latter flushes owed subagent deliveries and only the latter can start a step.

**`TurnEnd`** (`types.rs:631-653`) is `AgentOutcome` minus `UsageRecorded`, `Started` and `ForkSummary`, which `on_agent_outcome` (`mod.rs:978-1034`) answers before routing. The five real variants are `Concluded { output }`, `Asked`, `Failed { error, terminal }`, `Parked`, `Interrupted`. Routing is by *identity*, not variant: on a run, `run.index_of_agent(who)` decides step-vs-subagent; off a run, `who == self.id` is the main agent, then forks, then subagents.

**`resolve_agent` (`mod.rs:628-670`) spawns; `stop_target` (`turns.rs:122-144`) does not.** Both resolve `None`/`"main"` to the step in flight on a run. `resolve_agent` will spawn a cold step, fork or subagent actor on demand; `stop_target` is pure resolution from state, deliberately, because waking a cold agent in order to cancel it achieves nothing (`turns.rs:88-94`).

**`AgentCommand::Enqueue { item, ack }`** (`agent_loop/agent_actor.rs:79-82`, handled at `:2373-2386`) self-sends a `Drain`, then persists an `AgentDomainEvent::Received` with the `ack` attached. So an `ack` given to `Enqueue` fires when the *agent's* journal write is durable — that is the write a `202` promises.

---

## 1. `Turns` — every handler arm

`Turns::handle` (`turns.rs:55-81`) dispatches three `TurnCommand` variants.

### 1.1 `TurnCommand::UserMessage { agent_id, text, reply }` → `on_user_message` (`turns.rs:398-505`)

Fully described in §4. Summary of its shape: five refusal gates, then either a fork hand-off or an enqueue; the reply is *never* resolved on this mailbox on the success path.

### 1.2 `TurnCommand::Stop { agent_id, reply }` → `on_stop` (`turns.rs:95-115`)

Reply type `Result<(), String>`.

Guards, in order:

1. `stop_target(state, &agent_id)` is `None` → reply `Err(format!("no such agent: {agent_id}"))`, `CommandEffect::none()`. Nothing journaled. (`turns.rs:102-105`)
2. `stop_boundary(state, key)` is `None` — the agent is resolvable but not working → reply **`Ok(())`**, `CommandEffect::none()`. Not-working is explicitly not a failure: a client that pressed Stop as the turn ended on its own has got what it asked for. (`turns.rs:106-111`)

Otherwise: `self.cancel_agent(key).await` (`mod.rs:757-777` — cancels the cached runtime client's in-flight call first, then `AgentCommand::Cancel` with an ack, waiting up to `CANCEL_TIMEOUT` = 5s, `mod.rs:84`), **then** reply `Ok(())`, **then** `persist_and_advance(state, vec![stopped], ctx)`.

**Reply ordering: the `Ok(())` is sent before persistence and after the cancel completes.** A caller that gets `Ok` knows the run has stopped but does *not* know the boundary event is durable.

`stop_target` (`turns.rs:122-144`) resolution order:

- `"main"` → the run's `current_agent()` as `AgentKey::Step`, else `AgentKey::Main`.
- Otherwise parse as a `Uuid` (unparseable → `None` → "no such agent").
- Then: `state.run.index_of_agent(id)` → `Step`; then `state.forks.contains(id)` → `Fork`; then `state.subagents.node(id)` → `Sub`. Steps and forks before the forest, and forks before subagents, because the roster cannot say what *kind* of agent an id names — forks and subagents share one map.

`stop_boundary` (`turns.rs:158-195`) — the gate and the event to journal, answered together so they cannot disagree. `at_ms = now_ms()` for all four:

| key | gate | event |
|---|---|---|
| `Main` | `state.status == SessionStatus::Running` | `TurnStopped { at_ms }` |
| `Step(id)` | `run.index_of_agent(id)` resolves **and** `run.current() == Some(index)` | `StepCancelled { at_ms, index }` |
| `Fork(id)` | `state.forks.get(id)?.status == AgentStatus::Running` | `ForkTurnEnded { at_ms, id, outcome: TurnOutcome::Stopped(EmptyOutcome {}) }` |
| `Sub(id)` | `state.subagents.node(id)?.status == SubAgentStatus::Running` | `SubAgentFailed { at_ms, id, error: STOPPED_ERROR }` |

`STOPPED_ERROR` is `"stopped before it finished"` (`subagents.rs:25`). A stopped child is reported to its parent as a *failure* deliberately: the parent is blocked on a `spawn_agent` result, so stopping the child quietly would leave it waiting for one that can never come — the same shape recovery delivers.

The gate is `Running` and **not** also `AwaitingInput`, for every kind except a step. Cancelling does not clear the questions an agent is parked on, so a boundary journaled over a park would read `Idle` beside questions still pending. A step escapes because `StepCancelled` suspends the execution outright.

No `tokio::spawn`.

### 1.3 `TurnCommand::Answer { agent_id, answers, reply }` → `on_answer` (`turns.rs:204-224`)

Reply type `Result<(), AnswerError>` (`agent_loop/inbox.rs:162-170`).

1. `resolve_agent(state, ctx, agent_id.as_deref())` is `None` → reply `Err(AnswerError::NothingPending)`, `CommandEffect::none()`. Note the variant: an *unknown agent* is reported as "nothing pending", not as a not-found.
2. Otherwise the `reply` oneshot is **moved into** `AgentCommand::Answer { answers, reply }` and told to the agent. The agent owns the questions, so only it can tell a complete answer set from a partial one; a half-answered park would leave a `tool_use` with no result on the wire.

If the `tell` fails, the code logs `tracing::warn!(session = %self.id, "answers could not reach the agent")` and returns `CommandEffect::none()`. **The reply oneshot was consumed by the failed `tell` and is never resolved** — the caller's `ask` observes a dropped channel, not an `AnswerError`. (`turns.rs:216-223`)

Always `CommandEffect::none()`; `Turns` journals nothing for an answer. The agent replies directly, after its own validation (`agent_actor.rs:2388-2405`).

No `tokio::spawn`.

### 1.4 `on_main_outcome` (`turns.rs:236-293`) — not a `TurnCommand`, routed by identity

Reached from `on_agent_outcome` when `who == self.id` and the session is not a run. No reply. `at_ms = now_ms()` throughout.

| `TurnEnd` | events | notes |
|---|---|---|
| `Concluded { .. }` | `[TurnEnded { at_ms }]` | the output is discarded here |
| `Asked` | `[AskRecorded { at_ms }]` | |
| `Interrupted` **and** `state.status == Running` | `[TurnInterrupted { at_ms }]` | |
| `Interrupted` otherwise | **`return CommandEffect::none()`** — no persist, no drain | |
| `Failed { error, terminal: true }` | `[SessionFailed { at_ms, reason: error }]` | terminal ⇒ the session can never run again |
| `Failed { error, terminal: false }` | `[TurnFailed { at_ms, error }]` | |
| `Parked` | `[TurnFailed { at_ms, error: "agent parked; timers are not supported in sessions" }]` | |

All non-`Interrupted` paths end in `persist_and_advance` (`turns.rs:292`), so a main-agent turn ending is a flush point for owed subagent deliveries. No turn is started here: whether another follows is the agent's own decision against its own queue.

The `Interrupted` guard is the interesting one. The agent reports what its *own* journal left open. A turn that failed before the loop began — abandoned by a start hook, or a context that would not build — never banked a boundary in the agent journal, so the agent still calls it open while the session, told directly, has already recorded `TurnFailed`. The session owns the merged status, so the session decides; a report about anything but a live turn is history already written.

Only `terminal: true` is treated as unrecoverable, and the comment gives the reason: re-provisioning would silently rebuild a workspace the user believes they still have.

### 1.5 `fork_request` (`turns.rs:300-315`) and `start_fork` (`turns.rs:325-390`)

`fork_request(text)` is pure. It runs `horsie_support::plugin::commands::parse_invocation(text, '/')` (`crates/support/src/plugin/commands.rs:109-123` — strips the sigil, takes the name up to the first whitespace, requires it to be non-empty and alphanumeric/`-`/`_`, and trims the rest as args), then `builtins::builtin(name)` (`builtins.rs:52-55`, table at `:30-49`: `compact`, `fork`, `summary-n-fork`). It maps `"fork"` → `ForkMode::Copy` and `"summary-n-fork"` → `ForkMode::Summary`; anything else → `None`. Returns a `ForkRequest { mode, name, message: args.trim().to_string() }` (`turns.rs:37-42`); `name` exists only so the refusal can quote what the user typed.

`start_fork` is not `async` and returns before doing any work. Guards, in order:

1. `message.is_empty()` → reply `Err(UserMessageError::Rejected(format!("/{name} needs a message saying what the new conversation should do")))`. Note `{name}` interpolates `"fork"` or `"summary-n-fork"`.
2. `state.run.is_some()` → reply `Err(UserMessageError::Rejected("a workflow run cannot be forked".to_string()))`.
3. `key` is `AgentKey::Sub(_)` or `AgentKey::Step(_)` → reply `Err(UserMessageError::Rejected("only a conversation can be forked".to_string()))`. `Main` → `ForkParent::Main`, `Fork(id)` → `ForkParent::Fork(id)`.

On success it **`tokio::spawn`s** a detached task (`turns.rs:368-388`) holding `self.me(ctx)` (a `SessionRef` back into this mailbox). The task does:

```
self_ref.ask(|r| SessionCommand::Fork(ForkCommand::Create { parent, mode, message, reply: r }))
```

and maps the outcome onto the *original* caller's oneshot:

- `Ok(Ok(id))` → `Ok(MessageAccepted { message_id: id.to_string(), forked_agent: Some(id.to_string()) })` — the message id **is** the fork id
- `Ok(Err(why))` → `Err(UserMessageError::Rejected(why))`
- `Err(e)` (mailbox error) → `Err(UserMessageError::Rejected(format!("fork: {e}")))`

It is off-mailbox because `Create` reads the source agent's log head and then waits on its own write; holding this mailbox across both would stall every other agent in the session.

`start_fork` itself returns `CommandEffect::none()` in every case. `Turns` journals nothing for a fork.

### 1.6 `impl Component for Turns` (`turns.rs:508-586`)

- **`actions`** (`:512-514`) — `Vec::new()`, always. A conversation's turns are the agent's own decision, taken against the queue it holds; the session has neither the message nor the gate.
- **`on_load`** (`:528-530`) — `None`, always. A turn the process died inside is reported by the agent whose turn it was, and arrives as an ordinary `AgentOutcome::Interrupted`. The comment at `:519-527` records why the previous self-send was removed: a reconcile self-send queues behind everything the supervisor sent while the actor was loading, so a message or flushed subagent result handled first could start a *real* turn, and the reconcile then recorded that one as interrupted.
- **`busy`** (`:535-537`) — `matches!(state.status, SessionStatus::Running)`.
- **`apply`** (`:551-585`) — folds seven events:

| event | effect |
|---|---|
| `TurnBegan` | `status = Running`; **`last_error = None`** |
| `AskRecorded` | `status = AwaitingInput`; **and if `state.run` is `Some`, `run.apply_awaiting()`** |
| `TurnEnded` \| `TurnStopped` \| `TurnInterrupted` | `status = Idle` |
| `TurnFailed { error }` | `status = Failed { reason: error }`; `last_error = Some(error)` |
| `SessionFailed { reason }` | `status = Unrecoverable { reason }`; `last_error = Some(reason)` |
| anything else | `unreachable!("Turns was handed {other:?}")` |

The `unreachable!` is safe by construction: `SessionActor::apply_event` (`mod.rs:1106-1143`) matches every variant explicitly and routes each to exactly one component, so a newly added event fails to compile *there*.

---

## 2. `WorkflowRun` — every handler arm

`WorkflowRun::handle` (`run.rs:35-60`) dispatches four `RunCommand` variants.

### 2.1 `RunCommand::State { reply }` (`run.rs:42-45`)

No guard. Reply `state.run.clone()` (`Option<WorkflowRunState>`) **immediately**, `CommandEffect::none()`. A conversation replies `None`. Journals nothing. No spawn.

### 2.2 `RunCommand::Advance` (`run.rs:46`)

No reply, no guard. `CommandEffect::persist(actor.flush_then_drain(state, ctx).await)`.

This is the one command whose entire body is the drain. Because `flush_then_drain` goes through `next_actions`, a session that is `Provisioning` or `ProvisioningFailed` produces zero events and the run does not start — which is what makes a run's first step wait for its create.

`Advance` is sent by `WorkflowRun::on_load` and, in principle, after a retry (the doc comment at `types.rs:132-134` says so, though `on_retry_step` starts the step itself rather than self-sending `Advance`).

### 2.3 `RunCommand::RetryStep { index, reply }` → `on_retry_step` (`run.rs:148-210`)

Reply type `Result<(), String>`.

Guards, in order:

1. `state.run` is `None` → reply `Err("this session is not a workflow run".into())`, `CommandEffect::none()`.
2. `run.get(index)` is `None` → reply `Err(format!("no step execution at index {index}"))`, `CommandEffect::none()`.

There are **no other guards**. A `Finished`, `Failed` or `Suspended` run is retryable, and so is an index naming a step that failed, was cancelled, or is itself currently running. `RuntimeLifecycle::ready` is not consulted, so a retry starts a step even on a session whose provision failed — unlike every path that goes through `next_actions`.

Body, in order:

1. If `run.current()` is `Some(current)`: `cancel_agent(AgentKey::Step(step.agent))` for `run.get(current)` (skipped if that lookup fails), then push `StepCancelled { at_ms: now_ms(), index: current }` — the push happens whether or not the agent lookup succeeded. Cancelling first is what makes the retry the only writer on the shared workspace.
2. Fold those events onto a local `next = state.clone()`.
3. `new_index = next.run.steps.len() as u32` (default `0`) — `StepCancelled` appends nothing, so this is the current log length.
4. `attempt = next.run.attempts_of(&target.step) + 1` (default `1`).
5. **Reply `Ok(())`** (`run.rs:188`) — before `start_step`, before persistence.
6. `start_step(StepStart { index: new_index, step: target.step, agent: WorkflowRunSpec::step_agent_id(self.id, new_index), attempt, from: target.from, via: target.via, input: target.input }, &next, ctx)` and extend `events`.
7. `CommandEffect::persist(events)` — **not** `persist_and_advance`, so no delivery flush.

`from`/`via`/`input` are copied from the target entry, so the retry sits on the same graph edge as the original rather than inventing a new one. The workspace is **not** rolled back: a retried step re-runs against whatever the previous attempt left on disk.

`journal order` for a retry over a live step: `[StepCancelled{current}, StepStarted{new_index}]`, or `[StepStarted{new_index}]` if nothing was in flight. One atomic write.

No `tokio::spawn`.

### 2.4 `RunCommand::ReconcileInterrupted` (`run.rs:50-58`)

No reply. Guard: `state.run.and_then(WorkflowRunState::current)` is `None` → `CommandEffect::none()`, silently. Otherwise `CommandEffect::persist(vec![StepCancelled { at_ms: now_ms(), index }])` — again *not* `persist_and_advance`, so the reconciled run does not then advance. That is intended: `apply_cancelled` sets the run `Suspended`, and only a retry moves a suspended run.

### 2.5 `start_step` (`run.rs:68-121`) — called from `perform` and from `on_retry_step`

Takes a `StepStart` (`orchestrator.rs:41-52`), returns `Vec<SessionDomainEvent>`. Three outcomes, all `at_ms = now_ms()`:

1. `spawn_step_agent(ctx, state, agent, &step)` is `None` → `[RunFailed { at_ms, error: format!("step '{step}' is no longer in this workflow") }]`. The step name is handed in rather than looked up, because the event being built is what will put it in the log.
2. `actor.tell(AgentCommand::Enqueue { item: Incoming::User { id: format!("step:{index}:{attempt}"), text: input.clone() }, ack: None })` fails → `[RunFailed { at_ms, error: format!("step '{step}' could not be started") }]`. The agent has already been spawned and registered at this point; nothing un-registers it.
3. Otherwise `[StepStarted { at_ms, index, step, agent, attempt, from, via, input }]`.

The input goes through the queue rather than being run directly, so a step that is asked something and answered later resumes down the same path.

### 2.6 `finish_run` (`run.rs:124-129`) and `fail_run` (`run.rs:132-137`)

`finish_run(output)` → `[RunFinished { at_ms: now_ms(), output }]`. `fail_run(error)` → `[RunFailed { at_ms: now_ms(), error }]`. Both are `async fn(&mut self, ..)` that use neither the `await` nor the `&mut self`; they are pure event constructors reached from `perform` (`mod.rs:799-811`).

### 2.7 `on_step_outcome` (`run.rs:215-270`) — routed by identity

Reached from `on_agent_outcome` when `state.run` is `Some` and `run.index_of_agent(who)` resolves. No reply.

| `TurnEnd` | events | advance? |
|---|---|---|
| `Concluded { output }` | `[StepConcluded { at_ms, index, output }]` | **yes** — `persist_and_advance` |
| `Asked` | `[AskRecorded { at_ms }]` | no — plain `persist` |
| `Failed { error, .. }` | `[StepFailed { at_ms, index, error }]` | no — plain `persist` |
| `Parked` | `[]` (empty) | no — plain `persist` of nothing |
| `Interrupted` | **`return CommandEffect::none()`** | — |

`terminal` on `Failed` is discarded: a step that fails fails the run either way, because the shared workspace holds whatever the failed attempt left behind and re-running blind would redo half-finished work.

`Parked` produces *nothing at all* — the step stays `Running` and its timer or its subagents will wake it. This used to fail the run outright, which made a step that suspended itself deliberately indistinguishable from one that crashed.

`Interrupted` is dropped because `WorkflowRun::on_load` already suspended the step, and a step agent stays cold so its own report arrives long after the repair; acting on it would append a second log entry for one execution.

`Concluded` is the only advancing case, and it is what makes the pair `[StepConcluded, StepStarted]` land in one atomic journal write — closing the window in which a crash could leave a run whose last step concluded but which `on_load` will not advance (see §7).

### 2.8 `spawn_step_agent` (`run.rs:277-347`)

`None` if `self.spec().workflow_run()` is `None` or `run_spec.step(step_name)` is `None`.

Differences from every other agent this session spawns:

- Settings come from `step.settings`, resolved from the run **snapshot**, not through `effective_settings`. At spawn the execution is not in the run log yet — the event that records it persists *after* the agent exists — so the id cannot be looked up.
- Equipment starts as `runners::assemble(RunnerKind::Workflow, ..)` and then gets two `push_front` calls, **front and not back**, because the list ends with the capability that claims every call offered to it, so an appended `submit_result` would be swallowed by the sandbox:
  1. `StepResultCapability::new(step.outcomes, step.fields, step.interactive)` — the typed `submit_result`.
  2. An `AskUserCapability`, always, chosen by `(step.interactive, unattended)`: `(true, false)` → `new()`; `(true, true)` → `unattended()`; `(false, _)` → `not_interactive()`. Equipped even when the step may not ask, or `ask_user` falls through to the sandbox and the model is never told no — and *which* mute matters, because the model is told which.
- `AgentPlan { kind: SessionAgentKind::Step(agent_id), .. }`, which sets `params.requires_result = true` at `mod.rs:546` — a step is the only agent for which a turn ending in plain text is not an answer.

### 2.9 `impl Component for WorkflowRun` (`run.rs:350-477`)

- **`actions`** (`:354-359`) — `cx.spec.workflow_run()` is `None` → `Vec::new()`. Otherwise `WorkflowOrchestrator::new(cx.id, run_spec).step_actions(state)` (`workflow/driver.rs:62-138`), which returns nothing when the run is terminal, `Suspended`, `AwaitingInput` or has a step in flight; `Fail` when the step budget is exhausted, when the start step is missing, when the last step vanished from the definition, or when a transition names a step that is not in the workflow; `Finish { output }` when no transition matched; otherwise one `StartStep`.
- **`on_load`** (`:373-385`) — gated on `cx.spec.workflow_run()?` (the **spec**, not the state), then:
  - `state.run` is `None` → `Some(RunCommand::Advance)` — a run is created and left to begin by itself, with no first message to trigger it. This is the one place a session starts work at load.
  - `run.current().is_some()` → `Some(RunCommand::ReconcileInterrupted)`. A step left in flight is not resumed: how far it got is unknowable, and its effect on the shared workspace with it.
  - `run.status == Pending` → `Some(RunCommand::Advance)`.
  - otherwise `None`.

  The spec gate matters: a conversation also has no run state, and reading only the state would advance one — which, for a session holding an uncollected subagent result, silently starts a turn at load.
- **`busy`** (`:387-389`) — `state.run.as_ref().is_some_and(|r| r.current().is_some())`.
- **`apply`** (`:402-476`) — folds six events:

| event | effect |
|---|---|
| `StepStarted { at_ms, step, agent, attempt, from, via, input, .. }` | `state.run.get_or_insert_with(default)`, then `run.apply_started(..)` (pushes a `StepRun` with `status: Running`, sets run status `Running`); `state.status = Running`; `last_error = None`. **The first `StepStarted` is what turns this state into a run** — `initial_state` is static and cannot see the spec. The `index` field is ignored; position is implied by the push. |
| `StepConcluded { at_ms, index, output }` | `run.apply_concluded(index, output, at_ms)` if a run exists — sets the entry `Concluded`, records output and `ended_at_ms`, and **reasserts** run status `Running`. Session status untouched. |
| `StepFailed { at_ms, index, error }` | `run.apply_step_failed(..)` **and** `run.apply_failed(error)`; `state.status = Failed { reason: error }`; `last_error = Some(error)`. |
| `StepCancelled { at_ms, index }` | `run.apply_cancelled(index, at_ms)` (entry `Cancelled`, run `Suspended`); **`state.status = SessionStatus::Idle`**. |
| `RunFinished { output }` | `run.apply_finished(output)`; `state.status = SessionStatus::Finished` — deliberately not `Idle`, because a run that ran to completion and one that stopped part-way both rest and telling them apart is the point of a run list. |
| `RunFailed { error }` | `run.apply_failed(error)`; `state.status = Failed { reason: error }`; `last_error = Some(error)`. |
| anything else | `unreachable!("WorkflowRun was handed {other:?}")` |

`apply_concluded`'s reassertion of `Running` (`workflow/mod.rs:180-187`) exists for exactly one path: a step parked on a question set `AwaitingInput` and nothing else ever cleared it, so answering resumed the step and then stalled the run at the very step it had just finished.

---

## 3. `SubAgents` — every handler arm

`SubAgents::handle` (`subagent.rs:35-186`) dispatches four `SubAgentCommand` variants.

### 3.1 `SubAgentCommand::Spawn { caller, label, task, agent_type, reply, .. }` (`subagent.rs:42-113`)

The `..` discards `agent: AgentId` — the runners' flat-id-space field is accepted on the wire and ignored by this handler (`types.rs:155-161` says so explicitly: "The old handler still reads `caller`; the runner reads this").

Reply type `Result<Uuid, String>`. Four gates, in this order — see §5 for the full predicates:

1. **Unknown caller (depth resolution).** `owner_for(caller, state.root_owner())` → `tree(owner)` → `map_or_else(|| matches!(caller, Main).then_some(0), |tree| tree.depth_of(caller))` is `None` → reply `Err("caller is not a known agent".to_string())`, `CommandEffect::none()`.
2. **Depth.** `parent_depth >= MAX_SUBAGENT_DEPTH` → reply `Err(format!("max subagent depth {MAX_SUBAGENT_DEPTH} reached"))` — with `MAX_SUBAGENT_DEPTH = 4` (`subagents.rs:11`) this renders as `"max subagent depth 4 reached"`.
3. **Unknown caller again (settings resolution).** `effective_settings_for_parent(state, caller)` is `None` → reply `Err("caller is not a known agent".to_string())` — the *same string* as gate 1, from a different cause.
4. **Concurrency.** `state.subagents.active_count() >= max` → reply `Err(format!("{max} subagents already active"))` — with the default `max` of 8 this renders as `"8 subagents already active"`.

On success, `id = Uuid::new_v4()` and the event is built:

```
SubAgentSpawned { at_ms: now_ms(), id, parent: caller, label, task, depth: parent_depth + 1, agent_type }
```

Then a `oneshot` pair is made, and it **`tokio::spawn`s** (`subagent.rs:96-111`) a detached task holding `actor.me(ctx)`. The task awaits `rx` — the persist ack — and on a closed channel synthesises `Err(JournalError::Backend("spawn ack channel closed".to_string()))`. Either way it then `tell`s the session `SubAgentCommand::FinishSpawn { id, task, agent_type, reply, persisted }`, carrying the original caller's oneshot forward.

Return: `CommandEffect::persist(vec![spawned]).and_ack(ReplyTo::from_sender(tx))`.

**Persist first, spawn second.** A crash between the two replays as a `Running` node with no actor, which recovery reconciles to failed — never an untracked agent. This is the deliberate exception to the tell-then-persist rule delivery uses (`mod.rs:816-820`).

**The reply is deferred twice**: past the journal ack, and then past a second trip through this mailbox.

### 3.2 `SubAgentCommand::FinishSpawn { id, task, agent_type, reply, persisted }` (`subagent.rs:114-144`)

Internal only. Guards:

1. `persisted` is `Err(e)` → reply `Err(format!("persist subagent: {e}"))`, `CommandEffect::none()`. Nothing is spawned; the tool gets the error.
2. `spawn_sub_agent_actor(ctx, state, id, agent_type)` is `None` → reply `Err("could not start the subagent".to_string())`, `CommandEffect::none()`. **The `SubAgentSpawned` event is already durable at this point**, so the node exists in the tree with no actor and will be reconciled to failed at the next load.

Otherwise: `agent.tell(AgentCommand::Enqueue { item: Incoming::User { id: format!("task:{id}"), text: task }, ack: None })` — errors ignored — then reply `Ok(id)`, then `CommandEffect::none()`.

Queued rather than run directly, so a subagent has one way in whatever is addressed to it. It drains at once, because there is nothing else in its queue and nothing in flight.

### 3.3 `SubAgentCommand::Status { caller, id, reply, .. }` (`subagent.rs:145-168`)

Again `..` discards `agent: AgentId`. Reply type `Result<String, String>`, sent **immediately**, `CommandEffect::none()`. Journals nothing.

`tree = state.subagents.owner_for(caller, state.root_owner()).and_then(|owner| state.subagents.tree(owner))` — visibility is answered within the caller's own tree, so a step and a conversation each see their own and neither learns the other exists.

- `Some(id)` **and** `tree.is_some_and(|t| t.visible_to(caller, &id))` → `tree.and_then(|t| t.render_node(&id)).ok_or_else(|| format!("no such subagent: {id}"))`.
- `Some(id)` otherwise → `Err(format!("no such subagent: {id}"))`. Out-of-subtree and unknown ids are indistinguishable — neither confirms the node exists.
- `None` → `Ok(tree.map(|t| t.render_subtree(caller)).unwrap_or_else(|| "No subagents.\n".to_string()))`.

`visible_to` (`subagents.rs:298-303`): a `Main` caller sees every node in the tree; a `SubAgent(root)` caller sees itself and its own descendants, never siblings. `render_subtree` (`:308-336`) *excludes* the caller itself when the caller is a subagent, and falls back to the same `"No subagents.\n"` when it produces nothing.

### 3.4 `SubAgentCommand::Reconcile` (`subagent.rs:169-184`)

Internal, no reply. `state.subagents.interrupted()` — every node still `Running`, across every tree (`subagents.rs:452-457`) — is empty → `CommandEffect::none()`. Otherwise `CommandEffect::persist(..)` of one `SubAgentFailed { at_ms: now_ms(), id, error: INTERRUPTED_ERROR.to_string() }` per id, where `INTERRUPTED_ERROR = "interrupted by restart"` (`subagents.rs:17`).

Plain `persist`, **not** `persist_and_advance` — the parents these failures are now owed to are not woken in this batch. They are delivered at the next boundary, which for a conversation is the user's next message (§4) and for a subagent parent is the next subagent outcome.

### 3.5 `on_sub_agent_outcome` (`subagent.rs:196-242`) — routed by identity

Reached when the reporting agent is neither the session, nor a step, nor a fork. No reply.

Guard: `state.subagents.node(id).is_none()` → `tracing::warn!(subagent = %id, "outcome from an unknown subagent; ignored")`, `CommandEffect::none()`.

| `TurnEnd` | event (`at_ms = now_ms()`) |
|---|---|
| `Concluded { output }` | `SubAgentCompleted { at_ms, id, output: output.as_str().map(str::to_string).unwrap_or_else(\|\| output.to_string()) }` — a JSON string is unwrapped to its contents; anything else is serialised |
| `Failed { error, .. }` | `SubAgentFailed { at_ms, id, error }` — `terminal` discarded |
| `Asked` | `SubAgentFailed { at_ms, id, error: "subagent asked the user; not supported" }` — defensive; a subagent has no ask tool |
| `Parked` | `SubAgentFailed { at_ms, id, error: "subagent parked; timers are not supported in sessions" }` — defensive |
| `Interrupted` | **`return CommandEffect::none()`** |

Every non-`Interrupted` case ends in `persist_and_advance` (`:241`), so a subagent's outcome is a delivery flush point for the whole forest.

`Interrupted` is dropped because a subagent's interruption is repaired from the forest at *session* load by `SubAgents::on_load`. A subagent actor stays cold and spawns on demand, so its own recovery runs long after the node was reconciled; acting on it would fail the same node twice.

### 3.6 `spawn_sub_agent_actor` (`subagent.rs:250-284`)

`None` if `effective_settings(state, AgentKey::Sub(id))` is `None`. Settings are derived from the node's **stored parent** via the tree root (`mod.rs:722-726`): a workflow step's spawns run under the step's preset, a conversation's under the main agent's — never a fabricated session-wide value. That is what lets a cold node be woken correctly.

Equipment is `runners::assemble(RunnerKind::SubAgent, ..)` with `agent_type` passed through; nothing is pushed on top. A worker owes a report: it can delegate further, but it cannot ask, name the session or branch it.

`agent_type` travels no further than the provider — the *definition* is resolved from the library scan when the subagent runs, so an agent whose plugin was removed in between fails loudly rather than running with a prompt nobody can point at.

### 3.7 `impl Component for SubAgents` (`subagent.rs:287-370`)

- **`actions`** (`:291-293`) — `crate::sessions::orchestrator::owed_deliveries(state)` (`orchestrator.rs:77-101`). Every terminal, un-notified node yields one `AgentAction::Deliver { to, child, part }`. Recipient: `SubAgentParent::SubAgent(p)` → `AgentKey::Sub(p)`; `SubAgentParent::Main` → `AgentKey::Step(agent)` or `AgentKey::Main` **read off the owning tree**, not off the session's current root, so a step that has since been superseded is still what asked. It is unconditional on what the recipient is doing: the result goes into a queue, and when that queue becomes a turn is the agent's own rule.
- **`on_load`** (`:297-300`) — `(!state.subagents.interrupted().is_empty()).then_some(SessionCommand::SubAgent(SubAgentCommand::Reconcile))`.
- **`busy`** (`:304-306`) — `state.subagents.has_active()`, forest-wide. This is the invariant that keeps a forty-minute tool call from being unloaded out from under itself.
- **`apply`** (`:319-369`) — folds five events:

| event | effect |
|---|---|
| `SubAgentSpawned { id, parent, label, task, depth, at_ms, agent_type }` | owner = `state.subagents.owner_for(parent, state.root_owner()).unwrap_or(TreeOwner::Main)` — resolved against the state **before** this event, so it is the step in flight for a run and `Main` otherwise — then `tree_mut(owner).apply_spawned(..)` |
| `SubAgentRunning { id, at_ms }` | `owner_of(id)` → `apply_running`; **silently nothing if the node is unknown** |
| `SubAgentCompleted { id, output, at_ms }` | `owner_of(id)` → `apply_completed`; silently nothing if unknown |
| `SubAgentFailed { id, error, at_ms }` | `owner_of(id)` → `apply_failed`; silently nothing if unknown |
| `SubAgentNotified { id, .. }` | `owner_of(id)` → `apply_notified`; silently nothing if unknown |
| anything else | `unreachable!("SubAgents was handed {other:?}")` |

`SubAgentNotified` is journaled by `SessionActor::deliver` (`mod.rs:824-851`) — tell-then-persist, so a crash in that window leaves the result still owed and the next boundary re-delivers it. Delivery is at-least-once, never lost. It is *skipped, not failed*, when the agent cannot be reached.

---

## 4. `on_user_message` in full (`turns.rs:398-505`)

Signature: `(&mut self, state, agent_id: Option<String>, text: String, reply: ReplyTo<Result<MessageAccepted, UserMessageError>>, ctx)`. `UserMessageError` has three variants (`sessions/mod.rs:417-427`): `NotFound` (`"session not found"`), `Unrecoverable(String)`, `Rejected(String)`.

The name understates it. In order:

### 4.1 The `Unrecoverable` refusal (`:406-409`)

```rust
if let SessionStatus::Unrecoverable { reason } = &state.status {
    let _ = reply.send(Err(UserMessageError::Unrecoverable(reason.clone())));
    return CommandEffect::none();
}
```

Journals nothing, on this session or on any agent. A terminal session refuses a message outright rather than queueing one nothing will ever answer. `Unrecoverable` is reached only from `SessionFailed`, i.e. from a `TurnEnd::Failed { terminal: true }` or a terminal provisioning failure.

### 4.2 The workflow-root refusal (`:413-420`)

```rust
if self.spec().workflow_run().is_some()
    && agent_id.as_deref().unwrap_or(super::MAIN_AGENT_ID) == super::MAIN_AGENT_ID
{
    let _ = reply.send(Err(UserMessageError::Rejected(
        "this session is a workflow run; name a step agent to message it".to_string(),
    )));
    return CommandEffect::none();
}
```

Gated on the **spec**, so it fires from creation, before any `StepStarted` exists. Naming a step is fine — that agent exists and can be spoken to like any other. Both `None` and the literal `"main"` are refused. Journals nothing.

### 4.3 Agent resolution (`:421-424`)

`resolve_agent(state, ctx, agent_id.as_deref())` is `None` → reply `Err(UserMessageError::NotFound)`, `CommandEffect::none()`. `resolve_agent` spawns cold steps, forks and subagents on demand, so "not found" genuinely means no such agent in this session's state.

### 4.4 `/fork` and `/summary-n-fork` detection (`:428-430`)

```rust
if let Some(req) = Self::fork_request(text.trim()) {
    return self.start_fork(state, key, req, reply, ctx);
}
```

Deliberately resolved **before** the session is titled: a fork command is not a thing to name a session after, because it says what the *new* conversation should do. See §1.5 for `start_fork`.

### 4.5 `title_from_first_message` (`:434`, implementation `core.rs:105-115`)

```rust
self.title_from_first_message(&text).await;
```

- No-op if `self.spec().name.is_some()`.
- `derive_title(text)` (`core.rs:22-32`): the first line, trimmed; `None` if empty; returned as-is if `<= TITLE_MAX_CHARS` chars; otherwise the first `TITLE_MAX_CHARS` chars with trailing whitespace trimmed and `…` appended (so the result is `TITLE_MAX_CHARS + 1` chars). `TITLE_MAX_CHARS = title_tool::SESSION_TITLE_MAX_CHARS` (`core.rs:19`).
- `rename_session(title)` (`core.rs:118-139`) asks the **supervisor** to persist it, then sets `self.spec_mut().name` and `tell`s `PublishSessionTitle`. A failure is logged at `warn` and swallowed: `"failed to persist fallback session title"`.

It is called for a message addressed to **any** agent — a subagent's or a fork's message titles the session too — and it awaits a supervisor round-trip on this mailbox.

Note it journals nothing here; the `Renamed` event is `SessionCore`'s and is written on the `CoreCommand` paths, not this one.

### 4.6 `/compact` handling and the builtin fallthrough (`:436-465`)

`id = Uuid::new_v4().to_string()` is minted first — the message id the caller is given.

```rust
let item = match parse_invocation(text.trim(), '/').and_then(|(name, args)| builtin(name).map(|b| (b, args))) {
    Some((builtin, args)) if builtin.name == "compact" => Incoming::Compact {
        id: id.clone(),
        instructions: (!args.trim().is_empty()).then(|| args.trim().to_string()),
    },
    Some((builtin, _)) => {
        tracing::error!(builtin = builtin.name, "unhandled builtin command");
        Incoming::User { id: id.clone(), text: text.clone() }
    }
    None => Incoming::User { id: id.clone(), text: text.clone() },
};
```

A built-in is resolved **before** anything treats the text as a prompt: `/compact` asks the server to do something and must never reach `expand_invocation`, a template, or the model. The builtin table is consulted ahead of the plugin catalogue, so an installed bundle cannot take over a control the product owns (`builtins.rs:1-16`).

`instructions` is `None` when the args are empty or whitespace.

The middle arm is unreachable with today's table: `compact` is handled, and `fork`/`summary-n-fork` were already caught at §4.4. See §7 for what the comment there claims versus what the code does.

### 4.7 How `MessageAccepted` is resolved (`:466-487`)

A `oneshot` pair, and a **`tokio::spawn`**:

```rust
let (tx, rx) = oneshot::channel();
let accepted = id.clone();
tokio::spawn(async move {
    let answer = match rx.await {
        Ok(Ok(())) => Ok(MessageAccepted::queued(accepted)),
        Ok(Err(e)) => Err(UserMessageError::Rejected(format!("persist message: {e}"))),
        Err(_)     => Err(UserMessageError::NotFound),
    };
    let _ = reply.send(answer);
});
agent.tell(AgentCommand::Enqueue { item, ack: Some(ReplyTo::from_sender(tx)) }).await
```

**The ack it waits on is the *agent's* durable write, not the session's.** `tx` is handed to the agent as the `Enqueue` ack, and the agent's `handle_command` attaches it to `CommandEffect::persist(vec![AgentDomainEvent::Received { item, at_ms }])` (`agent_actor.rs:2373-2386`). So the caller's `202` means "this message is in the agent's journal and will survive a crash". The session mailbox never blocks on that write.

`MessageAccepted::queued(id)` sets `forked_agent: None` (`types.rs:210-219`); only `start_fork` sets it.

A failed `tell` logs `tracing::warn!(session = %self.id, "message could not reach the agent")` and falls through — the spawned task is still waiting on `rx`, whose sender was dropped with the failed command, so the caller receives `UserMessageError::NotFound`.

### 4.8 The re-provision nudge (`:494-499`)

```rust
if matches!(state.status, SessionStatus::ProvisioningFailed { .. }) {
    let _ = self.me(ctx).tell(SessionCommand::Lifecycle(LifecycleCommand::Provision)).await;
}
```

A session whose create failed has no runtime, so the message the UI invited ("send a message to try again") has to build one rather than start a turn that would ask for it. The message waits in the agent's queue and the create's own completion releases it, exactly as at session creation.

This is a `tell` back into this mailbox; it queues behind whatever else is there. Note the message was already enqueued and its ack already promised at this point.

### 4.9 The turn boundary (`:504`)

```rust
self.persist_and_advance(state, Vec::new(), ctx).await
```

Zero events of its own, but the drain runs. A person acting is the boundary that flushes results owed to subagent parents: those strand once every node is terminal, because no further subagent outcome will arrive to trigger a flush, so the next thing the user does has to be what delivers them.

Interaction with §4.8: `ProvisioningFailed` makes `RuntimeLifecycle::ready` false, so on that path `next_actions` returns empty and the drain does nothing.

---

## 5. The subagent gates, exactly

All three live in `SubAgentCommand::Spawn` (`subagent.rs:50-80`) and are evaluated in the order below. Each reply is an `Err(String)` on the tool's own oneshot; none journals anything.

### 5.1 Unknown caller

```rust
let owner = state.subagents.owner_for(caller, state.root_owner());
let Some(parent_depth) = owner
    .and_then(|owner| state.subagents.tree(owner))
    .map_or_else(
        || matches!(caller, SubAgentParent::Main).then_some(0),
        |tree| tree.depth_of(caller),
    )
else { /* refuse */ };
```

- `owner_for(Main, root)` = `Some(root)`; `owner_for(SubAgent(id), _)` = `owner_of(id)`, i.e. `None` for an id in no tree (`subagents.rs:418-423`).
- `root_owner()` (`types.rs:769-774`) = `TreeOwner::Step(current_agent)` when a run has a step in flight, `TreeOwner::Main` otherwise.
- If there is no tree yet for that owner (an empty forest, the very first spawn of a session), the `map_or_else` default applies: a `Main` caller gets depth `0`, anything else gets `None`.
- If there is a tree, `depth_of` (`subagents.rs:210-215`): `Main` → `Some(0)`; `SubAgent(id)` → the node's stored `depth`, or `None` if the node is not in *that* tree.

Refusal: **`"caller is not a known agent"`**.

### 5.2 Depth

```rust
if parent_depth >= MAX_SUBAGENT_DEPTH { /* refuse */ }
```

`MAX_SUBAGENT_DEPTH = 4` (`subagents.rs:11`). The child's recorded depth is `parent_depth + 1`, so the deepest node that can exist is depth 4 (Main=0 → d1 → d2 → d3 → d4), and a spawn *by* d4 is refused.

Refusal: `format!("max subagent depth {MAX_SUBAGENT_DEPTH} reached")` → **`"max subagent depth 4 reached"`**.

### 5.3 Unknown caller, second time

```rust
let Some(settings) = actor.effective_settings_for_parent(state, caller) else { /* refuse */ };
```

`effective_settings_for_parent` (`mod.rs:733-745`): a `Main` caller resolves through `state.root_owner()` to either the session's agent settings or the step's; a `SubAgent(id)` caller resolves through `effective_settings(state, AgentKey::Sub(id))`, which reads `owner_of(id)` and then the tree root's settings (`mod.rs:709-727`).

Refusal: **`"caller is not a known agent"`** — the same literal as §5.1.

### 5.4 The concurrency cap

```rust
let max = settings.max_subagents();
if state.subagents.active_count() >= max { /* refuse */ }
```

Refusal: `format!("{max} subagents already active")` → with the default, **`"8 subagents already active"`**.

**Where the number comes from:** `AgentSettings::max_subagents()` (`spec.rs:117-121`) = `self.max_concurrent_subagents.unwrap_or(DEFAULT_MAX_CONCURRENT_SUBAGENTS)`, and `DEFAULT_MAX_CONCURRENT_SUBAGENTS = 8` (`subagents.rs:14`). Those settings are the *caller's*, resolved in §5.3 — so a workflow step's spawns are budgeted by the step's own preset.

**Per-tree or per-session?** The *measure* is per-session. `SubAgentForest::active_count()` (`subagents.rs:442-444`) sums `active_count()` over **every tree in the forest** — the conversation's and every step's alike. So the cap is a session-wide count compared against a per-caller limit. See §7.

---

## 6. Step lifecycle

### 6.1 Starting

Two entry points, and only two.

1. **The driver.** `WorkflowRun::actions` → `WorkflowOrchestrator::step_actions` → `AgentAction::StartStep(StepStart { index, step, agent, attempt, from, via, input })` → `SessionActor::perform` (`mod.rs:807`) → `start_step`. `index` is `run.steps.len()`, `agent` is `WorkflowRunSpec::step_agent_id(session_id, index)` — derived from the session id and the log position, so replay reconstructs it — and `attempt` is `run.attempts_of(step_name) + 1`. `input` is `compose_step_input(prompt, from_step, incoming)`, which for a non-first step wraps the previous output under a `## Input from step \`<name>\`` header after the step's own prompt.
2. **A retry.** `on_retry_step` builds the `StepStart` itself with the same `index`/`agent`/`attempt` derivation but copies `from`, `via` and `input` from the entry being retried.

`start_step` spawns the agent (which is what makes `AgentPlan::kind = Step`, `requires_result = true`, and equips the typed `submit_result`), enqueues `Incoming::User { id: format!("step:{index}:{attempt}"), .. }`, and journals `StepStarted`. `apply` pushes a `StepRun { status: Running }` and sets both the run and the session `Running`, clearing `last_error`.

The gate that holds the *first* step back is `next_actions`' `RuntimeLifecycle::ready` check, not anything in this file.

### 6.2 Concluding

The step agent calls `submit_result`, its turn ends `Concluded`, `on_agent_outcome` routes it by identity to `on_step_outcome`, which journals `StepConcluded { index, output }` and calls **`persist_and_advance`**. The local fold makes the entry `Concluded` and reasserts the run `Running`; `flush_then_drain` then asks `WorkflowRun::actions` for the next action against that folded state. So `[StepConcluded, StepStarted]`, or `[StepConcluded, RunFinished]`, or `[StepConcluded, RunFailed]`, land as **one atomic write**.

A step's turn ending is not a step ending. A turn of plain text from an agent with `requires_result = true` is nudged by the agent loop (`MAX_RESULT_NUDGES = 2`, `agent_actor.rs:67` — the first nudge is a plain message, the second forces `submit_result` in `tool_choice`), and only a model that defeats both fails the step.

### 6.3 Failing

`TurnEnd::Failed` → `StepFailed { index, error }`, plain `persist`, no advance. `apply` marks the entry `Failed`, marks the *run* `Failed` with the same error, and sets the session `Failed { reason: error }`. `terminal` is ignored: retrying is a decision for a person, because the shared workspace holds whatever the failed attempt left behind.

A `RunFailed` reaches the log by a different route: `start_step`'s two failure branches, and `AgentAction::Fail` from the driver (`fail_run`).

### 6.4 Parking

Two different parks, and they are not the same thing.

- **`TurnEnd::Asked`** → `AskRecorded`, no advance. The entry stays `Running`; `Turns::apply` sets the session `AwaitingInput` **and** calls `run.apply_awaiting()`. The answer, sent to the step agent (unaddressed resolves to the step in flight), resumes it.
- **`TurnEnd::Parked`** (a timer armed, or subagents outstanding) → **no events at all**, no advance. The entry stays `Running` and the run stays `Running`. Nothing in the session records the park.

### 6.5 Cancelling

Three routes, all producing `StepCancelled { index }`:

1. `Stop` addressed to the step's agent id (or unaddressed on a run) → `stop_boundary` gated on `run.current() == Some(index)` → `cancel_agent`, reply `Ok(())`, then `persist_and_advance`.
2. `RunCommand::ReconcileInterrupted` at load, when `run.current()` is `Some`. No agent is cancelled — there is no live agent after a restart.
3. `on_retry_step`, for whatever was in flight, immediately before the replacement starts.

`apply_cancelled` marks the entry `Cancelled` and the run `Suspended`; `WorkflowRun::apply` additionally sets the *session* status to `Idle`. `step_actions` returns nothing for a `Suspended` run, so only a retry can move it.

### 6.6 Retrying, and how attempts are numbered

See §2.3 for the exact body. The numbering rule is the same in both start paths: `attempt = <run state>.attempts_of(step_name) + 1`, where `attempts_of` (`workflow/mod.rs:139-141`) counts **every** entry in the log with that step name, on any path. So a step reached twice by a loop and a step retried once both yield `attempt: 2` — the number is "how many executions of this step exist", not "how many retries".

The retry is appended at `new_index = steps.len()`, never replacing, so the earlier attempt stays readable and the graph stacks them on the same node. `from` and `via` are copied so it draws on the same edge.

### 6.7 Recovery

`WorkflowRun::on_load` (§2.9). A step the process died inside is **suspended, not resumed**: how far it got is unknowable, and its effect on the shared workspace with it. Without this the entry stayed `Running`, so `current()` never cleared and the driver started nothing ever again — the run wedged while its page read "Running".

`TurnEnd::Interrupted` from the step agent's own recovery is dropped, because the repair already happened and a step agent stays cold long enough that its report always arrives second.

### 6.8 `stop_boundary` (`turns.rs:158-195`)

Two questions kept deliberately apart. *Which agent* is `stop_target` — pure resolution, never spawning. *Whether it is doing anything* is `stop_boundary`, which answers with the event to journal, so the gate and the record cannot disagree about what was stopped.

Every kind ends its turn in its own vocabulary — the session's status is the main agent's, a fork's is its roster entry, a subagent's is its node in the forest — so there is no one event that means "stopped" for all of them, and the mapping lives in one place rather than four. Full table in §1.2.

---

## 7. Surprises, and comments the code contradicts

Highest-value section. Each item quotes both sides.

### 7.1 The subagent cap counts the whole session but is budgeted per caller

`subagent.rs:69-71` says:

> // The cap is the *caller's* settings' cap: a workflow step's
> // spawns are counted against the step's preset, never against a
> // session-wide value that nothing in a run owns.

But the measure at `subagent.rs:77` is `state.subagents.active_count()`, and `SubAgentForest::active_count` (`subagents.rs:442-444`) is:

```rust
pub fn active_count(&self) -> u32 {
    self.trees.values().map(SubAgentTree::active_count).sum()
}
```

— a sum over **every tree in the forest**. A step's spawns are therefore counted against the conversation's subagents and against every other step's, while the *limit* they are compared to comes from that one step's preset. Two steps with different `max_concurrent_subagents` see different limits over the same shared count, and whichever runs second can be refused by work it does not own.

### 7.2 `Turns::apply` writes `WorkflowRun`'s slice

`component.rs:11-14` states the discipline:

> //! A component may **read** any part of the state ... It may **write**
> //! only its own slice, through its own events. Reading across is what keeps
> //! components from having to talk to each other; writing across is what this
> //! trait exists to prevent.

`turns.rs:560-565` does exactly that:

```rust
SessionDomainEvent::AskRecorded { .. } => {
    state.status = SessionStatus::AwaitingInput;
    if let Some(run) = state.run.as_mut() {
        run.apply_awaiting();
    }
}
```

`AskRecorded` is routed to `Turns::apply` by `mod.rs:1115`, and a *step*'s park is journaled as `AskRecorded` by `run.rs:231-239`. So a run's `AwaitingInput` status is written by the conversation component, into the run component's state. Correspondingly, `WorkflowRunState::apply_concluded` has to reassert `Running` (`workflow/mod.rs:180-187`) to undo it.

### 7.3 The `Turns` module doc names `state.run`; the code reads the spec

`turns.rs:14-16`:

> //! Silent when `state.run` is set and no agent is named: a run works from its
> //! definition and there is nobody to send *it* a message

The guard at `turns.rs:413` is `self.spec().workflow_run().is_some()`. These differ for a real interval: a workflow session that has not yet folded a `StepStarted` has `state.run == None` but `spec.workflow_run() == Some`. Reading the spec is the stronger and evidently intended behaviour — the refusal is live from creation — but the module doc describes the weaker one.

`stop_target` (`turns.rs:124`) and `resolve_agent` (`mod.rs:636`) *do* read `state.run`, so the three disagree about when a session "is a run".

### 7.4 `Stop` on `"main"` during a run's gap between steps journals `TurnStopped`

`stop_target` maps `"main"` to `AgentKey::Main` whenever `run.current_agent()` is `None` (`turns.rs:124-127`), and `stop_boundary(Main)` fires on `state.status == Running` (`turns.rs:164-165`). A run between steps has no current step but *does* have `status == Running` (set by `StepStarted`, and never cleared by `StepConcluded`). So a `Stop` landing in that window journals `TurnStopped` and sets the session `Idle` while the run continues, and `cancel_agent(AgentKey::Main)` is a no-op because `SessionAgents::Workflow` has no main (`mod.rs:128-134`). The window is narrow because `[StepConcluded, StepStarted]` is one atomic write, but the mailbox can be entered between the persist and the next command.

### 7.5 The unhandled-builtin arm sends the slash command to the model as prose

`turns.rs:452-460`:

> // Every other built-in, present and future. Reaching here means the
> // table names something this match does not handle, which is a bug
> // rather than a message: sending it on as a prompt would show the
> // user's `/thing` to the model as if it were prose.

```rust
Some((builtin, _)) => {
    tracing::error!(builtin = builtin.name, "unhandled builtin command");
    Incoming::User { id: id.clone(), text: text.clone() }
}
```

The comment names the failure mode and then the code performs it, logging at `error` first. It is unreachable with today's `BUILTINS` table (`compact` is handled here; `fork` and `summary-n-fork` are caught earlier by `fork_request`), so adding a fourth built-in without touching this match silently ships the bug the comment describes.

### 7.6 A dropped `Answer` reply is never resolved

`turns.rs:216-222` moves the caller's oneshot into the agent command, then on failure only warns. The caller's `ask` sees a closed channel rather than an `AnswerError`, which is a different failure surface from every other refusal in these three files. Contrast `on_user_message`, which maps its equivalent case onto `UserMessageError::NotFound` (`turns.rs:474`).

### 7.7 An unknown agent on `Answer` is reported as `NothingPending`

`turns.rs:212-215`. `AnswerError` has no not-found variant (`agent_loop/inbox.rs:162-170`), and `NothingPending` displays as `"this agent is not waiting on an answer"` — so "you named an agent that does not exist" and "that agent has no questions" are indistinguishable to a caller.

### 7.8 A dropped message ack becomes `NotFound`

`turns.rs:474`: `Err(_) => Err(UserMessageError::NotFound)`, where `NotFound` displays as `"session not found"`. A message that reached a live agent whose mailbox then died is reported to the client as a missing session.

### 7.9 `FinishSpawn` can leave a durable node with no actor

`subagent.rs:125-128`: `spawn_sub_agent_actor` returning `None` replies `"could not start the subagent"` — but `SubAgentSpawned` is already durable, so the tree holds a `Running` node forever until a load reconciles it to failed. The persist-then-spawn comment (`subagent.rs:81-83`) covers the *crash* case explicitly and this in-process case only by implication.

### 7.10 `run.rs:414-419` contains the same comment twice

```rust
// The first step is what turns the state into a run:
// `initial_state` is static and cannot see the spec, so the mode
// is established by the log rather than at construction.
// The first step is what turns this state into a run:
// `initial_state` is static and cannot see the spec, so the run
// is established by the log rather than at construction.
```

Two near-identical paragraphs, differing only in "the state"/"this state" and "the mode"/"the run".

### 7.11 `finish_run` and `fail_run` are `async fn(&mut self)` that need neither

`run.rs:124-137`. They contain no `.await` and touch no field. They are `async` and `&mut self` only to fit the shape `perform` calls them with.

### 7.12 `on_retry_step` skips the readiness gate

Every other path that starts a step goes through `next_actions`, which returns nothing unless `RuntimeLifecycle::ready(state)` (`mod.rs:916-918`). `on_retry_step` calls `start_step` directly (`run.rs:189-208`), so a retry on a `Provisioning` or `ProvisioningFailed` session spawns a step agent and enqueues its input regardless.

### 7.13 `Reconcile` does not flush what it makes owed

`subagent.rs:174-183` uses plain `CommandEffect::persist`, so the `SubAgentFailed` events it writes are owed to their parents but nothing delivers them in that batch. The test `a_stranded_grandchild_result_flushes_at_the_next_turn_boundary` pins the consequence: the delivery waits for the user's next message.

### 7.14 `Turns::busy` is true for a run

`turns.rs:533-535`:

> /// A turn in flight. `WorkflowRun` answers for a step, so this is only ever
> /// asked about a conversation — but `status` is shared, so the check is the
> /// same either way and double-counting is harmless.

`StepStarted` sets `state.status = Running` (`run.rs:424`), so `Turns::busy` returns `true` for a running step as well. The comment acknowledges this; it is noted here only because "only ever asked about a conversation" reads as a precondition and is not one.

---

## 8. Test inventory

### `turns.rs` — `mod tests` (`:588-1050`)

The module doc (`:596-602`) states the split: the queue's own rules — what merges, what waits out a park, what an answer must cover — belong to the agent and are tested in `crate::agent_loop::inbox`. What is left here is what the session still owns.

| # | test | what it pins |
|---|---|---|
| 1 | `a_fresh_session_is_idle` (`:612`) | `SessionState::default().status == Idle` |
| 2 | `a_turn_beginning_clears_the_previous_failure` (`:619`) | folding `TurnFailed` then `TurnBegan` yields `Running` **and `last_error == None`** — the detail endpoint must not advertise the previous turn's failure. **Exact string**: seeds and reads back `"provider exploded"` |
| 3 | `a_failed_turn_is_sticky_but_not_terminal` (`:634`) | `TurnFailed` → `Failed { .. }` with `last_error` set. **Exact string** `"provider exploded"` |
| 4 | `stop_and_interrupt_both_land_idle` (`:644`) | `TurnStopped` and `TurnInterrupted` both fold to `Idle` after a `TurnBegan` |
| 5 | `an_ask_parks_the_session_without_carrying_the_questions` (`:658`) | `AskRecorded` → `AwaitingInput` carrying no questions; a later `TurnBegan` → `Running` |
| 6 | `an_unrecoverable_session_refuses_a_message` (tokio, `:668`) | §4.1: the refusal is `UserMessageError::Unrecoverable` **and journals nothing** — asserted by comparing journal length across two refusals |
| 7 | `a_reported_interruption_ends_the_turn` (tokio, `:738`) | a journal ending at `TurnBegan`, plus a reported `AgentOutcome::Interrupted`, settles at `Idle` |
| 8 | `a_reported_interruption_leaves_a_failed_turn_alone` (tokio, `:769`) | the `state.status == Running` guard in `on_main_outcome`: an interruption over an already-`Failed` turn changes nothing and journals no `TurnInterrupted`. **Exact string**: asserts `SessionStatus::Failed { reason: "provider said no" }` |
| 9 | `a_run_refuses_an_unaddressed_message` (tokio, `:820`) | §4.2 — refusal is `UserMessageError::Rejected` (variant only, not the string) |
| 10 | `a_message_is_acknowledged_after_the_agents_durable_write` (tokio, `:844`) | §4.7 — the returned `message_id` names a `LifecycleEvent::MessageQueued` entry in the *agent's* log. **Exact string**: `q.text == "go"` |
| 11 | `a_report_waits_out_an_awaiting_input_session` (tokio, `:882`) | end-to-end: a subagent completing during `AwaitingInput` is delivered but does not release the park; the user's next message does, and carries the report. **Exact strings**: `"[subagent \"research\" completed]"`, `"the first one"`, and the abandoned ask's result containing `"not answered"` |
| 12 | `a_message_can_name_a_subagent` (tokio, `:994`) | a message addressed to a subagent lands in that agent's log and **no other** |
| 13 | `a_message_naming_an_unknown_agent_is_refused` (tokio, `:1034`) | §4.3 — `UserMessageError::NotFound` |

Helper: `load_from` (`:724`) seeds a session journal and loads it, so the actor recovers from exactly what a killed process would have left.

### `run.rs` — `mod tests` (`:479-1038`)

All `#[tokio::test]`; there is no synchronous test in this file.

| # | test | what it pins |
|---|---|---|
| 1 | `a_completed_run_reports_finished` (`:503`) | `RunFinished` → `SessionStatus::Finished`, not `Idle` |
| 2 | `a_run_starts_itself_and_routes_on_its_first_steps_output` (`:530`) | the whole happy path: nobody sends a message; the first step's output picks the branch. **Exact strings**: `via == Some("outcome in [p0]")`, input contains ``"## Input from step `triage`"`` and starts with `"Fix it."`. Also pins `steps[0].agent == WorkflowRunSpec::step_agent_id(id, 0)` and that the two steps have different agents |
| 3 | `a_non_matching_condition_takes_the_catch_all` (`:575`) | the `else` branch, `via.is_none()` |
| 4 | `a_step_that_ends_a_turn_with_text_is_nudged_and_then_submits` (`:596`) | a turn ending is not a step ending; the nudged step still concludes, and the post-nudge result is the one that routes |
| 5 | `a_step_that_ends_a_turn_holding_a_timer_is_parked_not_nudged` (`:641`) | `TurnEnd::Parked` produces nothing: `nudges == 0`, one timer armed, entry still `Running` |
| 6 | `submitting_cancels_the_timers_the_step_had_armed` (`:696`) | conclusion disarms timers — and first asserts a timer really was armed, because the test passed vacuously when first written |
| 7 | `a_step_that_never_submits_fails_the_run` (`:750`) | the nudge budget is finite. **Exact string**: the run error must `contains("submit_result")` |
| 8 | `retrying_a_step_appends_an_attempt_on_the_same_edge` (`:778`) | §2.3/§6.6: a third entry with `attempt == 2`, same `from`/`via` as the original, and the first attempt untouched at `Concluded` |
| 9 | `a_runs_first_step_waits_for_the_create_too` (`:817`) | the `RuntimeLifecycle::ready` gate: while `Provisioning`, `run.steps` is empty; releasing the create starts step one |
| 10 | `a_parked_step_is_answered_unaddressed_and_the_run_carries_on` (`:860`) | three defects in one: an unaddressed `Answer` reaches the step in flight; the park leaves the entry `Running`; `AwaitingInput` is cleared so the run does not stall at the step it just finished |
| 11 | `interrupting_a_run_cancels_its_step_and_suspends_it` (`:912`) | `Stop` on a step agent → run `Suspended`, entry `Cancelled`, `current()` clear |
| 12 | `recovery_suspends_a_run_whose_step_was_interrupted` (`:951`) | `on_load` → `ReconcileInterrupted`: entry `Cancelled`, run `Suspended`, and `steps.len() == 1` — recovery starts nothing by itself |
| 13 | `a_cold_steps_transcript_is_still_readable_after_a_reload` (`:1000`) | after `node.restart()`, a step agent spawns on demand for a `ReadLog`, so a finished run's step page is not permanently blank |

### `subagent.rs` — `mod tests` (`:372-1003`)

| # | test | what it pins |
|---|---|---|
| 1 | `a_subagent_only_repair_still_reports_a_status` (tokio, `:399`) | a session whose only repair is `Reconcile` still reports a status at load — `Reconcile` persists and reports nothing itself, so `adopt` must report unconditionally |
| 2 | `subagent_events_fold_into_the_tree` (`#[test]`, `:422`) | `SubAgentSpawned` → `active_count == 1`; `SubAgentCompleted` → `Completed` and `!notified`; `SubAgentNotified` → `notified` |
| 3 | `a_running_then_failed_subagent_reads_as_interrupted_then_terminal` (`#[test]`, `:453`) | a spawned-but-unfinished node reads as `interrupted()`; a terminal event clears it |
| 4 | `spawn_records_a_running_subagent_in_the_tree` (tokio, `:478`) | the spawn is durable and attributed: `depth == 1`, `parent == Main`. **Exact strings**: `label == "research"`, `task == "dig into it"` |
| 5 | `spawn_beyond_depth_four_is_rejected` (tokio, `:498`) | §5.2. **Exact string**: `assert_eq!(res.unwrap_err(), "max subagent depth 4 reached")` |
| 6 | `spawn_beyond_the_concurrency_cap_is_rejected` (tokio, `:540`) | §5.4. **Exact string**: `assert_eq!(res.unwrap_err(), "8 subagents already active")` |
| 7 | `spawn_from_an_unknown_caller_is_rejected` (tokio, `:564`) | §5.1. **Exact string**: `assert_eq!(res.unwrap_err(), "caller is not a known agent")` |
| 8 | `a_completed_subagent_notifies_an_idle_main_agent` (tokio, `:584`) | delivery reaches the parent, and the result is its own content part — **never** merged into the user's text. **Exact strings**: `"[subagent \"research\" completed]"`, `"sub answer"`, and `"[subagent "` must appear in no user text |
| 9 | `a_completed_subagent_closes_the_turn_in_its_own_log` (tokio, `:623`) | the child's own log holds exactly one `TurnOutcome::Ended` — a finished subagent's page used to read `RUNNING` forever |
| 10 | `stopping_a_subagent_cancels_it_and_tells_the_parent` (tokio, `:645`) | §1.2's `Sub` row: the node goes `Failed` and the parent is told. **Exact string**: `"[subagent \"research\" failed]"` |
| 11 | `a_failed_subagents_own_log_carries_the_error` (tokio, `:688`) | the failure is visible where a reader will open it. **Exact string**: the turn's error `contains("bad key")` |
| 12 | `a_failed_subagent_reports_the_error_to_its_parent` (tokio, `:706`) | the same failure reaches the parent's transcript; polls rather than reads once, because `SubAgentNotified` means "handed over", not "recorded". **Exact strings**: `contains("bad key")`, `"[subagent \"risky\" failed]"` |
| 13 | `a_stranded_grandchild_result_flushes_at_the_next_turn_boundary` (tokio, `:744`) | §7.13 and §4.9: with every node terminal, C's result is owed to P forever until a user message triggers the flush; loading must start no runs. **Exact strings**: `"[subagent \"child\" failed]"`, `"interrupted by restart"`, `output == Some("sub answer")` |
| 14 | `recovery_respawns_subagents_and_fails_interrupted_ones` (tokio, `:844`) | `SubAgents::on_load` → `Reconcile` after a restart. **Exact string**: the node's error equals `INTERRUPTED_ERROR` (`"interrupted by restart"`) |
| 15 | `a_workflow_steps_subagent_completion_is_recorded` (tokio, `:890`) | the defect the forest exists to close: a step's spawn belongs to `TreeOwner::Step(step_agent)` and its completion is journaled rather than dropped. **Exact string**: `output == Some("sub answer")` |
| 16 | `a_runs_subagents_count_toward_the_session_wide_aggregates` (tokio, `:919`) | `has_active`, `active_count`, `interrupted` and the roster snapshot all span a run's trees — `has_active` answered `false` for every run before the forest |
| 17 | `a_nested_subagents_result_wakes_its_parent_inside_a_run` (tokio, `:956`) | delivery runs inside a run, not just a conversation: a grandchild's `notified` flips |
| 18 | `the_new_state_shape_round_trips` (`#[test]`, `:987`) | `SessionState` with a populated forest survives a serde round-trip. **Exact string**: `label == "x"` |
