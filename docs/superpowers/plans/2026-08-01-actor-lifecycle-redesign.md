# Actor Lifecycle Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the session actor tree production ready — lazy loading, idle offload, a resident agent, a durable input inbox, and a runtime that is transparent to the actors.

**Architecture:** The supervisor persists existence only and keeps status in memory; session actors load on demand and own their own status; the agent is resident for the session's loaded lifetime; all runtime lifecycle moves behind a server-level `RuntimeManager` that no actor blocks on.

**Tech Stack:** Rust (tokio, axum, `horsie-actor` event sourcing), fluorite schemas with TS codegen, React 19 web client, Playwright e2e.

**Spec:** `docs/superpowers/specs/2026-08-01-actor-lifecycle-redesign-design.md`

## Global Constraints

- **No backward compatibility.** Event enums, wire enums and status kinds change names freely. Existing state directories are wiped on deploy. Never add a deprecated variant "so old journals load".
- **Only the actor owning a persistence id touches that journal.**
- **No vendor call on an actor mailbox.** Vendor work happens inside a run task or a detached task, under a cancel token.
- Idle timing goes through an injectable clock. No `tokio::time::sleep` in a test to advance the idle timer.
- `make check` (fmt + clippy + tests + fluorite drift) must pass at the end of every task.
- Commit after every task, one commit per task.

---

### Task 1: Vendor wire protocol — `GetRuntime` and `HibernateRuntime`

**Files:**
- Modify: `models/fluorite/runtime_vendor.fl`
- Modify: `server/src/runtime_vendor/link.rs`, `server/src/runtime_vendor/mod.rs`, `server/src/runtime_vendor/fake.rs`
- Modify: `runtime-vendor/src/vendor.rs`
- Regenerate: `models/src/generated/`, `clients/ts/src/generated`, `clients/web/src/generated`

**Interfaces:**
- Produces: `RuntimeVendorCommand::GetRuntime(GetRuntimeRequest { runtime_id })`, `RuntimeVendorCommand::HibernateRuntime(HibernateRuntimeRequest { runtime_id })`, `RuntimeVendorEvent::GetRuntime(GetRuntimeResponse { runtime_id })`, `RuntimeVendorEvent::HibernateRuntime(HibernateRuntimeResponse { runtime_id })`; `RuntimeVendorLink::get(&self, runtime_id) -> Result<VendorRuntime, VendorError>`, `RuntimeVendorLink::hibernate(&self, runtime_id)`.
- `VendorError` becomes `{ Provision(String), Gone(String), Unavailable(String) }`.

- [ ] **Step 1: Rename the schema.** In `runtime_vendor.fl` replace `AttachRuntimeRequest`/`Response` with `GetRuntimeRequest { runtime_id: String }` / `GetRuntimeResponse { runtime_id: String }` (no `spec` field — a get must not be able to re-provision), and `StopRuntimeRequest`/`Response` with `HibernateRuntimeRequest`/`HibernateRuntimeResponse`. Update the union arms and the doc comments to the semantics in the spec's vendor contract table.
- [ ] **Step 2: Regenerate.** `make generate` (or the fluorite codegen npm scripts for `clients/ts` and `clients/web`). Expected: `models/src/generated/runtime_vendor.rs` and both TS trees update; the CI drift job would otherwise fail.
- [ ] **Step 3: Server link.** In `link.rs`, split `provision()` into `create()` (unchanged, sends `CreateRuntime`) and `get()` (sends `GetRuntime`, builds the same `RuntimeVendorTransport` + `RuntimeClient` on success). Add `hibernate()` sending `HibernateRuntime` and ignoring the reply like `delete()` does. Map a `RequestFailed` from `GetRuntime` to `VendorError::Gone`, and a write failure / disconnected socket to `VendorError::Unavailable`.
- [ ] **Step 4: Agent loop.** In `runtime-vendor/src/vendor.rs` handle the two new commands: `GetRuntime` answers `GetRuntimeResponse` if `ConnectedRuntimeRegistry` has a live transport for that id, else `RequestFailed { message: "runtime gone" }`; `HibernateRuntime` is a no-op that answers `HibernateRuntimeResponse` (this agent cannot suspend a process, and saying so honestly beats destroying the workspace). Delete the create-fallback path that `AttachRuntime` had.
- [ ] **Step 5: Serialize lifecycle per runtime id.** In the same command loop, hold a `tokio::sync::Mutex` per `runtime_id` (a `HashMap<String, Arc<Mutex<()>>>`) around create/get/hibernate/delete handling, so a `GetRuntime` arriving during an in-flight `CreateRuntime` waits for it — the conformance requirement from the spec.
- [ ] **Step 6: Fake vendor.** Update `fake.rs` to record `get:<id>` and `hibernate:<id>` signals, and add a builder knob `gone_on_get(bool)` so tests can drive the `RuntimeGone` path.
- [ ] **Step 7: Conformance tests.** In `server/src/runtime_vendor/link.rs` tests: (a) `get_after_create_returns_a_client`; (b) `get_without_create_is_gone`; (c) `hibernate_then_get_still_returns_a_client`; (d) `get_during_an_in_flight_create_waits_for_it` — drive the fake with a create that blocks on a barrier and assert the get resolves after it, not before.
- [ ] **Step 8: `make check`, then commit.** `git commit -m "feat(vendor): GetRuntime and HibernateRuntime replace attach/stop"`

---

### Task 2: `RuntimeManager` and `RuntimeClientProvider`

**Files:**
- Create: `server/src/runtime_manager.rs`
- Modify: `server/src/lib.rs` (module), `server/src/sessions/spec.rs` (`ServerDeps` gains `runtimes: Arc<RuntimeManager>`)

**Interfaces:**
- Produces:
```rust
pub enum RuntimeError { Unavailable(String), Gone(String), Provision(String) }

pub struct RuntimeManager { /* vendors, state_dir, github_tokens, plugins */ }
impl RuntimeManager {
    pub fn new(deps: RuntimeDeps) -> Self;
    pub async fn create(&self, session: &str, vendor: &str, spec: &SessionSpec) -> Result<(), RuntimeError>;
    pub async fn get(&self, session: &str, vendor: &str) -> Result<RuntimeClient, RuntimeError>;
    pub async fn hibernate(&self, session: &str, vendor: &str);
    pub async fn delete(&self, session: &str, vendor: &str);
    pub fn provider(self: &Arc<Self>, session: String, vendor: String) -> RuntimeClientProvider;
}
pub struct RuntimeClientProvider { /* Arc<RuntimeManager>, session, vendor */ }
impl RuntimeClientProvider { pub async fn get(&self) -> Result<RuntimeClient, RuntimeError>; }
```

- [ ] **Step 1: Move spec assembly.** Lift `write_runtime_spec`, the GitHub-token minting and the plugin-bundle/env resolution out of `session_actor.rs` into `RuntimeManager::runtime_spec(session, spec)`, re-assembled fresh on every `create`. Keep the capability file under `<state_dir>/sessions/<id>/`.
- [ ] **Step 2: Implement the four verbs.** Each resolves the vendor by name from `SharedVendors`; a missing name or a disconnected link is `RuntimeError::Unavailable` — never `Gone`. `get` returns the client from `link.get()`, mapping `VendorError::Gone` to `RuntimeError::Gone`. `hibernate` and `delete` are best-effort and ignore failures.
- [ ] **Step 3: Tests.** `unavailable_when_the_vendor_name_is_not_registered`, `unavailable_when_the_link_is_disconnected`, `gone_when_the_vendor_has_no_runtime`, `get_returns_a_client_after_create`, `create_assembles_env_fresh_each_time` (mint twice, assert two distinct tokens reached the fake).
- [ ] **Step 4: Wire into `ServerDeps`** and construct it in `server/src/bin/horsie-server/main.rs`. No call sites yet.
- [ ] **Step 5: `make check`, commit.** `git commit -m "feat(server): RuntimeManager owns runtime lifecycle"`

---

### Task 3: Session state machine

**Files:**
- Modify: `server/src/sessions/spec.rs` (`SessionStatus`), `server/src/sessions/session_actor.rs`, `server/src/sessions/mod.rs` (`UserMessageError`)
- Modify: `models/fluorite/session.fl` (`SessionStatusKind`)

**Interfaces:**
- Produces: `SessionStatus::{Idle, Running, AwaitingInput, Failed { reason }, Unrecoverable { reason }}`; `SessionDomainEvent::{MessageQueued, TurnBegan, AskRecorded, TurnEnded, TurnFailed, TurnStopped, TurnInterrupted, SessionFailed, UsageRecorded}`; `SessionState { status, pending_ask, inbox, agent_usage, last_error }`.

- [ ] **Step 1: Replace the status enum** in `spec.rs` and `SessionStatusKind` in `session.fl`; regenerate. Delete `Provisioning`, `Interrupted`, `Stopped`, `RecoveryFailed`; add `Unrecoverable`. Update `status_kind` / `status_reason`.
- [ ] **Step 2: Replace the event enum and `SessionState`** with the spec's tables. `InboxMessage { id: String, text: String, at_ms: u64 }`.
- [ ] **Step 3: Write the fold tests first** — `queued_then_begun_consumes_the_inbox`, `turn_began_clears_a_pending_ask`, `turn_stopped_keeps_the_inbox`, `turn_failed_sets_failed_and_keeps_the_inbox`, `session_failed_is_terminal`.
- [ ] **Step 4: Implement `apply_event`** to make them pass.
- [ ] **Step 5: Rewrite `handle_command`.** `UserMessage` always persists `MessageQueued` and replies `Ok(message_id)`; if the status is `Idle` or `Failed`, follow with the drain. `Stop` cancels the run, awaits the ack, persists `TurnStopped`, then drains. Delete `Provision`, `WakeMode`, `ensure_runtime`, `ensure_agent`, `wake`, `halt`'s runtime handling (cancel only), and the `on_agent_outcome` generation check.
- [ ] **Step 6: Implement `drain()`** — if the inbox is non-empty and no run is in flight: merge the texts in arrival order joined by `"\n\n"`, send `AgentCommand::Run { input }` or `AgentCommand::InjectToolResult` when `pending_ask` is set, and persist `TurnBegan { consumed, answering }`. Never called from recovery.
- [ ] **Step 7: `on_recovery_complete`** persists `TurnInterrupted` when the recovered status is `Running`, and does nothing else. No drain, no agent run.
- [ ] **Step 8: Behaviour tests** — `a_message_during_a_run_is_queued_not_rejected`, `turn_end_drains_the_inbox_as_one_merged_message`, `a_failed_turn_does_not_drain`, `stop_then_queued_message_starts_the_next_turn`, `recovery_from_running_lands_idle_with_the_inbox_intact`, `answering_an_ask_consumes_the_inbox_as_a_tool_result`.
- [ ] **Step 9: `make check`, commit.** `git commit -m "feat(session): durable inbox, four states plus terminal"`

---

### Task 4: Resident agent

**Files:**
- Modify: `workflow/src/agent_actor.rs`, `server/src/sessions/session_actor.rs`

**Interfaces:**
- Produces: `AgentActor` stays alive across turns; `RunReport { run_id, outcome }`; `ContextProvider` implementations receive a `RuntimeClientProvider`.

- [ ] **Step 1: Stop stopping.** In `handle_finished`, replace `CommandEffect::stop()` on `Completed` / `Concluded` / `Failed` with `CommandEffect::none()` (plus the existing persists). The agent goes idle instead of dying.
- [ ] **Step 2: `run_id`.** Give each started run a `u64` id held in `self.running`; `RunFinished` carries it; drop a report whose id is stale. Delete `SessionParent::generation` and `SessionActor::generation` and the staleness branch in `on_agent_outcome`.
- [ ] **Step 3: Provider not client.** `SessionContextProvider` holds `RuntimeClientProvider`; `provide()` calls `.get().await` and maps `RuntimeError::Gone` to a distinct failure the session turns into `SessionFailed { RuntimeGone }`, and `Unavailable`/`Provision` into `TurnFailed`.
- [ ] **Step 4: Delete transient readers.** Remove `NoContextProvider` and both spawn sites; `read_history` / `read_usage` ask `main_agent` directly. The session holds `main_agent: ActorRef<AgentCommand>` and `sub_agents: HashMap<String, ActorRef<AgentCommand>>`, both created when the session actor starts.
- [ ] **Step 5: Rename and persist the repair.** `sanitize_for_resume` → `repair_unanswered_tool_calls`; `sanitize_answering` → `repair_unanswered_tool_calls_except`. Persist the synthetic results as `AgentDomainEvent::MessageComplete` rows on `RunCancelled` and on recovery when the last turn was interrupted, and stop repairing on the turn-start path.
- [ ] **Step 6: Tests** — `agent_survives_a_concluded_turn`, `a_stale_run_report_is_ignored`, `history_reads_spawn_no_actor` (count children before/after), `interrupted_tool_calls_are_repaired_once_in_the_journal`.
- [ ] **Step 7: `make check`, commit.** `git commit -m "feat(agent): resident agent, run_id fence, persisted repair"`

---

### Task 5: Supervisor — existence only, lazy load, idle offload

**Files:**
- Modify: `server/src/sessions/supervisor.rs`
- Create: `server/src/sessions/clock.rs`

**Interfaces:**
- Produces: `trait Clock { fn now(&self) -> Instant }` with `SystemClock` and `TestClock`; `SessionSupervisorEvent::{SessionCreated, SessionNamed, SessionDeleted}` only; `SessionSupervisorCommand::{..., Tick}`.

- [ ] **Step 1: Delete `SessionStatusChanged` from the event enum** and from `SessionRecord`; keep an in-memory `status: HashMap<SessionId, SessionStatus>` updated by the (retained) `SessionStatusChanged` *command*. `List`/`Get` return `Option<SessionStatus>` — `None` means not loaded.
- [ ] **Step 2: Delete `on_recovery_complete`'s re-spawn loop.** Nothing loads at boot.
- [ ] **Step 3: Add `ensure_loaded(&mut self, ctx, id) -> Option<ActorRef<SessionCommand>>`** — returns the live child or spawns one from the recovered `SessionRecord`. Route `UserMessage`, `Stop`, `Subscribe`, `History`, `UsageStats`, `Delete` through it.
- [ ] **Step 4: Idle clock.** Record `last_activity: HashMap<SessionId, Instant>` on every routed command. A detached task sends `Tick` every 10s (interval also injectable). `Tick` offloads every loaded session whose last activity is older than the configured idle timeout **and** whose cached status is not `Running`.
- [ ] **Step 5: Offload sequence.** `SessionCommand::PrepareOffload { reply }` → the session refuses (`reply(false)`) if a run is in flight, else stops its agents, calls `runtimes.hibernate`, and returns `CommandEffect::none().and_ack(...).and_stop()`. On `true` the supervisor removes the child and its status entry.
- [ ] **Step 6: Tests** — `boot_loads_nothing` (assert no child and zero vendor signals after recovery with two sessions in the journal), `any_command_loads_the_session`, `idle_past_the_timeout_offloads_and_hibernates`, `a_running_session_is_never_offloaded`, `a_message_after_offload_reloads_and_gets_not_creates`.
- [ ] **Step 7: `make check`, commit.** `git commit -m "feat(supervisor): lazy load and idle offload"`

---

### Task 6: Runtime created once, at session creation

**Files:**
- Modify: `server/src/sessions/supervisor.rs` (`Create`), `server/src/sessions/session_actor.rs`

- [ ] **Step 1:** On `SessionSupervisorCommand::Create`, after persisting `SessionCreated`, spawn a **detached task** calling `runtimes.create(id, vendor, spec)`. Do not await it on the mailbox and do not await it in the HTTP handler.
- [ ] **Step 2:** Delete `SessionCommand::Provision` entirely. The first turn's `provider.get()` is what waits for creation to land — that wait is the vendor's obligation.
- [ ] **Step 3:** On `Delete`, call `runtimes.delete(id, vendor)` after the session actor stops.
- [ ] **Step 4: Test** — `twenty_turns_and_three_offloads_create_once`: drive twenty turns with offloads interleaved against the fake vendor and assert exactly one `create:` signal and many `get:` signals.
- [ ] **Step 5: `make check`, commit.** `git commit -m "feat(session): create the runtime once, at session creation"`

---

### Task 7: HTTP and SSE surface

**Files:**
- Modify: `server/src/http/handlers.rs`, `server/src/sessions/events.rs`, `models/fluorite/session.fl`, `models/fluorite/session_api.fl`

- [ ] **Step 1:** `send_message` returns `202 Accepted` with `{ messageId }`. Delete `UserMessageError::{TurnInFlight, Provisioning, RecoveryFailed}` and the `409` mapping; keep `NotFound`.
- [ ] **Step 2:** `SessionDetail` gains `inbox: Vec<QueuedMessage { id, text, at_ms }>`; regenerate schemas.
- [ ] **Step 3:** Add `SessionEvent::InboxChanged { queued: Vec<QueuedMessage> }` to the SSE union and emit it from the session on `MessageQueued` and `TurnBegan`. Delete the dead `SessionEvent::Asked` variant.
- [ ] **Step 4: Tests** — `send_message_returns_202_with_an_id`, `send_message_during_a_run_is_also_202`, `detail_exposes_the_inbox`.
- [ ] **Step 5: `make check`, commit.** `git commit -m "feat(api): 202 for messages, inbox on the wire"`

---

### Task 8: Web client

**Files:**
- Modify: `clients/web/src/pages/SessionView.tsx`, `clients/web/src/components/Composer.tsx`, `clients/web/src/hooks/useSessionStream.ts`, `clients/web/src/hooks/useSessions.ts`

- [ ] **Step 1:** Remove the `409` handling and the `askLocked` / `stoppable` composer latch that depended on `AwaitingInput`. The composer is always enabled except when the session is `Unrecoverable`.
- [ ] **Step 2:** Render queued inbox messages as unread — a muted bubble with an "unsent" marker — sourced from `detail.inbox` and kept live by `InboxChanged`.
- [ ] **Step 3:** Render `Unrecoverable` as a read-only banner carrying the reason, with the composer disabled and a "start a new session" link.
- [ ] **Step 4:** Show "unknown" (an em dash) for a session whose status the list does not have yet.
- [ ] **Step 5:** Update `clients/web/e2e/` specs that assert the old statuses or the 409 path.
- [ ] **Step 6: `make check` + `bun run build`, commit.** `git commit -m "feat(web): unread queued messages, terminal sessions"`

---

### Task 9: End-to-end invariants

**Files:**
- Modify: `tests/tests/session_server_e2e.rs`

- [ ] **Step 1:** Port the existing suite to the new statuses and the 202 contract.
- [ ] **Step 2:** Add the spec's remaining invariants that are not already covered by a unit test: boot loads nothing (2), reads acquire no runtime (2), hibernate on idle (3), no offload during a run (4), crash mid-turn keeps the inbox (6), `Gone` is terminal while `Unavailable` is retryable (8).
- [ ] **Step 3: `make check`, commit.** `git commit -m "test: pin the lifecycle invariants end to end"`

---

## Status as of 2026-08-01

Landed on `feat/actor-lifecycle-redesign`, workspace green (`cargo fmt`, `clippy --all-targets`, `cargo test --workspace`) at every commit:

- **Task 1** — `GetRuntime` / `HibernateRuntime` replace attach/stop across the schema, the link, the agent loop and the fake, with the four conformance tests. Commit `6f86867`.
- **Task 2** — `RuntimeManager` + `RuntimeClientProvider`, spec assembly moved out of the session, `Unavailable` / `Gone` / `Provision` taxonomy. Commit `d23682f`.
- **Tasks 3–6** — session state machine (inbox, `TurnBegan`, four states plus terminal `Unrecoverable`), resident agent with `Shutdown`, supervisor rewritten for existence-only persistence, lazy load, injectable clock and idle offload, runtime created once at session creation. Commits `3e9d76f`, `5932666`.
- **Task 7** — done as far as the server needed to compile and be honest: `202` with a message id, optional `status` on the wire, `UserMessageError` reduced to `NotFound` / `Unrecoverable`.

### Still owed

- **Task 7 remainder** — `inbox` on `SessionDetail` and an `InboxChanged` SSE event. Without them the client cannot render unread messages, so Task 8 is blocked on it.
- **Task 8** — the whole web client.
- **Task 9** — the e2e suite was ported to the new contract (statuses, `202`, `get`/`hibernate` signals) and two tests whose premise was the `409` path were deleted. The invariants that replace them — queue-and-merge across a restart, hibernate on the idle clock through HTTP, `Gone` terminal vs `Unavailable` retryable — are not yet written.
- **Agent-side gaps from Task 4**: `run_id` still needs to replace the internal staleness check, and the tool-call repair is still recomputed per turn rather than journaled once (`repair_unanswered_tool_calls` rename not yet applied).
- **Vendor conformance against the real agent loop**: the four tests run against `FakeRuntimeVendor`. The per-id `lifecycle_locks` in `runtime-vendor/src/vendor.rs` are unexercised by them.

## Self-review notes

- Spec sections map to tasks: components/tiers → 2, 5; lifecycle invariants → 5, 6; state machine → 3; RuntimeManager → 2; vendor contract → 1; agent actor → 4; deletions → spread across 1, 3, 4, 5, 7; wire/client → 7, 8; testing → every task plus 9.
- `RuntimeError` is defined once in Task 2 and consumed with the same variant names in Tasks 3, 4 and 6.
- `SessionStatus` variant names are fixed in Task 3 and used unchanged in 5, 7, 8.
- Out of scope, unchanged by this plan: `FileJournal` snapshotting, real vendor-side suspend, session fork, credential refresh (#96).
