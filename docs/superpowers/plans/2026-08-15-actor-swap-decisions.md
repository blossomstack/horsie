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

**A terminal turn failure must journal a terminal *runtime* failure (found writing B6).** Today `turns.rs:270` and `fork.rs:317` journal `SessionFailed` directly, and `route` fans it out to every agent — which is how a session that has died tells its forks and resident workers to stand down. In the new vocabulary the only producer of that fan-out is `Runner{runtime, Runtime(Failed{terminal:true})}`; a conversation's `TurnFailed` routes to that conversation alone, correctly. So B9's turn-failure path has to journal the runtime event too when the failure is terminal. Miss it and nothing breaks loudly: the session shows failed, and its forks sit there believing they can still run.
