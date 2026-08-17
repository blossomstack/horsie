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

---

### Task 4 — `submit_result` migrates

Also needs no session — a step's output travels on its outcome.

**Create:** `agent_loop/capabilities/step_result.rs`.
**Delete:** `sessions/workflow/toolbox.rs`, `runners/capabilities/step_result.rs`.

**Test:** an undeclared outcome is refused and journals nothing; a valid submission parks then concludes.

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
