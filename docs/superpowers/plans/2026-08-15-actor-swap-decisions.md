# The six holes, settled

Planning notes for `feat/session-actor-runners`. Fold the durable parts into the spec before the PR; delete the rest.

## 1. `AskUserCapability.pending` — deleted, not fixed

The hole: `AskUserToolbox` sends no command (it returns `StopRun`), so `Tool("ask_user")` never reaches the capability, `pending` is never set, and `Message::Ask(Answered)` is claimed by nobody.

The tempting fix was to have the actor synthesize a `Message::Tool("ask_user")` from `AgentOutcome::Asked`. Rejected: a synthetic tool call is a lie in the journal, invented so a field could be populated.

Look at what `pending` is *for*. It is the addressee an answer routes to. But an answer arrives as `TurnCommand::Answer { agent_id, .. }`, which already names the agent — and `SessionState.agents` already maps an agent to its runner. The routing question `pending` exists to answer is one the session can answer without it.

**So `pending` goes, and with it `AskMsg`, the `Message::Ask` arm, and `ask_user::Event`.** `AskUserCapability` keeps only `mute` and becomes pure config: it decides what to equip and answers for the tool name. An answer is addressed by agent id like every other structural message.

This also kills the "nobody claimed it" error for answers, because answers stop being offered around at all.

## 2. `ForkCommand::Delete` → `SessionEvent::RunnerDeleted { id }`

A runner leaves the state. Not `RunnerEnded { Cancelled }` plus a `deleted` flag: a deleted fork is *gone*, and a flag every projection has to remember to filter is the kind of thing one of them will forget. `SessionState::apply` removes the record and every `agents` entry pointing at it.

## 3. `CoreCommand::TitleSet` → `SessionEvent::Renamed { name }`

A person renaming from the session list. Writes `spec.name`. Distinct from `set_session_title`, which is an agent naming its own conversation and belongs to `TitleCapability`.

## 4. A workflow-root session gets no `TitleCapability` — unchanged

Today a step has no title layer, because a title belongs to the run rather than to one of its steps. Keep that. A person renaming a run goes through `Renamed` (3), which needs no capability. Nothing regresses: `CoreCommand::SetTitle` from a *person* still works for any session.

## 5. The root conversation's runtime client stays unscoped

`AgentKey::Main` gets the unscoped client today; everything else gets `with_agent_id`. The cwd and env bucket hang off that, so scoping the root would move every existing session's working directory — silently, and only visible on the second turn.

`Loading.key: AgentKey` becomes `Loading.role: AgentRole { Root, Fork, Sub, Step }`, derived from the owning `RunnerKind` plus `runner == state.root`. `scoped()` is `match role { Root => client, _ => client.with_agent_id(agent) }`. The role also carries the three other decisions `AgentKey` was making: the subagent role prompt, `SubagentStart` vs `SessionStart`, and progress narration.

`MAIN_AGENT_ID` survives as a *name*: the root conversation publishes revisions on `"main"` while journaling under its uuid, because `AwaitAgentRevision` keys the long poll on that exact string.

## 6. Subagent `depth` keeps per-tree semantics

`depth_of` is session-tree depth, so a subagent of a workflow step would read `2` where the wire says `1`. That is a defensible number and probably the better one, but it is a wire change and this PR is a refactor — it should preserve behaviour or change it deliberately, not as a side effect.

The projection computes depth as hops up to the nearest runner that is not a `SubAgent`. Changing it to session-tree depth is a one-line follow-up with its own note in the API docs.

---

# Also settled while reading the plan

**`RunCommand::RetryStep`** gets a pure `workflow::State::retry(&self, index) -> Result<Emit, String>`, called by the session rather than reached through `AgentLifecycle`. A retry comes from a person, not from an agent, so it is not part of the agent lifecycle.

**The three recovery repairs stay explicit.** `Runner::actions` is idempotent but will not restart a subagent that is `started && result.is_none()` — it has no way to know the process died. `SubAgentCommand::Reconcile`, `RunCommand::ReconcileInterrupted` and `ForkCommand::ReseedInterrupted` become `on_load` scans, each with a test that cuts the journal mid-flight. This is the worst failure mode in the change: silent, permanent, and — once invariant 6 lands — it deadlocks the parent forever.

**Ordering trap to write down:** `SessionState::apply` makes the first runner created the root. So `RecordSpec` must journal the root runner *before* the runtime runner, or the sandbox becomes the session's status.

# Four findings from the first B9 attempt

Written down because the reading that produced them is the expensive part, and the attempt that found them ran out of budget before implementing.

**1. `SubAgentCapability` has no gates, and B9's tests cannot port without them.** `on_tool` parses and mints a child unconditionally. The old handler refuses on depth (`"max subagent depth 4 reached"`), on an unknown caller (`"caller is not a known agent"`) and on the cap (`"{max} subagents already active"`). `Caller` carries `depth` and `active_agents` for precisely this and both are ignored. Four tests assert those exact strings — three in `subagent.rs`, one in `reads.rs` asserting a *per-step* cap of `"1 subagents already active"`. So the "concurrency cap" item I had filed under the last PR is load-bearing for the swap and moves ahead of it.

**2. My delivery-scan predicate was wrong.** I wrote `status == Done && parent.is_some()`. That never delivers a *failed* child: `finished()` answers `RunnerStatus::Failed` and `outcome()` answers `SubAgentOutcome::Failed`, so a worker that failed would be owed a report for ever. It must be `status.is_terminal() && parent.is_some()`. Scanning every terminal runner is safe and idempotent because the capability's own `outstanding.get(child)?` is the real ownership gate — which also means a child's outcome can go through plain `Capabilities::offer` and the session needs no owner lookup of its own.

**3. `on_agent_started` is not who journals a runner's start.** That method is *the agent reporting a turn began*, and the three runners disagree about it: conversation → `TurnBegan`, subagent → nothing, workflow → `StepStarted`. So `perform(StartAgent)` journals `AgentStarted` **plus** a per-kind start record. Doing it there rather than on the agent's report also closes a real double-start race: `workflow::actions()` keeps returning `StartAgent` for the same index until `StepStarted` is folded, and the agent's `Started` can arrive arbitrarily later. Corollary for `Advance`: self-send only when the batch produced events, and make `StartAgent` a no-op for an agent already in `state.agents`.

**4. A fork mid-seed reads not-busy — and this one is a live bug, not a port risk.** `conversation::State::busy()` is `matches!(turn, Running)`, but a fork awaiting its seed is `turn: Idle, seeded: false`, so `PrepareOffload` can unload the session while the detached seeding task is still running and then write into a stopped actor. Today the same hole exists by a different route: `ForkedAgents::busy` is written and simply never wired into `SessionActor::busy`. Likewise `ForkedAgents::on_load` is absent from `adopt`'s repair list, so `ReseedInterrupted` never fires at all — the third `on_load` scan fixes a live bug rather than preserving behaviour.

**5. Today's boundary performs *before* anything is durable.** `persist_and_advance` folds locally, drains — sending mailbox messages and spawning actors — and only then returns `CommandEffect::persist`. At-least-once delivery is the documented consequence for `deliver`, but every other action shares the ordering, including `StartAgent`.

That matters for the two-batch design, which reads naturally as persist-then-perform. Both are defensible; what is not defensible is drifting between them. **B9 keeps today's order — perform, then persist — and the second batch exists to give the *report* a re-drive point, not to make actions durable first.** A crash after performing and before persisting replays the action, which is exactly what `StartAgent`-is-a-no-op-for-a-known-agent and the `outstanding` ownership gate already make safe. Write that down in the boundary's doc comment, because the next reader will assume the opposite.

Two more live bugs the spec surfaced, worth fixing in the swap rather than porting:

- **A halt's liveness guard reads the *session's* status for every key** (`hooks.rs:436`), so a halt aimed at a subagent or a fork is dropped unless the session itself is `Running`. Under runners the guard becomes "that runner is not terminal", which is what it always meant.
- **Two doc comments sit on the wrong function** (`mod.rs:890` describes `flush_then_drain` but is attached to `busy`; `mod.rs:383` describes `report_status` but is attached to `report_forks`), and both real functions have none. Cheap to fix while rewriting the file.

Smaller, and easy to lose:

- `Turn::UserMessage` carries more than the table says: the `Unrecoverable` refusal, the workflow-root refusal, `Incoming::Compact` for `/compact` (no capability owns it; it stays in the actor), `title_from_first_message` before the enqueue, a re-provision self-send on `ProvisioningFailed`, and a `MessageAccepted` resolved by the **agent's** journal ack rather than the session's.
- `Fork::Create` reads `source_seq` with an `ask(AgentCommand::LogHead)` *on* the mailbox today. Moving it off-mailbox still requires the head before `RunnerCreated` is journaled — so either keep it inline or defer the create behind a self-send.
- `LifecycleCommand::Provision`'s old guard (`Idle | Provisioning | ProvisioningFailed`) is a different predicate from `runtime::State::actions()` (`Pending | Failed{terminal:false}`); the old one re-provisions from `Idle`.
- **The root conversation's `AgentId` is `AgentId(session_id)`. Settled.** A fresh uuid would move its transcript from `agent/<session_id>` to `agent/<uuid>`. The *session* journal breaks deliberately, but agent journals are separate persistence ids — a fresh id orphans every existing transcript and buys nothing. This is the one place the two id spaces coincide, and only for the root; it is not a general equivalence, and `AgentId == RunnerId` remains wrong.

---

**A terminal turn failure must journal a terminal *runtime* failure (found writing B6).** Today `turns.rs:270` and `fork.rs:317` journal `SessionFailed` directly, and `route` fans it out to every agent — which is how a session that has died tells its forks and resident workers to stand down. In the new vocabulary the only producer of that fan-out is `Runner{runtime, Runtime(Failed{terminal:true})}`; a conversation's `TurnFailed` routes to that conversation alone, correctly. So B9's turn-failure path has to journal the runtime event too when the failure is terminal. Miss it and nothing breaks loudly: the session shows failed, and its forks sit there believing they can still run.
