# Agent-side capabilities — implementation plan

**Spec:** `docs/superpowers/specs/2026-08-15-session-runners-design.md` (revised 2026-08-17)

**Goal:** move the capability machinery into the agent actor, one capability at a time, with the suite green at every step.

## Strategy: strangle, do not swap

The old path stays live and the new one grows beside it. A capability is migrated when the agent actor can route to it *and* the old toolbox for it is deleted — never a window where both are wired. The suite is green after every task, which is the point: this is a two-actor rewrite, and a red tree here means no checkpoint to fall back to.

The previous attempt at a big-bang swap ran out of budget mid-flight and left nothing. That is the failure this ordering exists to prevent.

## Global constraints

- Repo `horsie`, worktree `.claude/worktrees/actor-swap`, branch `feat/session-actor-runners`. **Local only — do not push.**
- `cargo clippy -p horsie-server --lib --all-features --tests -- -D warnings` and stable `cargo fmt` after every task. **Never `+nightly`** — `.rustfmt.toml` declares nightly-only options CI silently ignores.
- Journals break deliberately on both sides. No shims, no `#[serde(default)]` bridges.
- Every capability is `XxxxCapability` in its own file under `agent_loop/capabilities/`. Inner types stay plain: `sub_agent::Event`.
- A capability never reaches past `Act`. Needing a sixth verb means the enum grows deliberately.

## What already exists and survives

The three open PRs are not wasted. `runners/` — the four runners, the session fold, `assemble`, `reads`, `lifecycle_routing` — all stand. The capability *implementations* in `runners/capabilities/` largely survive too: their tool parsing, their refusal strings, their decision logic. What changes is the trait they implement and which journal folds them.

So most tasks below are **move and reshape**, not write-from-scratch.

---

### Task 1 — the trait, agent-side

**Create:** `crates/server/src/agent_loop/capabilities/mod.rs`

`Capability` (with `setup`/`teardown`/`tools`/`handle`/`apply`/`save`), `Msg`, `Decision`, `Act`, `SessionRequest`, `CapEvent`, `CapSlice`, and `Capabilities` — the newtype, with the same `save()`-based `Clone` and `Vec<CapSlice>` serde the session-side one uses.

Nothing implements it yet. Pure, no actor.

**Test:** the composition rules — first to claim wins; `Msg::Turn` reaches every capability rather than the first; a `CapSlice` round trip preserves folded state.

---

### Task 2 — the agent actor holds capabilities

**Modify:** `agent_loop/agent_actor.rs`

- `AgentState.capabilities: Capabilities`, folded from a new `AgentDomainEvent::Capability(CapEvent)`.
- Routing: a tool call is offered; `Answer` is offered; `Turn` is broadcast.
- Performing `Act::{Answer, Park, Resume, Enqueue}`. **`Act::Ask` is deferred to task 5** — until then a capability returning it is a `todo!()`, which nothing reaches because no capability is migrated yet.
- One generic toolbox layer that advertises `capabilities.tools()` and dispatches `execute` by name.

**Test:** a fake capability claiming a fake tool, parking, and resuming. The fold replays identically.

---

### Task 3 — `ask_user` migrates

The one capability needing no session at all, which is why it goes first: it proves the whole loop end to end.

**Create:** `agent_loop/capabilities/ask_user.rs` — holds `pending: BTreeMap<call, question>`, handles `Msg::Tool` and `Msg::Answer`.
**Delete:** `sessions/ask_tool.rs` (`AskUserToolbox`), `AgentState.asks`, `answered_turn` in `inbox.rs`, `runners/capabilities/ask_user.rs`.
**Modify:** `TurnCommand::Answer`'s handler forwards to the agent and nothing more; the `Asked` outcome becomes a status report.

**Test:** port every existing ask test. The reconciliation ones (`Incomplete { missing, unexpected }`) move to the capability. Fail-first: break the set comparison and watch them fail.

**Done so far** (commits `3b062967`, `7c8a92ef`): the capability, plus `AgentParams.capabilities` → `AgentDomainEvent::Equipped` → the fold, and `AgentCommand::Answer` consulting the park before the old path. Nothing equips it yet, so the old path is still what runs.

**Settled while building it — where abandonment lives.** The first cut left the rule in `queued_turn` and gave the capability bookkeeping on `TurnEvent::Began`, which is right about `Began` (a park *ends its own turn*, so clearing on `Ended` would discard the park it just made) but wrong about the data. `queued_turn` reads `state.asks` to build the abandonment results, so the park would live on both sides — decision #7 says a fact lives on exactly one.

The switch therefore has to move the *results* to the capability: `Msg::Turn(Began)` returns `Act::Resume` carrying `ABANDONED_ASK_RESULT` for each call it still holds. `queued_turn` keeps the rule it actually owns — which arriving item may override a park, and only a person may — and reads a generic "is parked" flag rather than the questions. Then `AgentState.asks` can go, which it cannot while `queued_turn` needs it.

---

### Task 4 — `submit_result` migrates

Also needs no session — a step's output travels on its outcome.

**Create:** `agent_loop/capabilities/step_result.rs`.
**Delete:** `sessions/workflow/toolbox.rs`, `runners/capabilities/step_result.rs`.

**`Act` grows a sixth verb here, deliberately.** `submit_result` does not park: it *concludes*. Both return `StopRun` to the toolbox, which is why the old code could treat them alike and sort them out afterwards in `interpret` by matching tool names — but a park owes a result later and a conclusion owes nothing ever, and `AgentOutcome::Concluded` carries an output that `Act::Park` has nowhere to put. So `Act::Conclude { output }`, and `interpret`'s name-matching on `SUBMIT_RESULT_TOOL`/`ASK_USER_TOOL` goes with it: the actor stops recognising two tool names and instead performs what a capability asked for.

**Test:** an undeclared outcome is refused and journals nothing; a valid submission concludes. Plus: the nudge budget (`MAX_RESULT_NUDGES`) has no unit test today — `ended_without_result` is covered only through `run.rs` integration tests. Give it one while it moves.

---

### Task 5 — the `Ask` path

**Modify:** `agent_actor.rs` performs `Act::Ask` by telling the session and routing the reply back as `Msg::Reply`. Session side gains one command carrying `SessionRequest` and one reply type.

This is the first cross-actor task, and it carries the crash window: **`Requested` is journalled before the ask; a dangling `Requested` is re-asked on load; the session dedupes by call id.** Write the crash test first — a journal cut between the two, replaying to exactly one child.

**Test:** the dedupe, and the re-ask. Both fail-first.

---

### Task 6 — `set_session_title` migrates

First user of `Act::Ask`. Small, and proves the path before `sub_agent` leans on it.

**Delete:** `sessions/title_tool.rs`.

---

### Task 7 — `spawn_agent` migrates

**Create:** `agent_loop/capabilities/sub_agent.rs` — `outstanding` lives here now, in the agent's journal.
**Delete:** `sessions/spawn_tool.rs`, `runners/capabilities/sub_agent.rs`.

Carries the three gates (depth, cap, unknown caller) — but the **cap moves to the session**, checked on `StartRunner`, because the count belongs to whoever owns the tree. The refusal comes back as a reply.

Invariant 6 lands here and is four lines: `Msg::Turn(Ended)` with a non-empty `outstanding` holds the conclusion.

**Test:** the gates, each fail-first. Invariant 6 with a real outstanding child.

---

### Task 8 — `fork` and `invoke_workflow` migrate

Same shape as task 7. `/fork` reaches the agent as a user message and is claimed by `ForkCapability`.

---

### Task 9 — the setup-only capabilities move

`runtime`, `memory`, `mcp`, `control_plane`. These only implement `setup`, so this is a file move plus a trait reshape. `RuntimeCapability` is the big one — it carries the scan, the provisioning and the base toolbox.

**Delete:** `runners/capabilities/` entirely, and `Runner::capabilities`/`capabilities_mut`.

---

### Task 10 — the session sheds what is no longer its

Runners lose their capability lists. `SessionState` loses every `CapEvent`. The session's job shrinks to: start runners, forward messages, track the tree, hold session-level facts.

---

### Task 11 — delete the old vocabulary

`AgentKey`, `SessionAgentKind`, the three-registry probes, `orchestrator.rs`, `forks.rs`, the old `lifecycle_routing.rs`, the eight `Component` impls. Remove `#![allow(dead_code)]` — that is the completion signal.

---

## Order rationale

Tasks 3 and 4 need no session, so they prove the agent-side machinery in isolation. Task 5 opens the cross-actor path and pays its crash-window cost once. Tasks 6–8 are then the same shape three times. Task 9 is bulk with little risk. Tasks 10–11 are deletion, which is where `-D warnings` does the work.

The riskiest task is 5, not 7 — the dedupe-and-re-ask pair is the only genuinely new distributed reasoning in the whole plan.

---

## Settled while implementing (2026-08-17)

All ten capabilities are ported to the agent-side trait. What the build taught us, beyond the plan:

**`Act` has eight verbs, and each growth was forced.** `Answer`, `Park`, `Resume`, `Enqueue`, `Ask` were the plan's five. Then:

- **`Conclude { output }`** — `submit_result` does not park, it concludes. Both stop the run, which is exactly why the old code could treat it and `ask_user` alike and sort them out afterwards by matching tool names in `interpret`. A park owes a result later; a conclusion owes nothing ever and carries an output `Park` has nowhere to put.
- **`Record(LifecycleEvent)`** — a capability's own events are folded but append nothing a client can read. `ask_user` journaling its park purely as a `CapEvent` would have left the question invisible in the UI: green tests, and only a browser would have noticed.
- **`Hold { note }`** — invariant 6. A turn boundary is *broadcast*, and `Capabilities::broadcast` merges what comes back, so a capability claiming the boundary with an empty `Decision` is invisible to the actor **by construction**. Only something in the merged result can carry "do not treat this ending as the agent finishing". The first cut used a claimed-but-empty decision and could never have worked.

**`Park` carries a `note`, and the actor journals its own record of it.** Being parked governs what no capability can see — whether the queue may start a turn, and which dangling calls recovery must leave alone. The capability keeps whatever it needs beyond that, which for `ask_user` is the question. Two facts about one call, not one fact twice.

**`interpret` no longer knows which tools finish a run.** It asks what its capabilities decided: a conclusion they carried, or a park they already journaled. The two name matches that remain are the old toolboxes' path and go with them.

**`Act::Ask` reaches the session through `AgentOutcomeSink::request`** rather than a new channel — it is the same relationship, and the same gates. `StopHookParent` must delegate it explicitly: the trait's default refuses, and it is the sink actually installed, so inheriting the default would fail every request with a reason that is not true.

## Known gaps, deliberately left

- **`spawn_agent` no longer lists the installed agent types.** `Capability::tools()` takes no `AgentFacts`, and the catalogue only exists after the runtime's workspace scan — which is what the old compose-time toolbox layer was for. `agent_type` is still parsed and forwarded, and an unknown one is still refused, but the model is no longer *told* which exist. Fixing it means either facts at advertise time or holding the catalogue as capability config. **This is a real regression, not a simplification.**
- **`subagent_status` takes no `id`.** The old tool could report one child's status from the session's forest; agent-side there is only "what I am still owed".
- **The crash window is not closed.** A capability journals its request before asking, but nothing re-asks a dangling one on load, and the session does not dedupe `StartRunner` by call id. Needs a `Msg` the actor broadcasts after recovery.
