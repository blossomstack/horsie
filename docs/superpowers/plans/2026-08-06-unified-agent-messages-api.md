# Unified Agent Messages API Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace three read endpoints and two broadcast channels with one per-agent log, one cursor, and one `GET /sessions/{id}/messages` endpoint.

**Architecture:** The agent actor becomes the sole writer of a monotonically-numbered log held in its snapshotted state. Session-scoped facts reach a client because the session actor tells the target agent to append them, so no merge exists anywhere in the read path. Deltas stay out of the journal and are numbered within the entry they follow, giving a two-part cursor `(entry_seq, delta_seq)` whose staleness is arithmetically detectable. A `watch` channel carries position only, so there is no buffer to overflow.

**Tech Stack:** Rust (tokio, axum, sqlx, `horsie-actor`), fluorite schemas with Rust + TypeScript codegen, React 19 + TanStack Query, Playwright.

**Spec:** `docs/superpowers/specs/2026-08-06-unified-agent-messages-api-design.md`

## Global Constraints

- **No backward compatibility.** Old endpoints are deleted, not deprecated. Existing transcripts are wiped, not migrated.
- **Persisted shapes are a durability contract.** Every field on a snapshotted struct carries `#[serde(default)]`. Never rename or repurpose a persisted enum variant — the 2026-08-02 outage was exactly that.
- **A `.fl` edit must regenerate both type trees:** `make ts-types` (for `clients/ts`) and `cd clients/web && bun run generate-types`. CI drift-checks only `clients/ts`, so `clients/web` will silently rot if forgotten.
- **`clients/web` installs with `bun`, never `npm ci`.**
- **Wire keys are camelCase.** A snake_case key in a hand-written JSON body is silently ignored, never an error.
- **Rust iteration:** `cargo test -p <crate> --lib` while working. Full `make check` once before pushing, never twice in one command.
- **Reads never touch the journal.** Every read is answered from resident actor state.

---

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `models/fluorite/agent.fl` | log entry, body union, lifecycle union | modify |
| `models/fluorite/session.fl` | delete `AgentStreamEvent` + `SessionEvent`; add the stream frame | modify |
| `models/fluorite/session_api.fl` | `as_of_seq` on the agent view; delete `HistoryPage` | modify |
| `workflow/src/agent_log.rs` | **new** — `log_page`, cursor parsing/resolution, the three-row table | create |
| `workflow/src/agent_actor.rs` | `AgentState.log`/`next_seq`/`deltas`, seq assignment, `ReadLog`, position watch | modify |
| `server/src/sessions/lifecycle_routing.rs` | **new** — which `SessionDomainEvent` goes to which agent | create |
| `server/src/sessions/session_actor.rs` | emit lifecycle to agents; inbox moves to the agent; delete `agent_frames` | modify |
| `server/src/sessions/supervisor.rs` | delete `Subscribe`/`SubscribeAgent`/`PublishInbox` and the `frames` map | modify |
| `server/src/http/messages.rs` | **new** — the one endpoint, both forms | create |
| `server/src/http/sse.rs` | keep `global_events` only | modify |
| `server/src/http/handlers.rs` | delete `get_history`; add `as_of_seq` to `get_agent`; `aid` on send/answer | modify |
| `clients/web/src/hooks/useSessionStream.ts` | one connection, one fold, seq-compared documents | modify |

`agent_log.rs` and `lifecycle_routing.rs` are new files rather than additions to their 4276- and 7242-line neighbours: both are pure functions with a table of cases, which is exactly what wants isolated tests.

---

### Task 1: The log schema

**Files:**
- Modify: `models/fluorite/agent.fl:95-109`
- Modify: `models/fluorite/session_api.fl`
- Test: `models/src/lib.rs` (unit tests at the bottom)

**Interfaces:**
- Produces: `AgentLogEntry { seq: u64, at_ms: u64, body: AgentLogBody }`, `AgentLogBody::{Llm, Hook, Lifecycle}`, `LifecycleEvent` (11 arms), `TurnOutcome`.

- [ ] **Step 1: Replace `HistoryEntry` in `agent.fl`**

```
/// One item in an agent's log — the single ordered record a client reads.
struct AgentLogEntry {
    /// Monotonic within this agent, assigned in the fold. The cursor.
    seq: u64,
    at_ms: u64,
    body: AgentLogBody,
}

#[type_tag = "type"]
union AgentLogBody {
    Llm(Message),
    Hook(HookEntry),
    Lifecycle(LifecycleEvent),
}

#[type_tag = "kind"]
union LifecycleEvent {
    Provisioning(ProvisioningLifecycle),
    MessageQueued(QueuedLifecycle),
    TurnBegan(TurnBeganLifecycle),
    TurnEnded(TurnEndedLifecycle),
    AskRecorded(AskLifecycle),
    SubAgent(SubAgentLifecycle),
    Step(StepLifecycle),
    TaskList(TaskListLifecycle),
    SessionFailed(SessionFailedLifecycle),
}

struct ProvisioningLifecycle { stage: String, detail: Option<String> }
struct QueuedLifecycle { id: String, text: String }
struct TurnBeganLifecycle { consumed: Vec<String>, answered: Vec<String> }
struct TurnEndedLifecycle { outcome: TurnOutcome }
struct AskLifecycle { tool_call_id: Option<String>, question: String }
struct SubAgentLifecycle { id: String, label: String, status: String }
struct StepLifecycle { index: u32, status: String }
struct TaskListLifecycle { tasks: Vec<TaskItem> }
struct SessionFailedLifecycle { reason: String }

#[type_tag = "kind"]
union TurnOutcome {
    Ended(EmptyOutcome),
    Failed(FailedOutcome),
    Stopped(EmptyOutcome),
    Interrupted(EmptyOutcome),
}

struct EmptyOutcome {}
struct FailedOutcome { error: String }
```

Keep `HistoryEntry` for now — Task 2 removes it once nothing references it.

- [ ] **Step 2: Regenerate both type trees**

```bash
make ts-types
cd clients/web && bun install && bun run generate-types
```

- [ ] **Step 3: Write the failing test in `models/src/lib.rs`**

```rust
#[test]
fn a_lifecycle_entry_round_trips_with_its_tag() {
    let entry = AgentLogEntry {
        seq: 7,
        at_ms: 1_700_000_000_000,
        body: AgentLogBody::Lifecycle(LifecycleEvent::TurnEnded(TurnEndedLifecycle {
            outcome: TurnOutcome::Failed(FailedOutcome { error: "boom".into() }),
        })),
    };
    let json = serde_json::to_value(&entry).unwrap();
    assert_eq!(json["body"]["type"], "Lifecycle");
    assert_eq!(json["body"]["value"]["kind"], "TurnEnded");
    assert_eq!(json["body"]["value"]["value"]["outcome"]["kind"], "Failed");
    let back: AgentLogEntry = serde_json::from_value(json).unwrap();
    assert_eq!(back.seq, 7);
}
```

- [ ] **Step 4: Run it**

Run: `cargo test -p horsie-models --lib a_lifecycle_entry_round_trips`
Expected: PASS (codegen already produced the types; this pins the nesting).

- [ ] **Step 5: Commit**

```bash
git add models/ clients/ts/src/generated clients/web/src/generated
git commit -m "models: the agent log entry, body and lifecycle unions"
```

---

### Task 2: `AgentState` holds the log

**Files:**
- Modify: `workflow/src/agent_actor.rs:330-374` (state), `:1315-1372` (`apply_event`), `:510-518` (`prompt_messages`)
- Test: `workflow/src/agent_actor.rs` test module

**Interfaces:**
- Consumes: `AgentLogEntry`, `AgentLogBody` from Task 1.
- Produces: `AgentState { log: Vec<AgentLogEntry>, next_seq: u64, .. }`; `AgentState::prompt_messages() -> Vec<Message>` unchanged in signature.

- [ ] **Step 1: Replace the state fields**

Delete `history: Vec<HistoryEntry>`. Add:

```rust
/// The agent's log: everything a client reads, in one order.
///
/// Renamed from `history: Vec<HistoryEntry>` deliberately. State is
/// snapshotted, so this is a durability contract; renaming means serde
/// ignores the now-unknown `history` key and defaults this to empty, which
/// yields a wiped transcript rather than a failed `recover()` — the failure
/// mode a renamed *variant* caused on 2026-08-02.
#[serde(default)]
pub log: Vec<AgentLogEntry>,
/// The next seq to hand out. Deterministic on replay because the fold is.
#[serde(default)]
pub next_seq: u64,
```

- [ ] **Step 2: Assign seq in `apply_event`**

Add above the match:

```rust
fn push(state: &mut AgentState, at_ms: u64, body: AgentLogBody) {
    state.log.push(AgentLogEntry { seq: state.next_seq, at_ms, body });
    state.next_seq += 1;
}
```

Then every arm that pushed to `history` pushes through it — `InputMessage` and `MessageComplete` use `message.created_at_ms`, `ToolComplete` uses its `at_ms`, `HookRan` uses its `at_ms`.

- [ ] **Step 3: `prompt_messages` becomes a three-arm match**

```rust
pub fn prompt_messages(&self) -> Vec<Message> {
    self.log
        .iter()
        .filter_map(|e| match &e.body {
            AgentLogBody::Llm(m) => Some(m.clone()),
            AgentLogBody::Hook(h) => crate::hook_translation::translate(h),
            // Every lifecycle variant, present and future. This arm is why
            // `Lifecycle` is one union rather than eight flattened arms:
            // provider isolation cannot be forgotten for a new variant.
            AgentLogBody::Lifecycle(_) => None,
        })
        .collect()
}
```

- [ ] **Step 4: Write the determinism test**

```rust
#[tokio::test]
async fn replay_reproduces_the_same_sequence_numbers() {
    let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
    let first = drive_a_turn(&journal).await;      // helper below
    let second = recover_state(&journal).await;
    let pairs = |s: &AgentState| -> Vec<(u64, String)> {
        s.log.iter().map(|e| (e.seq, tag_of(&e.body))).collect()
    };
    assert_eq!(pairs(&first), pairs(&second), "replay must reproduce the log exactly");
    assert_eq!(second.next_seq, second.log.len() as u64);
}
```

- [ ] **Step 5: Run it**

Run: `cargo test -p horsie-workflow --lib replay_reproduces`
Expected: PASS.

- [ ] **Step 6: Delete `HistoryEntry` from `agent.fl` and `models/src/lib.rs`, regenerate, fix compile errors**

Run: `cargo check --workspace` and fix every reference. `server/src/wire_redact.rs`, `server/src/sessions/events.rs` and `server/src/http/handlers.rs` all touch it.

- [ ] **Step 7: Commit**

```bash
git add -A && git commit -m "agent: the log replaces the history vec, seq assigned in the fold"
```

---

### Task 3: Cursor and paging

**Files:**
- Create: `workflow/src/agent_log.rs`
- Modify: `workflow/src/lib.rs` (add `pub mod agent_log;`), `workflow/src/agent_actor.rs` (delete `history_page`)
- Test: `workflow/src/agent_log.rs`

**Interfaces:**
- Produces:
  ```rust
  pub struct Cursor { pub entry_seq: u64, pub delta_seq: usize }
  impl Cursor { pub fn parse(s: &str) -> Option<Cursor>; pub fn to_string(&self) -> String; }
  pub struct LogPage { pub entries: Vec<AgentLogEntry> }
  pub fn page_before(log: &[AgentLogEntry], before: Option<u64>, max: usize) -> LogPage;
  pub fn page_after(log: &[AgentLogEntry], after: u64) -> &[AgentLogEntry];
  ```

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_cursor_round_trips_in_both_forms() {
    assert_eq!(Cursor::parse("99"), Some(Cursor { entry_seq: 99, delta_seq: 0 }));
    assert_eq!(Cursor::parse("99.50"), Some(Cursor { entry_seq: 99, delta_seq: 50 }));
    assert_eq!(Cursor { entry_seq: 99, delta_seq: 0 }.to_string(), "99");
    assert_eq!(Cursor { entry_seq: 99, delta_seq: 50 }.to_string(), "99.50");
    assert_eq!(Cursor::parse("nonsense"), None);
}

#[test]
fn page_before_returns_the_window_ending_just_before_the_cursor() {
    let log = fixture(0..10);
    let page = page_before(&log, Some(5), 3);
    assert_eq!(seqs(&page.entries), vec![2, 3, 4]);
}

#[test]
fn page_before_with_no_cursor_returns_the_tail() {
    let log = fixture(0..10);
    assert_eq!(seqs(&page_before(&log, None, 3).entries), vec![7, 8, 9]);
}

#[test]
fn an_unknown_before_cursor_returns_an_empty_page() {
    // The honest answer: the caller named an entry this log does not have, so
    // it must re-seed rather than be handed a silently wrong window.
    let log = fixture(0..10);
    assert!(page_before(&log, Some(999), 3).entries.is_empty());
}

#[test]
fn page_after_is_everything_past_the_cursor() {
    let log = fixture(0..5);
    assert_eq!(seqs(page_after(&log, 2)), vec![3, 4]);
    assert!(page_after(&log, 4).is_empty());
}

#[test]
fn seq_lookup_is_a_binary_search_not_a_scan() {
    // Guards the reason seq is stored rather than implied by index: a
    // front-trimmed log must still resolve cursors correctly.
    let log = fixture(100..110);
    assert_eq!(seqs(&page_before(&log, Some(105), 2).entries), vec![103, 104]);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p horsie-workflow --lib agent_log`
Expected: FAIL, module not found.

- [ ] **Step 3: Implement `agent_log.rs`**

`page_before` binary-searches `log` by `seq` (the log is sorted by construction), returns `[start, end)` where `end` is the found index and `start = end.saturating_sub(max)`. `page_after` binary-searches and returns the tail slice. No `has_more` on either — fewer than `max` means no more.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p horsie-workflow --lib agent_log`
Expected: PASS.

- [ ] **Step 5: Delete `AgentActor::history_page` and `HistoryQuery`, fix callers**

`server/src/sessions/session_actor.rs:2978` (`SessionCommand::History`) and `server/src/http/handlers.rs:319` both go away in Task 7; for now point them at `page_before`.

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "workflow: cursor parsing and seq-addressed paging"
```

---

### Task 4: Deltas and the position watch

**Files:**
- Modify: `workflow/src/agent_actor.rs`
- Test: `workflow/src/agent_actor.rs` test module

**Interfaces:**
- Produces:
  ```rust
  pub enum ReadOutcome { Ok { entries: Vec<AgentLogEntry>, deltas: Vec<String>, reset_deltas: bool, cursor: Cursor } }
  AgentCommand::ReadLog { after: Cursor, reply: oneshot::Sender<ReadOutcome> }
  AgentCommand::PageLog { before: Option<u64>, max: usize, reply: oneshot::Sender<LogPage> }
  AgentActor::position() -> watch::Receiver<(u64, usize)>
  ```

- [ ] **Step 1: Add the in-memory fields to `AgentActor` (not `AgentState`)**

```rust
/// Chunks since the last log entry. Never serialised, never journaled, not
/// part of the fold — a delta's useful life ends when the message that
/// supersedes it lands.
deltas: Vec<String>,
/// (tail_seq, delta_count). A `watch` holds only the latest value and
/// overwrites, so a slow reader cannot fall behind it — which is what makes
/// overflow structurally impossible rather than handled.
position: watch::Sender<(u64, usize)>,
```

Push a delta where `AgentFrame::Delta` is emitted today; clear `deltas` wherever a log entry is appended. Send the new position after both.

- [ ] **Step 2: Write the three-row cursor table as tests**

```rust
#[tokio::test]
async fn a_client_behind_the_tail_gets_entries_and_no_deltas() {
    let a = agent_with(log_upto(10), deltas(&["x", "y"])).await;
    let out = a.ask(|reply| AgentCommand::ReadLog { after: Cursor { entry_seq: 5, delta_seq: 0 }, reply }).await.unwrap();
    assert_eq!(seqs(&out.entries), vec![6, 7, 8, 9, 10]);
    assert!(out.deltas.is_empty(), "live typing means nothing to a client this far behind");
    assert_eq!(out.cursor.delta_seq, 0);
}

#[tokio::test]
async fn a_caught_up_client_gets_the_deltas_after_its_own() {
    let a = agent_with(log_upto(10), deltas(&["x", "y", "z"])).await;
    let out = a.ask(|reply| AgentCommand::ReadLog { after: Cursor { entry_seq: 10, delta_seq: 1 }, reply }).await.unwrap();
    assert!(out.entries.is_empty());
    assert_eq!(out.deltas, vec!["y", "z"]);
    assert!(!out.reset_deltas);
}

#[tokio::test]
async fn a_restarted_run_is_detected_and_answered_with_a_reset() {
    // The trap this scheme exists to close. Entry 10 is still the tail after a
    // restart, but the new run has emitted fewer deltas than the client holds.
    // A flat counter would reissue the same numbers and nothing could notice.
    let a = agent_with(log_upto(10), deltas(&["a", "b"])).await;
    let out = a.ask(|reply| AgentCommand::ReadLog { after: Cursor { entry_seq: 10, delta_seq: 50 }, reply }).await.unwrap();
    assert!(out.reset_deltas, "50 > 2 is impossible unless the run restarted");
    assert_eq!(out.deltas, vec!["a", "b"]);
}
```

- [ ] **Step 3: Run to verify they fail**

Run: `cargo test -p horsie-workflow --lib cursor`
Expected: FAIL, `ReadLog` not a variant.

- [ ] **Step 4: Implement `ReadLog` and `PageLog`**

- [ ] **Step 5: Run to verify they pass**

Run: `cargo test -p horsie-workflow --lib`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "agent: deltas, the position watch, and cursor resolution"
```

---

### Task 5: Session lifecycle reaches the agent

**Files:**
- Create: `server/src/sessions/lifecycle_routing.rs`
- Modify: `server/src/sessions/session_actor.rs`, `server/src/sessions/mod.rs`
- Test: `server/src/sessions/lifecycle_routing.rs`

**Interfaces:**
- Produces:
  ```rust
  pub enum LifecycleTarget { Main, Agent(AgentKey), Parent(AgentKey), None }
  pub fn route(event: &SessionDomainEvent) -> (LifecycleTarget, Option<LifecycleEvent>);
  ```
- Consumes: `AgentCommand::RecordLifecycle { event: LifecycleEvent }` (added here), journaled as `AgentDomainEvent::LifecycleRecorded { event, at_ms }`.

- [ ] **Step 1: Write the routing-completeness test**

```rust
/// Every viewer-facing session event must have a destination. Forgetting one
/// means a fact that silently never reaches a client, which is not a failure
/// any other test would catch.
#[test]
fn every_viewer_facing_session_event_reaches_an_agent() {
    for event in every_session_domain_event_variant() {
        let (target, payload) = route(&event);
        match event {
            SessionDomainEvent::UsageRecorded { .. }
            | SessionDomainEvent::SubAgentRunning { .. }
            | SessionDomainEvent::SubAgentNotified { .. } => {
                assert!(matches!(target, LifecycleTarget::None), "{event:?} is bookkeeping");
                assert!(payload.is_none());
            }
            _ => {
                assert!(!matches!(target, LifecycleTarget::None), "{event:?} has no destination");
                assert!(payload.is_some(), "{event:?} routes but carries nothing");
            }
        }
    }
}

#[test]
fn a_spawned_subagent_is_recorded_on_its_parent() {
    let parent = Uuid::new_v4();
    let (target, _) = route(&SessionDomainEvent::SubAgentSpawned {
        at_ms: 1, id: Uuid::new_v4(), parent: SubAgentParent::Sub(parent),
        label: "l".into(), task: "t".into(), depth: 1,
    });
    assert_eq!(target, LifecycleTarget::Agent(AgentKey::Sub(parent)));
}
```

`every_session_domain_event_variant()` is an explicit `vec![]` of all 23, not a derive — the point is that adding a variant breaks compilation of this list until someone decides where it goes.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p horsie-server --lib lifecycle_routing`
Expected: FAIL, module not found.

- [ ] **Step 3: Implement `lifecycle_routing.rs`** per the spec's routing table.

- [ ] **Step 4: Add `AgentDomainEvent::LifecycleRecorded` and its fold arm**

```rust
AgentDomainEvent::LifecycleRecorded { event, at_ms } => {
    push(&mut state, at_ms, AgentLogBody::Lifecycle(event));
}
```

- [ ] **Step 5: Emit from the session actor**

Wherever `CommandEffect::persist` returns `SessionDomainEvent`s, route each through `route()` and `tell` the resolved agent. Resolve with the existing `resolve_agent`, which spawns a cold subagent — correct here, since an event it must record is a reason to load it.

- [ ] **Step 6: Run tests**

Run: `cargo test -p horsie-server --lib lifecycle`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add -A && git commit -m "sessions: route viewer-facing session events into the agent log"
```

---

### Task 6: The inbox moves to the agent

**Files:**
- Modify: `workflow/src/agent_actor.rs`, `server/src/sessions/session_actor.rs`, `server/src/http/handlers.rs`
- Test: both actors' test modules

**Interfaces:**
- Produces: `AgentCommand::Enqueue { id, text, reply }` → `PersistAndAck`; `AgentCommand::SetRuntimeReady { ready: bool }`; `AgentState.inbox: Vec<InboxMessage>`.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn an_enqueued_message_is_durable_before_the_ack_returns() {
    let (agent, journal) = agent_and_journal().await;
    agent.ask(|reply| AgentCommand::Enqueue { id: "m1".into(), text: "hi".into(), reply }).await.unwrap();
    // Read the journal directly: the ack must not precede the write.
    let state = recover_state(&journal).await;
    assert_eq!(state.inbox.len(), 1);
    assert!(matches!(state.log.last().unwrap().body, AgentLogBody::Lifecycle(LifecycleEvent::MessageQueued(_))));
}

#[tokio::test]
async fn a_message_waits_while_the_runtime_is_not_ready() {
    let agent = agent_not_ready().await;
    agent.ask(|reply| AgentCommand::Enqueue { id: "m1".into(), text: "hi".into(), reply }).await.unwrap();
    assert!(!agent_is_running(&agent).await, "provisioning gates the turn, not the accept");
    agent.tell(AgentCommand::SetRuntimeReady { ready: true }).await.unwrap();
    assert!(eventually(|| agent_is_running(&agent)).await);
}

#[tokio::test]
async fn recovery_drains_nothing() {
    // Unchanged behaviour, asserted because the inbox moved: a queued message
    // waits for the user, and auto-restart stays out of scope.
    let (_, journal) = agent_with_queued_message().await;
    let agent = recover_agent(&journal).await;
    assert!(!agent_is_running(&agent).await);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p horsie-workflow --lib enqueue`
Expected: FAIL.

- [ ] **Step 3: Implement.** `Enqueue` persists `MessageQueued` + the log entry with `CommandEffect::PersistAndAck`. The agent starts a turn when it is not running, not parked, and `runtime_ready`. `SetRuntimeReady` is in-memory only, seeded at spawn by the session and pushed on `ProvisioningSucceeded`.

- [ ] **Step 4: Rewire the session actor.** `SessionCommand::UserMessage` resolves the agent and forwards. Delete `SessionState.inbox` and the `MessageQueued`/`TurnBegan` session events once nothing folds them.

- [ ] **Step 5: Add `aid` to the HTTP handlers**

`POST /sessions/{id}/messages?aid=` and `POST /sessions/{id}/answers?aid=`, both defaulting to `main`. Return `{ id, seq }` from send.

- [ ] **Step 6: Run tests**

Run: `cargo test -p horsie-workflow --lib && cargo test -p horsie-server --lib`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add -A && git commit -m "agent: owns its inbox and decides when a message becomes a turn"
```

---

### Task 7: The endpoint

**Files:**
- Create: `server/src/http/messages.rs`
- Modify: `server/src/http/mod.rs:140-155`, `server/src/http/sse.rs`, `server/src/http/handlers.rs`, `server/src/sessions/supervisor.rs`
- Test: `tests/tests/session_server_e2e.rs`

**Interfaces:**
- Produces: `GET /api/sessions/{id}/messages?aid=&after=&before=&max=`; `SessionSupervisorCommand::ReadLog`/`PageLog`/`Position`.

- [ ] **Step 1: Write the failing e2e tests**

```rust
#[tokio::test]
async fn the_stream_replays_from_the_beginning_then_goes_live() {
    let server = server_with_session().await;
    let events = collect_sse(&url("?aid=main"), |evs| evs.len() >= 3).await;
    let seqs: Vec<u64> = events.iter().filter_map(|e| e.id.parse().ok()).collect();
    assert_eq!(seqs, (0..seqs.len() as u64).collect::<Vec<_>>(), "no gaps, no reordering");
}

#[tokio::test]
async fn last_event_id_resumes_exactly_where_it_left_off() {
    let server = server_with_session().await;
    let first = collect_sse(&url("?aid=main"), |e| e.len() >= 2).await;
    let cursor = first.last().unwrap().id.clone();
    let second = collect_sse_with_cursor(&url("?aid=main"), &cursor, |e| !e.is_empty()).await;
    assert!(second[0].id.parse::<u64>().unwrap() > cursor.parse().unwrap());
}

#[tokio::test]
async fn a_page_returns_and_closes_with_no_has_more() {
    let body: serde_json::Value = get(&url("?aid=main&max=2&before=5")).await;
    assert_eq!(body["entries"].as_array().unwrap().len(), 2);
    assert!(body.get("hasMore").is_none() && body.get("hasMoreBefore").is_none());
}

#[tokio::test]
async fn an_unknown_agent_is_a_404() {
    assert_eq!(status(&url("?aid=00000000-0000-0000-0000-000000000000")).await, 404);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p horsie-tests --test session_server_e2e the_stream_replays`
Expected: FAIL, 404 — route not registered.

- [ ] **Step 3: Implement `messages.rs`.** `before` present → page form, returns JSON and closes. Otherwise stream: resolve the cursor from `after=` or `Last-Event-ID`, loop on `position.changed()`, `ReadLog`, write entries with `id: {seq}` and deltas with `id: {seq}.{k}`.

- [ ] **Step 4: Delete the old routes and their machinery**

`get_history`, `agent_events`, `session_events`; `AgentStreamEvent`, `SessionEvent`, `HistoryPage` from the schemas; `Subscribe`, `SubscribeAgent`, `PublishInbox` from the supervisor; the `frames` map, `agent_frames`, `FRAME_BROADCAST_CAPACITY`, `STREAM_BUFFER`, `MAX_BACKFILL_PAGES`, `BACKFILL_LIMIT`, and the `Resync` frame. Keep `global_events`.

- [ ] **Step 5: Add `as_of_seq` to `get_agent`**

```rust
// Stamped with the log position it reflects, so a consumer holding a fold can
// tell a document that is *ahead* of it from one that is behind. A boolean
// latch cannot, which is why `tasksLive` needs `Resync` to reach in and
// release it — and why no guard over an unstamped read could ever be correct.
as_of_seq: u64,
```

- [ ] **Step 6: Run tests**

Run: `cargo test -p horsie-tests --test session_server_e2e`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add -A && git commit -m "http: one messages endpoint replaces history and both event streams"
```

---

### Task 8: The client

**Files:**
- Modify: `clients/web/src/hooks/useSessionStream.ts`, `clients/web/src/api/client.ts`, `clients/web/src/pages/SessionView.tsx`
- Test: `clients/web/src/hooks/useSessionStream.test.ts` (new), `clients/web/e2e/e-progress-ux.spec.ts`

**Interfaces:**
- Consumes: the endpoint from Task 7.
- Produces: `useSessionStream` with `{ items, deltas, status, queued, tasks, hasMoreBefore, loadingMore }` folded from one source.

- [ ] **Step 1: Write the fold-parity test**

```ts
// The cost this design accepts: the client owns a fold that must match the
// server's. One fixture, both sides, asserted equal.
it("folds the same status the server does", () => {
  const log = readFixture("log-with-failed-turn.json");
  expect(foldStatus(log)).toEqual(readFixture("expected-status.json"));
});

it("prefers the fresher of a document read and the fold", () => {
  const s = init();
  apply(s, { type: "document", asOfSeq: 200, tasks: [{ id: "a" }] });
  apply(s, { type: "entry", seq: 150, body: taskList([{ id: "b" }]) });
  expect(s.tasks).toEqual([{ id: "a" }]);   // seq 150 < asOfSeq 200
  apply(s, { type: "entry", seq: 201, body: taskList([{ id: "c" }]) });
  expect(s.tasks).toEqual([{ id: "c" }]);
});
```

- [ ] **Step 2: Run to verify they fail**

Run: `cd clients/web && bun run test useSessionStream`
Expected: FAIL.

- [ ] **Step 3: Rewrite the hook.** One `EventSource`. Delete `seed-queue`, `sawLiveInbox`, `needsResync` and its effect, `hookEntryIds`, the `seen` set, `liveStatus`/`statusSeq`/`statusReason`/`livePendingAsks`, `progression`, and the `useSession` read. Replace `tasksLive`/`errorLive` with per-value `setFromSeq` compared against `asOfSeq`. `optimistic` stays; its kill signal is the `seq` returned by the POST.

- [ ] **Step 4: Run tests**

Run: `cd clients/web && bun run test && bunx tsc -b`
Expected: PASS. (`tsc --noEmit` is a no-op in this repo — use `tsc -b`.)

- [ ] **Step 5: Run the e2e suite**

Run: `cd clients/web && TMPDIR=/tmp bunx playwright test e-progress-ux`
Expected: PASS. `TMPDIR=/tmp` is required — Playwright's global setup dies under the default macOS `$TMPDIR` on the `sun_path` limit.

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "web: one connection, one fold, seq-compared documents"
```

---

## Final verification

- [ ] `make check` (fmt-check, clippy, test) — once, before pushing.
- [ ] `cd clients/web && bun run generate-types && git diff --exit-code src/generated` — both type trees current.
- [ ] `TMPDIR=/tmp bunx playwright test` — full e2e.
- [ ] Open the PR.

## Self-review notes

Spec coverage checked section by section: endpoint (Task 7), three request forms (3, 7), stream framing (7), log type (1, 2), seq assignment (2), deltas (4), cursor resolution table (4), routing table (5), write path (6), provisioning gate (6), backpressure (4, 7), `as_of_seq` (7, 8), deletions (7, 8), testing (each task). No spec section is unrepresented.

Naming checked across tasks: `AgentLogEntry.seq`, `Cursor { entry_seq, delta_seq }`, `page_before`/`page_after`, `ReadLog`/`PageLog`, `LifecycleTarget`, `as_of_seq` are used identically wherever they appear.
