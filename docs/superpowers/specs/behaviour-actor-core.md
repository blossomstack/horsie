# Behaviour of the session actor core, as it is today

A description of what three files do right now, so a rewrite can be checked against something on disk instead of against a re-reading of the code. It is deliberately not a design: nothing here says how any of it should be ported.

The files:

- `crates/server/src/sessions/session_actor/mod.rs` — `SessionActor`, its roster, its spawner, its turn boundary.
- `crates/server/src/sessions/session_actor/hooks.rs` — `SessionParent`, `SessionHookSink`, `StopHookParent`, `HookRouting`.
- `crates/server/src/sessions/session_actor/core.rs` — `SessionCore`, plus the actor-field handlers that live in its `impl SessionActor` block.

Everything else — `turns.rs`, `run.rs`, `subagent.rs`, `fork.rs`, `lifecycle.rs`, `reads.rs`, `context.rs` — is cited only where the three files reach into it.

## 1. What the actor holds

`SessionActor` (mod.rs:205–243) has nine fields:

| field | mod.rs | what it is |
| --- | --- | --- |
| `id: Uuid` | 206 | the session id; also the main agent's journal id |
| `account: crate::auth::UserId` | 211 | deliberately *not* in the persistence id |
| `spec: Option<SessionSpec>` | 215 | `None` until the log says, or until `RecordSpec` is handled |
| `users: Weak<UserRegistry>` | 218 | a shard recipe is synchronous, so the bundle cannot be handed in |
| `services: Option<Arc<UserServices>>` | 220 | resolved in `on_recovery_complete` |
| `supervisor: SupervisorRef` | 229 | a name with a warm cache, not a handle |
| `agents: Option<SessionAgents>` | 235 | `None` means "does not yet know what it is" |
| `last_reported: Option<SessionStatus>` | 239 | `None` at load, so a fresh session always reports once |
| `last_reported_forks: Vec<ForkRow>` | 242 | same, for the fork roster |

Three accessors panic rather than handle `None`, each with an `#[expect(clippy::expect_used)]`:

- `services()` (mod.rs:282) — "a session handles no command before recovery has resolved its account".
- `spec()` (mod.rs:302) and `spec_mut()` (mod.rs:314) — "a session is told what it is before anything else can reach it".

`deps()` (mod.rs:289) is `&self.services().deps`. `me(ctx)` (mod.rs:321) builds a `SessionRef` from `ctx.self_ref()`, the account and the id, with `None` for the fourth field.

`persistence_id_for(session_id)` (mod.rs:269) is `PersistenceId::new("session", session_id.to_string())` — the account is absent on purpose: putting one in now would orphan every log ever written.

### `ResidentAgent` (mod.rs:93–97)

`{ actor: ActorRef<AgentCommand>, provider: Arc<SessionContextProvider> }`. The two travel together because cancelling needs both — the mailbox to stop the loop, the provider to reach the client the in-flight run already acquired.

### `SessionAgents` (mod.rs:103–182)

An enum, not `Option` + map, because "a session's topology is decided at creation and never changes":

- `Interactive { main: ResidentAgent, subs: HashMap<Uuid, ResidentAgent> }`
- `Workflow { live: HashMap<Uuid, ResidentAgent> }` — no main agent at all.

Methods: `interactive` (114), `workflow` (121), `main` (128, `None` for a run), `sub` (135), `get` (143), `insert_sub` (150), `remove_sub` (164, only a fork's delete uses it), `drain_all` (172, used when the session unloads; for `Interactive` it drains `subs` and *clones* `main` onto the end).

`get` (mod.rs:143–148) is the first of `AgentKey`'s jobs: `Main` → `self.main()`; `Sub(id) | Step(id) | Fork(id)` → `self.sub(id)`. **Three variants collapse into one map.** The consequence is called out in `resolve_agent` (mod.rs:650–652): "the roster cannot say what *kind* of agent an id names — forks and subagents share one map. Answering `Sub` for a fork made a fork of a fork read as a fork of a subagent, and be refused."

### `AgentPlan` (mod.rs:189–203)

Everything that differs between the three (four) kinds of agent: `kind: SessionAgentKind`, `settings: AgentSettings`, `equipment: Capabilities`, `agent_type: Option<String>`. Everything identical — runtime provider, plugin library, MCP, memory, the session's mailbox — lives on the actor, which is why one spawner serves all.

### Constants

- `MAIN_AGENT_ID = "main"` (mod.rs:79) — the path segment and usage key.
- `CANCEL_TIMEOUT = 5s` (mod.rs:84) — "a backstop so a wedged run can never hold the mailbox — and with it the Stop button — hostage".
- `MAX_STOP_CONTINUATIONS = 3` (hooks.rs:118).

## 2. The actor's machinery, function by function

### `spawn_agent(&mut self, ctx, state, plan) -> Option<ResidentAgent>` — mod.rs:463

The single spawner. Cheap and runtime-free: provider, toolbox and system prompt resolve per run on the run's own task.

In order:

1. `revisions = self.services().revisions.of(&self.id.to_string())` (472) — taken from the account's registry, not owned here, so channels outlive the actor and unloading an idle session leaves a reader waiting rather than disconnecting it.
2. `(journal_id, revision)` (481–486) — `Main` → `(self.id, revisions.publishing(MAIN_AGENT_ID))`; `Sub|Step|Fork(id)` → `(id, revisions.publishing(&id.to_string()))`. `publishing` and not `for_agent`, because this node runs the agent and the reader is routinely on another node.
3. `name` (488–493) — `"main"` or the uuid string. This is the actor path segment.
4. `key = plan.kind.agent_key()` (494).
5. `loading = context::loading_for(...)` (498–528). The `RuntimeProvider` gets: the session id string; `state.provisioned_at_ms.map(to_string).unwrap_or_default()` — the empty string is what a later acquisition fails on "rather than silently addressing some other sandbox"; `matches!(state.status, SessionStatus::Provisioning)` so a substrate that has not reported the object yet is distinguishable from one with nothing there; the vendor; the whole spec.
6. `provider = Arc::new(SessionContextProvider { loading, equipment, kind, agent_type, plugins, settings })` (529–536).
7. `params` from `AgentRunDef` (537–551): `system_prompt: None`, `max_iterations`, `max_retries`, `allowed_tools` from settings; then `interactive = true`; `requires_result = matches!(plan.kind, Step(_))` — "a step is the only agent that owes a structured result"; `thinking_effort` parsed from the settings string.
8. `AgentRuntimeContext` (552–562): `parent: StopHookParent::wrap(self.me(ctx), key, provider.clone())`, `journal_id`, and `ready: Self::runnable(state)` — computed from the state this spawn was decided against and never remembered.
9. `ctx.actor_of(&name, ctx.persistent(AgentActor::new(agent_ctx, params)))` (568). On `Err`: `tracing::error!` and `return None`. A named child, "so it has a path under this session's, which is what makes it stop with the session and makes two callers racing to reach one agent get one actor over its journal".
10. Registration (576–585): `Main` → `self.agents = Some(SessionAgents::interactive(resident))`; `Sub|Step|Fork(id)` → `if let Some(agents) = self.agents.as_mut() { agents.insert_sub(id, resident) }`.

**Journals nothing.** Two guards worth naming:

- The `Main` arm **replaces the whole roster**, discarding any existing `subs`. Only `spawn_main_agent` reaches it, and only from `adopt`, where `agents` is `None` — but the write is unconditional.
- The `Sub|Step|Fork` arm silently skips registration when `self.agents` is `None` and **still returns `Some(resident)`**. The caller gets a live actor the roster does not know about.

### `spawn_main_agent(&mut self, ctx, state)` — mod.rs:590

`self.spec().agent_settings().cloned()` or return (the comment notes `adopt` already gates on the kind). Assembles `RunnerKind::Conversation` equipment with `unattended: self.spec().is_unattended()`, `fork: None`, `agent_type: None`, then `spawn_agent` with `SessionAgentKind::Main`. Return value discarded. Journals nothing.

### `resolve_agent(&mut self, state, ctx, agent_id) -> Option<(AgentKey, ActorRef<AgentCommand>)>` — mod.rs:628

Resolution order matters and is commented:

- `None | Some("main")` (635): if `state.run` has a `current_agent`, `resolve_step` on it — "at most one step runs at a time, and the definition chose it". Otherwise `self.agent().map(|a| (AgentKey::Main, a))`. Without the run branch, "everything a caller can leave unaddressed — an answer above all — resolved to nothing on a run and silently did nothing".
- `Some(raw)` (644): parse as uuid or `None`. Then, in this exact order:
  1. `resolve_step` (646) — the run log identifies a step, not the roster.
  2. `state.forks.contains(id)` → `spawn_fork_actor` → `AgentKey::Fork(id)` (653–657). **Before** the roster, for the "fork of a fork" reason above.
  3. resident sub → `AgentKey::Sub(id)` (658).
  4. cold node: `state.subagents.node(id)?.agent_type.clone()`, then `spawn_sub_agent_actor` → `AgentKey::Sub(id)` (663–667). "The type comes off the record, not from the caller: a cold node woken to answer a read must run as what it was spawned as."

Journals nothing. May spawn actors as a side effect of a *read*, which is what makes a finished agent readable.

### `resolve_step(&mut self, state, ctx, id) -> Option<(AgentKey, ActorRef)>` — mod.rs:680

`state.run.as_ref()?` → `run.index_of_agent(id)?` → resident sub if there is one → else `run.get(index)?.step.clone()` and `spawn_step_agent`. Always answers `AgentKey::Step(id)`.

The log and not the roster identifies a step, because "a run's step subagents are registered in the same roster, so residency alone cannot tell the two apart". Spawning on demand is what keeps a finished run's step transcripts readable after a reload.

### `agent(&self) -> Option<ActorRef>` — mod.rs:698

`self.agents.as_ref().and_then(SessionAgents::main).map(|a| a.actor.clone())`.

### `reach(&mut self, key, state, ctx) -> Option<ActorRef>` — mod.rs:855

Resident first (861). Then by key:

- `Sub(id)` (868) — cold node: type off the record, `spawn_sub_agent_actor`.
- `Step(id)` (875) — `resolve_step(...).map(|(_, actor)| actor)`; "a boundary can owe a result to a step whose actor has since been unloaded".
- `Fork(id)` (880) — `state.forks.contains(id).then(|| self.spawn_fork_actor(...))?`. Nothing comes off a record: "a fork runs under the session's own settings, like the conversation it branched from".
- `Main` (886) — `None`. "Spawned at load, so it is either resident or this session is a run and has none."

Journals nothing.

### `effective_settings(&self, state, key) -> Option<&AgentSettings>` — mod.rs:709

- `Main | Fork(_)` → `self.spec().agent_settings()`.
- `Step(id)` → `self.spec().workflow_run()?` → `state.run.as_ref()?.index_of_agent(id)?` → `state.run.as_ref()?.get(execution)?.step` (the name) → `run.step(name).map(|s| &s.settings)`.
- `Sub(id)` → `state.subagents.owner_of(id)?`: `TreeOwner::Main` → the session's settings; `TreeOwner::Step(agent)` → **recurses** into `effective_settings(state, AgentKey::Step(agent))`.

`effective_settings_for_parent(&self, state, caller: SubAgentParent)` (mod.rs:733) is the spawn-side twin: `SubAgentParent::Main` → `state.root_owner()` (`Main` or the in-flight `Step`); `SubAgentParent::SubAgent(id)` → `Sub(id)`. So "a step's spawns inherit the step's settings and its cap".

### `cancel_agent(&mut self, key)` — mod.rs:757

Two halves, in this order:

1. `self.agents...get(key).cloned()` or **return silently** — a key naming no resident agent is a no-op, and nothing is spawned to cancel.
2. `if let Some(client) = agent.provider.cached_client() { client.cancel_in_flight().await }` — the *cached* client, because "asking the manager for a fresh one would round-trip the vendor on this mailbox, and a vendor mid-tool-call cannot answer a lifecycle request until the call it is relaying resolves".
3. `AgentCommand::Cancel { ack: Some(...) }`, then `tokio::time::timeout(CANCEL_TIMEOUT, rx)`. On timeout: `tracing::warn!` and proceed.

Waiting matters because "the caller is about to record a turn boundary, and a run still winding down can still append to the agent journal". Journals nothing.

### `cancel_in_flight(&mut self, state)` — mod.rs:782

`state.run.as_ref().and_then(|r| r.current().and_then(|i| r.get(i))).map(|s| s.agent)`; `Some(agent)` → `cancel_agent(Step(agent))`, `None` → `cancel_agent(Main)`. "A run used to be skipped here entirely, so deleting one mid-step left its sandbox call running." Called from `LifecycleCommand::Delete` (lifecycle.rs:169) and nowhere else.

### `stop_agents(&mut self)` — mod.rs:1079

`self.agents.take()` or return. For each of `drain_all()`: `AgentCommand::Cancel { ack: None }` then `AgentCommand::Shutdown`. **Does not wait** for either. "Cancel first: a stopped mailbox makes the run task's next persist fail, but an in-flight tool call would run to completion first." Called from `PrepareOffload` (lifecycle.rs:156) and `Delete` (lifecycle.rs:170). Journals nothing.

### `busy(&self, state) -> bool` — mod.rs:900

`RuntimeLifecycle::busy || Turns::busy || WorkflowRun::busy || SubAgents::busy`. **`ForkedAgents::busy` is not in the list** — see §5.

- `RuntimeLifecycle::busy` (lifecycle.rs:196) — `status == Provisioning`.
- `Turns::busy` (turns.rs:535) — `status == Running`.
- `WorkflowRun::busy` (run.rs:387) — `run.current().is_some()`.
- `SubAgents::busy` (subagent.rs:304) — `state.subagents.has_active()`.

### `next_actions(&self, state) -> Vec<AgentAction>` — mod.rs:913

1. **The gate**: `if !RuntimeLifecycle::ready(state) { return Vec::new() }`. `ready` (lifecycle.rs:33) is `!matches!(status, Provisioning | ProvisioningFailed { .. })`. "One gate, checked once, for every component." Note this suppresses *deliveries* too, not just new turns.
2. `cx = ActionCx { id: self.id, spec: self.spec() }`.
3. `[SubAgents::actions(&cx, state), Turns::actions(&cx, state), WorkflowRun::actions(&cx, state)].concat()` — a fixed order, "a concatenation, not a negotiation". Subagent wakes go first because "a parent waiting on its children is work already in flight, and the next turn or step can wait a boundary".

What each returns:

- `SubAgents::actions` (subagent.rs:291) → `orchestrator::owed_deliveries(state)` — every result a child owes a parent that the parent has not been sent, unconditional on the recipient's state.
- `Turns::actions` (turns.rs:512) → **always empty**. "A conversation's turns are the agent's own decision now."
- `WorkflowRun::actions` (run.rs:354) → `cx.spec.workflow_run().cloned()` or empty, then `WorkflowOrchestrator::new(cx.id, run_spec).step_actions(state)`.

`ForkedAgents` has no `actions` impl (default empty). `SessionCore` has none. `Reads` does not implement `Component` at all.

`AgentAction` (orchestrator.rs:26–36) is `StartStep(StepStart) | Finish { output } | Fail { error } | Deliver(Delivery)`.

Exposed to tests as `testing::decisions(actor, state)` (testing.rs:38–40).

### `perform(&mut self, action, state, ctx) -> Vec<SessionDomainEvent>` — mod.rs:799

A four-way dispatch, no guards of its own:

| action | goes to | events on success | events on failure |
| --- | --- | --- | --- |
| `Deliver(d)` | `self.deliver` (mod.rs:824) | `SubAgentNotified` | `[]` |
| `StartStep(s)` | `self.start_step` (run.rs:68) | `StepStarted` | `RunFailed` |
| `Finish { output }` | `self.finish_run` (run.rs:124) | `RunFinished` | — |
| `Fail { error }` | `self.fail_run` (run.rs:132) | `RunFailed` | — |

"No turn ever begins here any more: an agent owns its queue and decides when that queue becomes a turn."

### `deliver(&mut self, delivery, state, ctx) -> Vec<SessionDomainEvent>` — mod.rs:824

`Delivery { to, child, part }`. `self.reach(to, state, ctx)` — **may spawn a cold agent**. If `None`, returns `[]`: "skipped, not failed — the result stays owed and the next boundary tries again". Then `AgentCommand::Enqueue { item: Incoming::SubAgent { id: child.to_string(), part }, ack: None }`; on send error, `[]`. On success, one `SubAgentNotified { at_ms: now_ms(), id: child }`.

The ordering is explicit in the doc comment: **tell-then-persist**. "A crash between the enqueue and this write leaves the result still owed, so the next boundary re-delivers it. Delivery is at-least-once in that window (the parent may see a result twice), never lost."

### `flush_then_drain(&mut self, state, ctx) -> Vec<SessionDomainEvent>` — mod.rs:931

```rust
let mut events = Vec::new();
let mut next = state.clone();
for action in self.next_actions(&next) {
    let produced = self.perform(action, &next, ctx).await;
    for e in &produced { next = Self::apply_event(next, e.clone()); }
    events.extend(produced);
}
events
```

Two facts about this loop:

- Each `perform` sees the state the previous one produced, because `next` is re-folded in place.
- **`next_actions` is evaluated exactly once**, before the loop. The action *list* is a snapshot taken against the entry state; work that becomes startable partway through the loop is not picked up until the next boundary.

It **persists nothing**. It returns events for a caller to persist. Three callers:

- `persist_and_advance` (mod.rs:966).
- `LifecycleCommand::FinishProvisioning` (lifecycle.rs:144) — folds the provisioning event locally first, then drains, then `CommandEffect::persist(events)`. "The runtime landed, so whatever queued behind it starts now. A failure drains nothing."
- `RunCommand::Advance` (run.rs:46) — `CommandEffect::persist(actor.flush_then_drain(state, ctx).await)`, with no prior events.

### `persist_and_advance(&mut self, state, events, ctx) -> CommandEffect<SessionDomainEvent>` — mod.rs:956

```rust
let next = events.iter().cloned().fold(state.clone(), Self::apply_event);
events.extend(self.flush_then_drain(&next, ctx).await);
CommandEffect::persist(events)
```

"The fold is local and one step early — the same fold the runtime will apply when it persists — because the drain has to see the state these events produce, not the one they were decided against."

### `runnable(state) -> bool` — mod.rs:1073

`RuntimeLifecycle::ready(state) && !matches!(state.status, SessionStatus::Unrecoverable { .. })`. An associated function, used only for `AgentRuntimeContext::ready` at spawn (mod.rs:561). Note it is a *different* predicate from the `next_actions` gate: `next_actions` checks `ready` alone and does not exclude `Unrecoverable`.

### `report_status(&mut self, state)` — mod.rs:441

Returns early if `self.last_reported.as_ref() == Some(&state.status)`. Otherwise stores and `supervisor.tell(SessionStatusChanged { id, status })`.

### `report_forks(&mut self, state)` — mod.rs:409

1. Return if `state.forks.is_empty() && self.last_reported_forks.is_empty()`.
2. Build `Vec<ForkRow>` from `state.forks`, mapping `ForkParent::Main → None` and `ForkParent::Fork(pid) → Some(pid)`, carrying `title`, `status`, `created_at_ms`, `last_activity_ms`.
3. Return if `forks == self.last_reported_forks`.
4. Store and `supervisor.tell(ForksChanged { id, forks })`.

"The whole roster every time... A projection built from the current value cannot drift the way one built from deltas can — and `List` is documented to load nothing."

Journals nothing. Both reporters are called only from `on_events_persisted` and once from `adopt`.

## 3. The turn boundary, in precise order

This is the load-bearing ordering. For a command that goes through `persist_and_advance(state, events, ctx)`:

1. **Fold locally.** `next = events.fold(state.clone(), apply_event)` (mod.rs:962–965). Nothing is durable yet.
2. **Gate.** `next_actions(&next)` returns `[]` immediately unless `RuntimeLifecycle::ready(&next)` (mod.rs:916).
3. **Collect once.** `SubAgents::actions ++ Turns::actions ++ WorkflowRun::actions`, in that order, against `next` (mod.rs:923–928). The list is fixed here.
4. **Perform in order.** For each action, `perform` runs — which *sends messages to agent mailboxes and spawns actors* — and each produced event is folded into the running `next` before the following action is performed (mod.rs:938–944).
5. **Return.** `CommandEffect::persist(events)` with the original events plus everything the drain produced (mod.rs:966–967).
6. **The runtime persists.** Only now does anything become durable, and the runtime applies the same fold a second time.
7. **`on_events_persisted`** (mod.rs:1152) runs, in this order: `record_lifecycle(events, state)`, `report_forks(state)`, `report_status(state)`.

So: **every side effect of a boundary happens before any of it is durable.** `deliver`'s tell-then-persist is the named consequence — at-least-once, never lost. `spawn_agent` is called the strict opposite way round elsewhere and the comment at mod.rs:819–820 says so: "`spawn_agent`'s stricter persist-then-spawn is the deliberate exception, because an untracked agent is worse than a duplicate." (Strictly, the persist-then-spawn is in `SubAgentCommand::FinishSpawn` / `ForkCommand::FinishCreate`, not inside `spawn_agent` itself; the comment names the path, not the function.)

Call sites of `persist_and_advance`, i.e. every place a turn boundary is declared:

| site | events handed in |
| --- | --- |
| turns.rs:114 | the `stop_boundary` event for a `Stop` |
| turns.rs:292 | `on_main_outcome`'s event list |
| turns.rs:504 | **`Vec::new()`** — a user message flushes the boundary with nothing of its own |
| subagent.rs:241 | the subagent's terminal event |
| run.rs:267 | `StepConcluded` only (the `advance == true` arm) |
| fork.rs:138 | a fork command's events |
| fork.rs:298 | `ForkStatusChanged { AwaitingInput }` |
| fork.rs:317 | `SessionFailed` |
| fork.rs:348 | `ForkTurnEnded` |

The empty-event call at turns.rs:504 is the point of the design: "A person acting is the boundary that flushes results owed to subagent parents. Those strand once every node is terminal — no further subagent outcome will arrive to trigger the flush."

### `on_events_persisted` and `record_lifecycle`

`record_lifecycle` (core.rs:160) loops over the persisted events, calls `lifecycle_routing::route(event, state)` for each, and for every `(key, payload)` tells that agent `AgentCommand::RecordLifecycle { event: payload, at_ms: now_ms() }`.

Two properties, both documented:

- **Resident agents only.** There is no `ActorContext` in this hook, so nothing can be spawned. A miss logs `tracing::warn!("no resident agent to record a session event on; it will be missing from the log")` and continues. The comment argues a miss is a bug rather than a case: main is resident for the session's loaded life, a step agent is live while its step is, and every subagent-targeted event happens while that subagent runs.
- **It routes against the batch's *final* state.** `on_events_persisted` is called once per batch with the state the whole batch folded to, "so an event is placed by where the batch ended rather than by where it itself sat".

`record_on(key, event)` (core.rs:186) is the single-entry version: resident-only, no warning, used by `CoreCommand::Progress`.

## 4. `adopt` and recovery

### The two callers

`adopt` (mod.rs:333) is reached from exactly two places, and the pair "is the whole of how a session learns what it is":

- `on_recovery_complete` (mod.rs:1192) — resolves `self.services` first, then `let Some(spec) = state.spec.clone() else { return }` and `adopt`. A session with an empty log deliberately returns without adopting: "the `RecordSpec` that brought this actor into being is next in this mailbox... Writing it from here instead would race that command, and a rename arriving first would have nothing to rename."
- `CoreCommand::RecordSpec` (core.rs:65) — idempotent on `state.spec.is_some()`, then `adopt((*spec).clone(), state, ctx)` and `CommandEffect::persist([SpecRecorded])`.

"Both go through here so that a run started for the first time and one resumed after a restart take exactly the same path."

### What `adopt` does, in order

1. `self.spec = Some(spec)` (339).
2. Topology by kind (340–348): `SessionKind::Workflow { .. }` → `self.agents = Some(SessionAgents::workflow())` and **no agent is spawned** ("step actors, like subagent actors, stay cold: they spawn on demand for a history read, a retry, or the next step a boundary picks"); `SessionKind::Agent { .. }` → `self.spawn_main_agent(ctx, state)`.
3. Build `ActionCx { id, spec }` (354).
4. Collect repairs (358–366), in this fixed order, flattening the `Option`s:
   1. `RuntimeLifecycle::on_load`
   2. `SubAgents::on_load`
   3. `WorkflowRun::on_load`
   4. `Turns::on_load`
5. `for cmd in repairs { me.tell(cmd).await }` (368–370) — a self-send, "because neither caller may write here — recovery must not persist at all, and `RecordSpec` is already returning an effect of its own".
6. `self.report_status(state).await` (381) — **unconditional**, repairs or not.

Step 6 carries a regression note in the comment: "This used to be skipped whenever a repair was queued, on the grounds that the repair reports the status it lands on — but `SubAgentCommand::Reconcile` persists an event and reports nothing, so a session whose only repair was an interrupted subagent loaded and said nothing at all."

`adopt` does **not** call `report_forks`.

### The four wired repairs

| component | file:line | condition | command |
| --- | --- | --- | --- |
| `RuntimeLifecycle::on_load` | lifecycle.rs:188 | `status` is `Provisioning` or `ProvisioningFailed { .. }` | `Lifecycle(Provision)` |
| `SubAgents::on_load` | subagent.rs:297 | `!state.subagents.interrupted().is_empty()` | `SubAgent(Reconcile)` |
| `WorkflowRun::on_load` | run.rs:373 | `spec.workflow_run()` is `Some`, then: no run state → `Advance`; `run.current().is_some()` → `ReconcileInterrupted`; `run.status == Pending` → `Advance`; else `None` | `Run(Advance)` / `Run(ReconcileInterrupted)` |
| `Turns::on_load` | turns.rs:528 | — | **always `None`** |

`RuntimeLifecycle::on_load` re-attempts a create because "`Provisioning` is precisely the state in which no turn has run, so there is no work in the workspace for a rebuild to destroy".

`WorkflowRun::on_load` is gated on the *spec*, not the state: "a conversation also has no run state, and reading only the state would advance one — which, for a session holding a subagent result nobody has collected, silently starts a turn at load".

`Turns::on_load` returning `None` is deliberate and carries the longest justification in the file (turns.rs:516–527): it used to self-send a reconcile that asked "is the session `Running`?", "a question the session cannot answer about a *turn*, since a self-send queues behind everything the supervisor sent while the actor was loading. A message, an answer or a flushed subagent result handled first could start a real turn, and the reconcile then recorded *that* one as interrupted." An interrupted turn is now reported by the agent as `AgentOutcome::Interrupted`.

### The `on_load` that exists and is NOT wired

**`ForkedAgents::on_load` (fork.rs:217) is never called.** `adopt`'s array (mod.rs:358–363) lists four components and `ForkedAgents` is not one of them.

```rust
fn on_load(_cx: &ActionCx<'_>, state: &SessionState) -> Option<SessionCommand> {
    state
        .forks
        .has_seeding()
        .then_some(SessionCommand::Fork(ForkCommand::ReseedInterrupted))
}
```

`ForkCommand::ReseedInterrupted` has a handler (fork.rs:193) and a unit test (fork.rs:690–695) but **no producer in the running system** — a grep for `ReseedInterrupted` finds only the variant, the handler, this `on_load`, and the test.

Two comments assert the opposite:

- types.rs:556–558 — "a crash between the two replays as a fork still `Provisioning`, which `ForkedAgents::on_load` re-seeds."
- fork.rs:12 — "which [`ForkedAgents::on_load`] re-seeds — strictly better than an untracked agent".

**`ForkedAgents::busy` (fork.rs:227) is likewise not in `SessionActor::busy`** (mod.rs:900). Its own doc says "A summariser call is provider time with nothing durable behind it. Unloading the session mid-seed loses it and leaves a fork that only a reload repairs" — and fork.rs:507 says "[`ForkedAgents::busy`] is what keeps the session loaded". It is unit-tested at fork.rs:673–677. It is never asked. So today a session mid-seed can be offloaded, and the reload that would repair it does not run `ForkedAgents::on_load` either.

This directly contradicts the `Component::busy` doc (component.rs:89–93): "Asked of every component and OR-ed together — which is what lets a component added later make itself heard, where today's single hand-written condition could not." `ForkedAgents` is the component added later, and it is not heard.

### Other `Component` gaps

- `SessionCore` (core.rs:204) implements `apply` only — no `on_load`, no `busy`, no `actions`.
- `Reads` does not implement `Component`.
- `ForkedAgents` has no `actions`.
- `Turns::actions` (turns.rs:512) is a hand-written `Vec::new()`, not the trait default.

## 5. The outcome path

`SessionCommand::AgentOutcome` is dispatched at mod.rs:1178 — "the one command routed by identity rather than by variant".

### `on_agent_outcome(&mut self, state, outcome, ctx)` — mod.rs:978

First, `TurnEnd::split(outcome)` (types.rs:662–684):

| `AgentOutcome` | → |
| --- | --- |
| `Concluded { agent, output }` | `Ok((agent, TurnEnd::Concluded { output }))` |
| `Asked { agent, .. }` | `Ok((agent, TurnEnd::Asked))` |
| `Parked { agent }` | `Ok((agent, TurnEnd::Parked))` |
| `Interrupted { agent }` | `Ok((agent, TurnEnd::Interrupted))` |
| `Failed { agent, error, terminal, .. }` | `Ok((agent, TurnEnd::Failed { error, terminal }))` |
| `UsageRecorded { agent, usage_total }` | `Err((agent, NotAnEnd::Usage(usage_total)))` |
| `Started { agent }` | `Err((agent, NotAnEnd::Started))` |
| `ForkSummary { agent, forks, result }` | `Err((agent, NotAnEnd::ForkSummary { forks, result }))` |

A `Result` and not an `Option` "so the caller cannot reach the routing path with a non-ending outcome still in hand".

The three `Err` arms are answered first and never reach routing:

- **`NotAnEnd::Usage`** (mod.rs:990–1000) — `agent_id = if agent == self.id { "main" } else { agent.to_string() }`, then `CommandEffect::persist([UsageRecorded { at_ms, agent_id, usage_total }])`. "Usage is banked for every agent alike, and always: the tokens were spent whatever became of the turn that spent them." No boundary, no drain.
- **`NotAnEnd::Started`** (mod.rs:1001) — `self.on_agent_started(state, agent)`.
- **`NotAnEnd::ForkSummary`** (mod.rs:1007) — `ForkedAgents::handle(self, state, ForkCommand::Summarised { forks, result }, ctx)`. "A summary taken for somebody else. Not this agent's turn ending — it may still be running."

Then routing by identity, in this order (mod.rs:1017–1033):

1. **`state.run.is_some()`** → `run.index_of_agent(who)`: `Some(index)` → `on_step_outcome(state, index, end, ctx)`; `None` → `on_sub_agent_outcome(state, who, end, ctx)`. "In a run, an outcome is a step's or one of a step's subagents'."
2. **`who == self.id`** → `on_main_outcome(state, end, ctx)`.
3. **`state.forks.contains(who)`** → `on_fork_outcome(state, who, end, ctx)`. Explicitly **before** the subagent forest: "asked last, every one of a fork's turns would be dropped as an outcome from an agent nothing recognises."
4. else → `on_sub_agent_outcome(state, who, end, ctx)`.

### `on_agent_started(&mut self, state, who)` — mod.rs:1043

| condition | persists |
| --- | --- |
| `who == self.id` | `[TurnBegan { at_ms }]` |
| `state.subagents.node(who).is_some()` | `[SubAgentRunning { at_ms, id: who }]` |
| `state.forks.contains(who)` | `[ForkStatusChanged { at_ms, id: who, status: AgentStatus::Running }]` |
| otherwise | `CommandEffect::none()` |

A step falls through to `none()`: "A step announces itself through `StepStarted` when the run picks it, so there is nothing to add here." Note this check runs *before* the run branch of `on_agent_outcome`, so a step's `Started` is answered here, not by `WorkflowRun`.

The fork arm is scoped deliberately: "a fork's own status, and only its own: the session's belongs to the main agent, and a fork answering a question is not the session working."

No branch here goes through `persist_and_advance` — none of them is a boundary.

### `on_main_outcome` — turns.rs:236

| `TurnEnd` | events |
| --- | --- |
| `Concluded { .. }` | `[TurnEnded]` (output discarded) |
| `Asked` | `[AskRecorded]` |
| `Interrupted` **and** `state.status == Running` | `[TurnInterrupted]` |
| `Interrupted` otherwise | `return CommandEffect::none()` |
| `Failed { error, terminal: true }` | `[SessionFailed { reason: error }]` |
| `Failed { error, terminal: false }` | `[TurnFailed { error }]` |
| `Parked` | `[TurnFailed { error: "agent parked; timers are not supported in sessions" }]` |

Then `persist_and_advance`. The `Interrupted` guard: "a turn that failed before the loop began — abandoned by a start hook, or a context that would not build — never banked a boundary there, so the agent still calls it open while the session, which was told directly, has already recorded `TurnFailed`."

### `on_step_outcome` — run.rs:215

Returns `(events, advance)`:

| `TurnEnd` | events | advance |
| --- | --- | --- |
| `Concluded { output }` | `[StepConcluded { index, output }]` | `true` |
| `Asked` | `[AskRecorded]` | `false` — the step is parked, "nothing else starts meanwhile" |
| `Failed { error, .. }` (terminal or not) | `[StepFailed { index, error }]` | `false` — "a step that fails fails the run, terminal or not" |
| `Parked` | `[]` | `false` — the step stays running; "this used to fail the run outright, which made a step that suspended itself deliberately indistinguishable from one that crashed" |
| `Interrupted` | `return CommandEffect::none()` | — |

`advance` chooses `persist_and_advance` vs a bare `CommandEffect::persist`. So a step's `Concluded` is the only step end that opens a boundary.

### `on_sub_agent_outcome` — subagent.rs:196

Guard first: `state.subagents.node(id).is_none()` → `tracing::warn!("outcome from an unknown subagent; ignored")` and `none()`.

| `TurnEnd` | event |
| --- | --- |
| `Concluded { output }` | `SubAgentCompleted { output: output.as_str().unwrap_or(output.to_string()) }` |
| `Failed { error, .. }` | `SubAgentFailed { error }` |
| `Asked` | `SubAgentFailed { error: "subagent asked the user; not supported" }` — "defensive: a subagent has no ask or timer tools" |
| `Parked` | `SubAgentFailed { error: "subagent parked; timers are not supported in sessions" }` |
| `Interrupted` | `return CommandEffect::none()` |

Then `persist_and_advance`. `terminal` is discarded for a subagent.

### `on_fork_outcome` — fork.rs:284

"A fork *is* a conversation, so there is no end the main agent can reach that a fork cannot. What differs is only the scope."

| `TurnEnd` | result |
| --- | --- |
| `Concluded { .. }` | falls through to `ForkTurnEnded { outcome: TurnOutcome::Ended }` |
| `Asked` | early `persist_and_advance([ForkStatusChanged { AwaitingInput }])` — not a boundary event |
| `Failed { terminal: true }` | early `persist_and_advance([SessionFailed { reason: error }])` — **session-wide**: "forks share the one runtime, so a runtime that cannot be rebuilt takes every conversation in the session with it" |
| `Failed { terminal: false }` | `ForkTurnEnded { outcome: TurnOutcome::Failed { error } }` |
| `Parked` | `ForkTurnEnded { outcome: Failed { error: "agent parked; timers are not supported in sessions" } }` |
| `Interrupted` | only if `state.forks.get(id)?.status == Running`, else `none()`; then `ForkTurnEnded { outcome: Interrupted }` |

The fork's status is *derived* from `ForkTurnEnded`'s outcome in the fold (fork.rs:257–265): `Failed → AgentStatus::Failed`, everything else → `Idle`. "A second field saying so is a second thing that can disagree with the first."

## 6. Hooks

### `SessionParent` — hooks.rs:39

`{ target: SessionRef }`. Its whole `AgentOutcomeSink::deliver` (hooks.rs:51) is `target.tell(SessionCommand::AgentOutcome(outcome))`, error dropped. "No generation tag: the agent is resident and fences its own stale runs by `run_id`, so every outcome that arrives here is one the session asked for."

It is never constructed directly by the actor — only inside `StopHookParent::wrap` (hooks.rs:151).

### `SessionHookSink` — hooks.rs:63

`{ target: SessionRef, key: AgentKey }`. Implements `horsie_runtime_host::HookSink`. Constructed at runtime.rs:331 with `loading.key`, and attached to the client the runtime capability acquires — which is why `mod.rs:30–33` makes `hooks` `pub(crate)`.

`record(hooks)` (hooks.rs:79):

1. `let halt = tool_halt_reason(&hooks)` — computed **before** the forward.
2. `tell(Hooks(HookCommand::Ran { key, records: hooks }))`.
3. If `Some(reason)`, `tell(Hooks(HookCommand::Halt { key, reason }))` — "after the records, so the transcript shows what halted the turn above the turn's own failure".

`tool_halt_reason` (hooks.rs:263) filters on `is_tool_seam` before `halt_of`. This narrowing is the fix for a real bug: server-initiated events' records travel this sink *and* are returned to the seam that fired them, "and each of those seams reads the halt off its own return value: `start_blocked` at the pre-run seam, `StopHookParent` at the stop seam. Acting on them here too would halt the same agent twice, and on `Stop` it would fail a turn the stop seam is deliberately ending cleanly."

`is_tool_seam` (hooks.rs:285) is an exhaustive match with no `_` arm. `true` for `PreToolUse | PostToolUse | PostToolUseFailure | PostToolBatch`; `false` for all fifteen others (`SessionStart`, `SessionEnd`, `UserPromptSubmit`, `UserPromptExpansion`, `Stop`, `StopFailure`, `SubagentStart`, `SubagentStop`, `TaskCreated`, `TaskCompleted`, `Notification`, `PreCompact`, `PostCompact`, `CwdChanged`). "Listed rather than `_`: a newly wired event must be classified here deliberately, because a server event misfiled as a tool one is halted twice."

`halt_of` (hooks.rs:272) reads `record.halt`, falling back to the literal `"a hook set continue: false"`.

### `StopHookParent` — hooks.rs:127

`{ inner: Arc<dyn AgentOutcomeSink>, session: SessionRef, key: AgentKey, provider: Arc<SessionContextProvider>, continuations: Arc<AtomicUsize> }`.

`wrap(session, key, provider)` (hooks.rs:145) puts a `SessionParent` in `inner` and starts `continuations` at 0. It is the *only* thing the actor hands to an agent as its parent (mod.rs:555) — every agent a session hosts is wrapped, main, sub, step and fork alike.

Why a decorator and not a branch in the session's handler: "`deliver` is called from the *agent's* `RunFinished` handler. A slow hook therefore delays that agent's own mailbox and never the session's command loop, which stays able to serve a cancel or another agent while a 30-second `Stop` hook runs."

`deliver(outcome)` (hooks.rs:162), in order:

1. **Not `Concluded`** → straight to `inner.deliver`. "An ask or a park is a turn still in progress, and a failure is not a stop the hook could act on."
2. **No plugins or no cached client** → `self.provider.use_plugins().then(|| self.provider.cached_client()).flatten()`; on `None`, straight to `inner`. "Nothing declared a hook, so the round-trip would be pure latency on every single turn." `Stop` never acquires a runtime of its own: "a turn that already concluded must not be able to fail on provisioning".
3. `used = continuations.load()`; `last_assistant_message = output.as_str()`; `stop_hook_active = used > 0` — "the spec's own definition: true when horsie would normally stop but is being held in the loop by a blocking hook".
4. **Event by kind** (hooks.rs:188–204): `SessionAgentKind::Sub(id)` → `ServerHookEvent::SubagentStop(SubagentStopInput { agent_id, agent_type, last_assistant_message, stop_hook_active })`; `Main | Step | Fork` → `ServerHookEvent::Stop(StopInput { .. })`. A step keeps `Stop` because "it fires `SessionStart` and roots its own subagent tree, so answering `SubagentStop` would contradict its own start." The gate exists because "until it was gated on the kind a session with four subagents fired `Stop` five times."
5. `records = client.run_hooks(event).await.unwrap_or_default()` — a failed hook run is an empty batch.
6. **Halt outranks block** (hooks.rs:213): `halt_reason(&records)` (unfiltered, all seams) → reset `continuations` to 0, `tracing::info!("a stop hook set continue: false; the turn ends")`, `inner.deliver(outcome)`. "This ends the turn the way an unblocked one ends — the records are already on their way to the transcript, `run_hooks` having put them on the sink, and there is nothing to add to them the way `CapReached` adds to a block."
7. Else `stop_verdict(&records)`:
   - `Some(reason)` **and** `used < MAX_STOP_CONTINUATIONS` → `continuations.fetch_add(1)`, `tell(Hooks(ContinueAfterStop { key, reason }))`, and **`inner.deliver` is not called**. "The parent never hears about it, so the session never marks the turn done and never drains its queue early."
   - `Some(_)` out of budget → reset to 0, `tell(Hooks(Ran { key, records: cap_reached(records) }))`, then `inner.deliver(outcome)`. "A second record says why — otherwise this reads as a turn that stopped on its own."
   - `None` → reset to 0, `inner.deliver(outcome)`.

`continuations` resets on **every** non-continuing path, so "a long interactive session that legitimately continues a few times never accumulates toward the cap".

`stop_verdict` (hooks.rs:316) is exhaustive over `HookAction`. `Stop(s)` with `StopOutcome::Blocked(b)` → `b.reason` or `"a Stop hook asked for another iteration"`; `SubagentStop(s)` with `SubagentStopOutcome::Blocked(b)` → `b.reason` or `"a SubagentStop hook asked for another iteration"`. `Ran | Failed | CapReached` on either → `None`: "a stop hook runs after the fact, so a guard that could not run cannot deny anything. Only `PreToolUse` fails closed." Every other action → `None`, listed rather than `_`, "so a future event that can hold a turn open must fail to compile here".

`cap_reached` (hooks.rs:366) rewrites `Blocked → CapReached` on `Stop` and `SubagentStop` only, leaving every other action untouched. "The only place `CapReached` is produced: `HookInvocation::record` sees one hook's reply and cannot know the budget."

### `MAX_STOP_CONTINUATIONS` — hooks.rs:118

`3`. "Not advisory. horsie runs unattended sessions, and `stop_hook_active` only stops a hook that reads it — this exists for the ones that do not." It caps *consecutive* continuations per agent, held per `StopHookParent`, i.e. per agent per session-load. It is not persisted; a reload resets it.

### `HookRouting::handle` — hooks.rs:412

"No events and no state... The one thing it decides is what a halt *means*, and it decides it by not deciding."

**`Ran { key, records }`** (hooks.rs:419): if `agents.get(key)` is resident, `tell(AgentCommand::HooksRan { records })`. Then `CommandEffect::none()`. A missing agent is not an error: "the records describe a call it made before it left". No spawn.

**`Halt { key, reason }`** (hooks.rs:429):

1. Resolve `agents.get(key)` **and** `.filter(|_| state.status == SessionStatus::Running)`. On `None`: `tracing::warn!("a hook halted an agent whose turn had already ended; ignored")` and `none()`. "A halt races the turn it is halting: the records reach the session on the sink while the tool call that produced them is still returning, so the turn can finish first. Failing it then would rewrite a turn that already ended."
2. Cancel with an ack and `timeout(CANCEL_TIMEOUT, rx)`; on timeout `tracing::warn!("halted agent did not finish in time")`. "Cancel first, so the agent is not still appending to its own journal when the outcome below is folded."
3. **Round-trip through a synthetic outcome** (hooks.rs:464–481):

```rust
actor.on_agent_outcome(
    state,
    AgentOutcome::Failed {
        agent: match key {
            AgentKey::Main => actor.id,
            AgentKey::Sub(id) | AgentKey::Step(id) | AgentKey::Fork(id) => id,
        },
        error: reason,
        recoverable: false,
        terminal: false,
    },
    ctx,
).await
```

"Routed through the ordinary outcome path rather than given its own per-key branching: a halt is a failure with a reason, and what a failure means for a main agent, a subagent and a step is already decided in one place." `recoverable: false, terminal: false` — "re-running the same turn would meet the same hook, but the session is perfectly able to run the next thing the user sends."

Note the shape of this: the session **manufactures an `AgentOutcome` no agent sent** and feeds it to `on_agent_outcome`, which immediately re-splits it back into a `TurnEnd::Failed` and re-derives the identity from the uuid it just encoded. The `AgentKey → Uuid` collapse is lossy in exactly the way `on_agent_outcome`'s routing then has to undo by consulting `state.run`, `self.id` and `state.forks`. `terminal: false` means the `Failed` arms of the four handlers are taken, so: main → `TurnFailed`, fork → `ForkTurnEnded { Failed }`, subagent → `SubAgentFailed`, step → `StepFailed` (and the run does not advance).

The `state.status == Running` guard also means a halt aimed at a *subagent* or a *fork* is dropped whenever the session's own status is not `Running` — the guard reads the session's status for every key, not the key's own.

**`ContinueAfterStop { key, reason }`** (hooks.rs:483): if resident, `tell(AgentCommand::Enqueue { item: Incoming::Continue { id: Uuid::new_v4().to_string(), reason }, ack: None })`. Then `none()`. "The turn it continues is over by the time this lands, so the agent's own boundary drain is what starts the next one." No spawn, no guard on status.

## 7. `SessionCore` (core.rs)

### `SessionCore::handle` — core.rs:38

| command | behaviour |
| --- | --- |
| `SetTitle { title, reply, .. }` | `normalize_session_title(&title)` → `Ok` → `actor.rename_session(title)`; `Err` → `Err(e.to_string())`. Persists `[Renamed { name }]` **only on `Ok`** — "a rejected title must not be recorded as one". Reply sent after the effect is chosen. The `agent: AgentId` field is discarded (`..`) — this handler renames the session regardless of who asked. |
| `TitleSet { name }` | `actor.spec_mut().name = Some(name.clone())`, persist `[Renamed { name }]`. The path that does not ask the supervisor first. |
| `RecordSpec { spec }` | Idempotent on `state.spec.is_some()` → `none()`. Else `actor.adopt(*spec, state, ctx)` then persist `[SpecRecorded { spec }]`. |
| `Progress { key, stage, detail }` | `actor.record_on(key, LifecycleEvent::Preparing(PreparingLifecycle { stage, detail }))`, then `none()`. |

### `impl SessionActor` in core.rs

- **`title_from_first_message(text)`** (core.rs:105) — returns if `spec().name.is_some()`; `derive_title(text)` or return; `rename_session`, logging a warning on failure. "Best-effort and fire-and-forget."
- **`rename_session(title) -> Result<String, String>`** (core.rs:118) — `supervisor.ask(RenameSession { id, name, reply })`, mapping a transport error to `"session supervisor unavailable: {e}"` and a persist error to `"persist session title: {e}"`. Then `spec_mut().name = Some(title)` and a fire-and-forget `PublishSessionTitle`. **The supervisor persists before the session's own journal does** — the `Renamed` event is written by the caller afterwards.
- **`record_lifecycle(events, state)`** (core.rs:160) — see §3.
- **`record_on(key, event)`** (core.rs:186) — resident-only, silent on a miss.

### `derive_title` — core.rs:22

First line, trimmed. Empty → `None`. `<= TITLE_MAX_CHARS` (= `title_tool::SESSION_TITLE_MAX_CHARS`) → as-is. Else the first `TITLE_MAX_CHARS` chars, `trim_end`, plus `'…'` — so the result is `TITLE_MAX_CHARS + 1` chars when nothing was trimmed.

### `SessionCore::apply` — core.rs:216

| event | fold |
| --- | --- |
| `UsageRecorded { agent_id, usage_total }` | `state.agent_usage.insert(agent_id, usage_total)` |
| `SpecRecorded { spec }` | `state.spec = Some(*spec)` |
| `Renamed { name }` | `if let Some(spec) = state.spec.as_mut() { spec.name = Some(name) }` — "a rename must not resurrect a spec that was never recorded" |
| anything else | `unreachable!("SessionCore was handed {other:?}")` |

`SessionActor::apply_event` (mod.rs:1106) routes every variant explicitly, so the `unreachable!` is guarded by an exhaustive match one level up.

## 8. `AgentKey`'s four jobs

`AgentKey` (types.rs:925) is `Main | Sub(Uuid) | Step(Uuid) | Fork(Uuid)`, `#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]`. **No inherent methods and no hand-written impls anywhere** — no `Display`, no `Ord`, no `From`.

`SessionAgentKind` (context.rs:109) has the same four variants, `#[derive(Clone, Copy)]`, and one `impl` block (context.rs:118) with three methods. Its own doc says it is "Temporary, and deliberately so: it duplicates `RunnerKind` plus an id, and the change that gives the session real runners deletes it."

The four jobs the two types do, in the terms the code uses:

1. **Addressing** — which mailbox, which journal id, which actor-path name, which revision channel.
2. **Provenance** — which log a record, a lifecycle entry or a progress line belongs in.
3. **Configuration** — which settings, which capabilities, which system-prompt sections, which hook event.
4. **Semantics of an end** — what "this agent stopped" journals, and whose status it moves.

### Every decision site

#### `crates/server/src/sessions/session_actor/context.rs`

| site | decides |
| --- | --- |
| 121–128 `agent_key()` | 1:1 `SessionAgentKind` → `AgentKey`; the key the session registers/looks up by |
| 133–135 `broadcasts()` | `Main \| Step \| Fork` narrate setup; `Sub` is silent — "its progress reaches the reader as the parent's `SubAgent` entry instead" |
| 142–147 `agent_id(session_id)` | `Main` → the session id; `Sub \| Step \| Fork` → own uuid, in the runners' flat id space |
| 163–165 `loading_for` | sets `Loading.key`, `.agent`, `.narrate` together so they cannot disagree |
| 194–204 `scoped_client` | `Main` → the raw client; `Sub \| Step \| Fork` → `with_agent_id(id)`, its own cwd/env bucket |
| 344 | `narration_pump(.., self.loading.key)` — whose log vendor boot narration streams into |
| 363 | `scoped_client(&self.kind, client)` per turn |
| 564–570 | start hook: `Sub` → `SubagentStart(agent_id, agent_type)`; `Main \| Step \| Fork` → `SessionStart` |
| 56–58 `emit_progress` | routes a progress stage to one agent's log by key |
| 80–82 `narration_pump` | same, for vendor narration |

#### `crates/server/src/sessions/session_actor/mod.rs`

| site | decides |
| --- | --- |
| 143–148 `SessionAgents::get` | `Main` → the main resident; `Sub \| Step \| Fork` → the one shared sub map |
| 481–486 | journal id + revision channel: `Main` under the session id / `"main"`, others under their uuid |
| 488–493 | the child actor's name, and therefore its path and dedup identity |
| 494 | `plan.kind.agent_key()` → the key handed to `StopHookParent::wrap` |
| 499, 532 | the kind handed to `loading_for` and stored on the provider |
| 546 | `params.requires_result = matches!(kind, Step(_))` — only a step owes a structured result |
| 561 | `ready: Self::runnable(state)` (not key-dependent, listed for completeness) |
| 576–585 | registration: `Main` creates the interactive roster; the rest `insert_sub` |
| 610 | `spawn_main_agent` builds `SessionAgentKind::Main` with `RunnerKind::Conversation` |
| 633–670 `resolve_agent` | unaddressed → in-flight `Step` on a run, else `Main`; an id is tried Step → Fork → Sub |
| 685–694 `resolve_step` | answers `Step(id)` for resident or log-spawned step actors |
| 712–726 `effective_settings` | `Main \| Fork` → session spec; `Step` → that step's preset; `Sub` → recurse to the tree owner |
| 740–743 `effective_settings_for_parent` | a top-level spawn inherits `Main` or the owning `Step`; a nested one inherits `Sub(id)` |
| 757 `cancel_agent` | looks up by key; cancels the cached client then the loop |
| 789–790 `cancel_in_flight` | the in-flight `Step` on a run, else `Main` |
| 857–887 `reach` | `Sub` cold-spawns from the record's `agent_type`; `Step` from the run log; `Fork` if in the roster; `Main` → `None` |

#### `crates/server/src/sessions/session_actor/turns.rs`

| site | decides |
| --- | --- |
| 122–143 `stop_target` | `"main"` on a run → in-flight `Step`, else `Main`; an id is Step → Fork → Sub (this order is what stops a fork being mislabelled) |
| 158–192 `stop_boundary` | `Main` → `TurnStopped` (gated on session `Running`); `Step` → `StepCancelled` (gated on `run.current() == Some(index)`); `Fork` → `ForkTurnEnded { Stopped }` (gated on the fork's `Running`); `Sub` → `SubAgentFailed(STOPPED_ERROR)` (gated on the node's `Running`) so a blocked parent is unblocked |
| 328, 355–362 `start_fork` | `Main` → `ForkParent::Main`; `Fork(id)` → `ForkParent::Fork(id)`; `Sub \| Step` → rejected |

The gate note at turns.rs:153–157: "`Running` and not also `AwaitingInput`, except for a step. Cancelling does not clear the questions an agent is parked on, so a boundary journaled over a park would read `Idle` beside questions still pending."

#### `crates/server/src/sessions/session_actor/reads.rs`

| site | decides |
| --- | --- |
| 282–286 | only `Step` looks up a run execution |
| 289–290 | only `Sub` looks up a forest node |
| 293–296 | the entry shape: `main_entry(status)` / `step_entry` / `sub_entry` / `fork_entry` |
| 304 | the document's settings come from `effective_settings(state, key)`, not the session spec |

#### `crates/server/src/sessions/session_actor/hooks.rs`

| site | decides |
| --- | --- |
| 68, 72 | `SessionHookSink { key }` — whose transcript runtime hook records land in |
| 95, 105 | the sink tags `Ran` / `Halt` with its key |
| 130, 147 | `StopHookParent { key }` / `wrap` |
| 188–203 | `Sub` → `SubagentStop`; `Main \| Step \| Fork` → `Stop` |
| 228, 240 | `ContinueAfterStop` / `Ran` tagged with the key on block and on cap |
| 469–470 | the synthetic failure's agent id: `actor.id` for `Main`, the uuid otherwise |

#### `crates/server/src/sessions/session_actor/core.rs`
- 188 `record_on(key, event)` — routes one lifecycle entry to that agent's actor when resident.

#### `crates/server/src/sessions/session_actor/{fork,run,subagent}.rs`
- fork.rs:378 — `effective_settings(state, AgentKey::Fork(id))`; a run answers `None` rather than inventing settings.
- fork.rs:396 — spawns with `SessionAgentKind::Fork(id)`.
- run.rs:167 — `cancel_agent(AgentKey::Step(step.agent))` before a retry, so the retry is the only writer.
- run.rs:340 — spawns with `SessionAgentKind::Step(agent_id)`.
- subagent.rs:261 — `effective_settings(state, AgentKey::Sub(id))`; a cold node reruns under its tree root's settings.
- subagent.rs:277 — spawns with `SessionAgentKind::Sub(id)`.

#### `crates/server/src/sessions/session_actor/types.rs`
- 318, 326, 333 — `HookCommand::{Ran, ContinueAfterStop, Halt}` each carry `key: AgentKey`.
- 367 — `CoreCommand::Progress { key }`.

#### `crates/server/src/sessions/lifecycle_routing.rs`

| site | decides |
| --- | --- |
| 21 | `type Entry = (AgentKey, LifecycleEvent)` |
| 41–45 `session_wide` | a session-level fact goes to `Main`, or to the in-flight `Step` on a run (a run has no main) |
| 147 | `StepStarted` → that step's own log ("These used to route to `Main`, which a run does not have, so every one of them was dropped with a warning") |
| 185–186 | `ForkCreated` → the parent conversation's log: `ForkParent::Main → Main`, `ForkParent::Fork(pid) → Fork(pid)` |
| 207 | `ForkTurnEnded` → the fork's own log, not the source's ("Left out, a fork read `RUNNING` for ever") |
| 225–234 `every_agent` | the session-wide key + every `Sub` + every `Fork`; forks are included so a `SessionFailed` reaches them |
| 273 | a finished subagent's `TurnEnded` on itself, paired with the parent entry |
| 286 `step_entry` | `Step(step.agent)` |
| 296–309 `last_step_entry` | the run's own end, on the step that ran last |
| 313–319 `parent_key` | `SubAgentParent::SubAgent(id)` → `Sub(id)`; `Main` → the in-flight `Step` on a run, else `Main` |

#### `crates/server/src/sessions/orchestrator.rs`
- 58 — `Delivery.to: AgentKey`.
- 84–91 `owed_deliveries` — a nested spawn's result → `Sub(parent)`; a top-level spawn's → the tree owner (`TreeOwner::Step(agent) → Step(agent)`, `TreeOwner::Main → Main`), read off the tree rather than the session's current root.

#### `crates/server/src/sessions/runners/`
- loading.rs:179 — `Loading.key`, used for narration, the hook sink and client scoping.
- loading.rs:204 — `progress()` emits under `self.key`, gated on `narrate`.
- capabilities/runtime.rs:111 — `Ok(scoped(loading.key, client))` in `acquire`.
- capabilities/runtime.rs:310–317 `scoped` — `Main` → the raw client; `Sub | Step | Fork` → `with_agent_id(id)`.
- capabilities/runtime.rs:333 — the hook sink is built with `loading.key`.
- capabilities/runtime.rs:416–425 — `matches!(loading.key, AgentKey::Sub(_))` is the only thing that adds the `subagent_role` prompt section (and the plugin agent definition after it).
- capabilities/sub_agent.rs:240–242 — where this agent's children hang: `Sub(id)` → `SubAgentParent::SubAgent(id)`; `Main | Step | Fork` → `SubAgentParent::Main`, because each roots its own tree.

#### `crates/server/src/sessions/mod.rs`
- 113 — a doc-comment reference only; no branching.

#### Test-only sites (kept for completeness)
- context.rs:691, 705, 711–712, 720–721 — control-plane tools reach `Main`/`Fork` only, never `Sub`/`Step`.
- context.rs:734, 743, 755 — main keeps `spawn_agent`/`set_session_title`/`ask_user`; sub is stripped.
- context.rs:785–788, 815–823, 856, 860, 1129, 1305, 1373, 1435.
- testing.rs:1272, 1341 — `test_loading(.., kind)` funnels through the real `loading_for`.
- testing.rs:1303, 1316–1317 — only `Fork` sets `Assembly::fork`.
- testing.rs:1322–1331 — kind → `RunnerKind`: `Main | Fork` → `Conversation`; `Sub` → `SubAgent`; `Step` → `Workflow` plus `StepResultCapability` plus a non-interactive `AskUser`.
- testing.rs:1343, 1349, 1435.
- lifecycle_routing.rs:510, 603, 640, 658, 667, 692, 740, 776, 786–787, 820, 822.
- orchestrator.rs:139, 198.
- capabilities/mod.rs:475; capabilities/runtime.rs:553, 561.

### The one asymmetry worth naming

`AgentKey` and `SessionAgentKind` are the same four variants with the same meanings, and `agent_key()` (context.rs:121) is a total 1:1 map between them. The split exists only because the provider is built before the session registers the agent. `SessionAgentKind` additionally answers `broadcasts()` and `agent_id()`, which `AgentKey` cannot; `AgentKey` is what every *other* file speaks. Anything that wants both — `spawn_agent` — holds both (mod.rs:494 and mod.rs:532).

## 9. Test inventory

### mod.rs

**No tests.** The file ends with `#[cfg(test)] pub(crate) mod testing;` (mod.rs:1212–1219) — a shared harness, not a test module. `testing::decisions` (testing.rs:38) is the only door onto `next_actions`, and `testing::fold` (testing.rs:29) folds an event list into a `SessionState`.

### hooks.rs — `mod tests` (hooks.rs:505), "What a `Stop` hook can do to a turn, and what a halt means"

| test | line | what it pins |
| --- | --- | --- |
| `a_blocking_stop_hook_starts_another_run_with_its_reason` | 522 | a blocking `Stop` means *blocked from stopping*: two inputs, the second containing the block reason |
| `an_unconditionally_blocking_stop_hook_is_stopped_by_the_cap` | 543 | exactly `1 + MAX_STOP_CONTINUATIONS` inputs |
| `the_capped_continuation_is_recorded_as_cap_reached` | 557 | the last stop outcome is `StopOutcome::CapReached` |
| `non_blocking_additional_context_does_not_start_a_run` | 572 | advisory context informs, does not force a turn: one input |
| `a_halt_beats_a_blocking_stop_hook` | 588 | `continue: false` outranks `decision: "block"`; one input, and no `TurnFailed` in the journal |
| `halting_the_main_agent_fails_the_turn_with_the_hooks_reason` | 617 | `HookCommand::Halt { key: Main }` drives the session to `Failed` with the hook's reason |
| `a_failing_stop_hook_concludes_the_turn_anyway` | 657 | `StopOutcome::Failed` does not deny: one input |
| `every_stop_hook_run_reaches_the_transcript` | 671 | one stop outcome recorded |
| `a_stop_hooks_context_reaches_the_next_prompt` | 691 | a `Stop` hook's `additionalContext` reaches the *next* turn's prompt |
| `a_subagents_hooks_land_on_its_own_transcript` | 720 | `Ran { key: Sub(id) }` records on the subagent's log and not the main agent's |

**Exact-string assertions:**

- **hooks.rs:646** — `assert_eq!(reason, "the repo is locked")`. The literal is supplied by the test at line 638, so it pins pass-through, not wording.
- **hooks.rs:745** — `assert_eq!(hook_ids(&page), vec!["hook:0".to_string()])`. **Pins a generated id format** (`hook:<index>`) that no code in these three files produces — the tightest coupling in the set.
- **hooks.rs:608** — `!events.iter().any(|e| e.contains("TurnFailed"))`. A substring match on debug-formatted journal events; renaming the `TurnFailed` variant makes this pass vacuously.
- **hooks.rs:535–536, 709** — `.contains(...)` on strings the test itself supplied.

Two literals in the production code have **no test**: `"a hook set continue: false"` (hooks.rs:276), `"a Stop hook asked for another iteration"` / `"a SubagentStop hook asked for another iteration"` (hooks.rs:324, 332). So is the `tracing::info!` wording at hooks.rs:215.

Harness: `stop_harness` (testing.rs:1027), `stop_harness_with_prompts` (1033), `stop_harness_with_journal` (1043), all onto `stop_harness_full` (1055); assertions read `settled_inputs` (1132), `stop_outcomes` (1113), `journaled_events` (1168), `hook_ids` (954).

### core.rs — `mod tests` (core.rs:243)

| test | line | what it pins |
| --- | --- | --- |
| `a_session_records_its_spec_in_its_own_log` | 292 | `TitleSet` reaches the session's own journal; the replayed spec has the fixture vendor and the new name |
| `a_reload_adopts_the_journaled_spec_instead_of_recording_again` | 322 | after `node.restart()`, a second load adds **exactly one** event (the rename), never a second `SpecRecorded` |
| `a_title_is_derived_from_the_first_line_only` | 368 | `derive_title`: first line only, empty → `None`, over-long → elided with `'…'` at `TITLE_MAX_CHARS + 1` chars |

Two local helpers: `journaled_spec` (core.rs:252) replays the journal directly (with an `#[expect(clippy::disallowed_methods)]`), `until_named` (core.rs:274) polls up to 100 × 10 ms and panics with "the rename never reached the session's own log".

**Exact-string assertions:**

- **core.rs:369** — `assert_eq!(derive_title("hello\nworld").as_deref(), Some("hello"))`. Genuinely pins behaviour.
- **core.rs:373** — `assert!(title.ends_with('…'))` pins the ellipsis character.
- **core.rs:311, 359** — `Some("named")`, supplied by the test.

`a_reload_adopts_the_journaled_spec_instead_of_recording_again` is the only test in these three files that exercises `adopt` at all, and it exercises only its idempotence, not any of the four repairs.

## 10. Surprises, and comments the code contradicts

Collected so none is lost in the sections above.

1. **`ForkedAgents::on_load` is never called.** `adopt` (mod.rs:358–363) lists four components; `ForkedAgents` is not one. `ForkCommand::ReseedInterrupted` therefore has no producer. Two comments claim otherwise:

   > types.rs:556–558 — "a crash between the two replays as a fork still `Provisioning`, which `ForkedAgents::on_load` re-seeds."

   > fork.rs:12 — "which [`ForkedAgents::on_load`] re-seeds — strictly better than an untracked agent"

   Against the code:

   > mod.rs:358–363 — `let repairs: Vec<SessionCommand> = [RuntimeLifecycle::on_load(&cx, state), SubAgents::on_load(&cx, state), WorkflowRun::on_load(&cx, state), Turns::on_load(&cx, state)]`

   It is unit-tested in isolation at fork.rs:690–695, which is why it looks live.

2. **`ForkedAgents::busy` is never asked**, so a session can be offloaded mid-seed. `SessionActor::busy` (mod.rs:900–905) ORs four components. The trait doc claims the opposite is structural:

   > component.rs:89–91 — "Asked of every component and OR-ed together — which is what lets a component added later make itself heard, where today's single hand-written condition could not."

   And fork.rs:507 says "[`ForkedAgents::busy`] is what keeps the session loaded". Combined with (1), a fork abandoned mid-seed is repaired by nothing: not by staying loaded, and not by the reload.

3. **`flush_then_drain` has no doc comment; `busy` has two.** The block at mod.rs:890–897 — "Everything the orchestrator wants started at this turn boundary, performed in order, each seeing the state the previous one produced. / Every turn boundary routes through here — without that, a result owed to a subagent parent strands..." — describes `flush_then_drain`, but it sits immediately above `/// Whether any component has work in flight...` and `fn busy` (mod.rs:898–900). `flush_then_drain` at mod.rs:931 has none.

4. **`report_status` has no doc comment; `report_forks` has both.** mod.rs:383–400 is entirely about `report_status` ("Tell the supervisor the status this session's journal just folded", "thirteen `report(LITERAL)` calls", "`None` at load, so a freshly recovered session always reports once") and is attached to `report_forks`, whose own doc begins at mod.rs:401. `report_status` at mod.rs:441 has none.

5. **The action list is a snapshot, not a fixpoint.** mod.rs:938 calls `next_actions(&next)` once, before the loop. The doc says "each seeing the state the previous one produced" — true of the *state* handed to `perform`, not of the *list*. An action that becomes available because an earlier one changed the state waits for the next boundary.

6. **Two different "can this run" predicates.** `next_actions` gates on `RuntimeLifecycle::ready` alone (mod.rs:916); `runnable` (mod.rs:1073), which is what an agent is *told* at spawn, is `ready && !Unrecoverable`. So on an `Unrecoverable` session the boundary still collects and performs actions.

7. **`spawn_agent` can return a live, unregistered agent.** mod.rs:580–584: the `Sub | Step | Fork` arm inserts only `if let Some(agents) = self.agents.as_mut()`, and returns `Some(resident)` either way. And the `Main` arm (mod.rs:577) overwrites the entire roster rather than replacing one slot.

8. **`HookCommand::Halt`'s liveness guard reads the *session's* status for every key.** hooks.rs:436–439 filters on `state.status == SessionStatus::Running` regardless of whether the key is `Main`, `Sub`, `Step` or `Fork` — so a halt aimed at a subagent or a fork is dropped whenever the session itself is not `Running`, even if that agent is.

9. **A halt is a round trip through a fabricated `AgentOutcome`.** hooks.rs:464–481 encodes the `AgentKey` down to a bare `Uuid`, which `on_agent_outcome` (mod.rs:1017–1033) then decodes again by consulting `state.run`, `self.id` and `state.forks`. The comment presents this as a virtue ("what a failure means... is already decided in one place"); the round trip is the mechanism by which the key's kind is lost and re-derived.

10. **`deliver` is at-least-once by design, and says so.** mod.rs:815–823. Every other write in the boundary shares the same ordering — perform first, persist after — so a crash mid-boundary can re-run any action whose event never landed.

11. **`Turns::on_load` and `Turns::actions` are both explicit no-ops** (turns.rs:512, 528), each with a long comment about the bug that made them so. `adopt` still calls `on_load` and `next_actions` still calls `actions`.

12. **`rename_session` writes to the supervisor before the session's own journal.** core.rs:118–139 asks the supervisor to persist, mutates the resident spec, and publishes; the `Renamed` event is persisted by the *caller* afterwards. A crash between the two leaves the supervisor's list ahead of the session's log — the reverse of the ordering `on_events_persisted` is careful about ("the supervisor's copy can lag the journal, never lead it", mod.rs:1149–1151).
