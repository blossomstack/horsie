# Session history and events API Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the session read APIs agent-scoped and served entirely from actor state, so `recover()` becomes the only journal reader in the server.

**Architecture:** Three categories of observable — transcript appends (cursored by message id), current values (documents), and ephemeral frames (live only). Each agent in a session is an addressable resource with a document, a history endpoint, and its own SSE stream. Durable frames are published by a new post-persist hook on the actor framework rather than by re-reading the journal.

**Tech Stack:** Rust (axum 0.7, tokio, fluorite codegen for wire types), React + TanStack Query (web client), Playwright (web e2e).

Design spec: `docs/superpowers/specs/2026-08-02-session-history-and-events-api-design.md`

## Global Constraints

- No backward compatibility. This is a breaking wire change; no aliases, no deprecation shims.
- Wire types are generated from `models/fluorite/*.fl` — never hand-edit `models/src/generated/` or `clients/web/src/generated/`. Regenerate with `make models` (verify the exact target in the `Makefile` before first use).
- Production code bans panic-prone constructs (`clippy.toml`); test modules opt out per-file with `#![cfg_attr(test, allow(clippy::unwrap_used, ...))]`.
- `clippy.toml` disallows `Journal::replay` outside the actor crate. Any remaining call site needs an `#[expect(clippy::disallowed_methods, reason = "...")]` — after this plan there should be none in the server.
- CI runs nightly rustfmt with import wrapping; local stable `cargo fmt` passes things CI rejects. Verify formatting via CI, not local nightly.
- Verify with `make check` (fmt + clippy + `cargo test --workspace`). Web e2e: `bun install --frozen-lockfile`, `playwright install chromium`, `cargo build -p horsie-server -p horsie-runtime -p horsie-mock-llm`, `bun run build`, then `HORSIE_E2E_SKIP_BUILD=1 ./node_modules/.bin/playwright test`. It is bun, not npm.
- Every new `AgentState` field needs `#[serde(default)]`. `AgentState` becomes a durability contract once the SQLite work writes snapshots.

---

### Task 1: Post-persist hook on the actor framework

The doorbell (`SessionFrame::Journaled`) exists because nothing tells an actor "these events are now durable and folded". `run_actor` knows exactly that. Add a hook so publication can replace journal re-reading.

**Files:**
- Modify: `actor/src/actor.rs` (add trait method)
- Modify: `actor/src/runtime.rs` (call it in `run_actor` after a successful persist)
- Test: `actor/src/runtime.rs` (existing `mod tests`)

**Interfaces:**
- Produces: `EventSourcedActor::on_events_persisted(&mut self, events: &[Self::Event], state: &Self::State)` — async, default no-op body. Called once per successful persist, after the fold, before snapshot/ack/stop. Not called when the persist failed or the batch was empty.

- [ ] **Step 1: Write the failing test**

In `actor/src/runtime.rs` `mod tests`, extend the `Counter` actor with a `persisted: Arc<Mutex<Vec<i64>>>` field recording each folded batch, implement `on_events_persisted` to push the state value, and add:

```rust
#[tokio::test]
async fn on_events_persisted_runs_after_the_fold() {
    let journal = Arc::new(InMemoryJournal::new());
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let actor = spawn_root(
        Counter { id: "hook".into(), report: None, persisted: seen.clone() },
        journal,
    );
    actor.tell(CounterCmd::Inc(3)).await.unwrap();
    actor.tell(CounterCmd::Inc(4)).await.unwrap();
    assert_eq!(current_value(&actor).await, 7);
    // The hook observes state AFTER the fold, so it sees 3 then 7 — never 0.
    assert_eq!(*seen.lock().unwrap(), vec![3, 7]);
}

#[tokio::test]
async fn on_events_persisted_is_skipped_when_the_write_fails() {
    let journal = Arc::new(
        crate::testkit::FaultyJournal::wrapping(InMemoryJournal::new()).fail_persist_after(0),
    );
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let actor = spawn_root(
        Counter { id: "hookfail".into(), report: None, persisted: seen.clone() },
        journal,
    );
    let durable = actor.ask(|ack| CounterCmd::IncAck(5, ack)).await.unwrap();
    assert!(durable.is_err());
    // Nothing was journaled, so nothing may be published.
    assert!(seen.lock().unwrap().is_empty());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p horsie-actor on_events_persisted`
Expected: FAIL — no method `on_events_persisted`, and `Counter` has no `persisted` field.

- [ ] **Step 3: Write minimal implementation**

In `actor/src/actor.rs`, add to the `EventSourcedActor` trait:

```rust
    /// Called after a batch of events is durably written AND folded into
    /// `state`, once per successful persist. Never called for an empty batch
    /// or a failed write, so an observer here can only ever see history that
    /// really exists. Best-effort side channel: publish frames, do not persist.
    async fn on_events_persisted(&mut self, _events: &[Self::Event], _state: &Self::State) {}
```

In `actor/src/runtime.rs` `run_actor`, after `persist_events` returns and before the snapshot block, capture whether the batch was non-empty and call the hook on success. `persist_events` currently consumes `events`; change it to return them alongside the state so the hook can borrow them:

```rust
        let result;
        let persisted;
        (state, persisted, result) =
            persist_events::<A>(&pid, &journal, events, state, &mut seq_nr).await;

        if result.is_ok() && !persisted.is_empty() {
            actor.on_events_persisted(&persisted, &state).await;
        }
```

and in `persist_events`, fold by reference so the vector survives:

```rust
    for event in &events {
        state = A::apply_event(state, event.clone());
        *seq_nr += 1;
    }
    (state, events, Ok(()))
```

This requires `A::Event: Clone`. Add that bound to the `EventSourcedActor` trait's associated type. Verify `AgentDomainEvent`, `WorkflowDomainEvent`, `SessionDomainEvent`, and `SessionSupervisorEvent` all already derive `Clone` (they do at time of writing); add the derive where missing.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p horsie-actor`
Expected: PASS, including the pre-existing suite.

- [ ] **Step 5: Commit**

```bash
git add actor/src/actor.rs actor/src/runtime.rs
git commit -m "feat(actor): add on_events_persisted hook"
```

---

### Task 2: Agent-side reads and publication

Give `AgentActor` a forward history cursor and a durable-frame observer; delete every journal-reading command.

**Files:**
- Modify: `workflow/src/agent_actor.rs`
- Test: `workflow/src/agent_actor.rs` (existing `mod tests`), `workflow/tests/workflow_e2e.rs`

**Interfaces:**
- Consumes: `EventSourcedActor::on_events_persisted` (Task 1).
- Produces:
  - `HistoryQuery { before: Option<String>, after: Option<String>, limit: usize }`
  - `AgentHistoryPage { messages, has_more_before: bool, has_more_after: bool }` — `tasks`/`usage` removed.
  - `AgentCommand::GetState { reply: oneshot::Sender<AgentStateView> }` where
    `AgentStateView { tasks: Vec<TaskRecord>, usage: UsageTotal, last_turn_usage: Option<Usage>, context_tokens: u32 }`
    (named distinctly from the wire `AgentDocument` in Task 3 — they are different layers)
  - `pub trait AgentObserver: Send + Sync` with
    `fn publish(&self, event: &AgentDomainEvent, state: &AgentState)`
  - `AgentActor::new(ctx, params)` unchanged; new `AgentActor::with_observer(ctx, params, Arc<dyn AgentObserver>)`
- Removes: `AgentCommand::ReplayEvents`, `AgentCommand::HeadSeq`, `AgentActor::replay_journal`, `AgentParams::compact_on_pause`, and the `interactive`-gated `and_snapshot()` calls that used it (snapshotting is now unconditional at those pause points).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn history_after_cursor_pages_forward() {
    let state = state_with_messages(&["m1", "m2", "m3", "m4"]);
    let page = state.history_page(&HistoryQuery {
        before: None,
        after: Some("m2".into()),
        limit: 2,
    });
    assert_eq!(ids(&page.messages), vec!["m3", "m4"]);
    assert!(!page.has_more_after, "m4 is the last message");
    assert!(page.has_more_before, "m1 and m2 precede this window");
}

#[test]
fn history_after_unknown_cursor_returns_nothing_owed() {
    let state = state_with_messages(&["m1", "m2"]);
    let page = state.history_page(&HistoryQuery {
        before: None,
        after: Some("ghost".into()),
        limit: 10,
    });
    assert!(page.messages.is_empty());
    assert!(!page.has_more_after);
}

#[test]
fn history_tail_reports_more_before_but_not_after() {
    let state = state_with_messages(&["m1", "m2", "m3"]);
    let page = state.history_page(&HistoryQuery { before: None, after: None, limit: 2 });
    assert_eq!(ids(&page.messages), vec!["m2", "m3"]);
    assert!(page.has_more_before);
    assert!(!page.has_more_after);
}

#[tokio::test]
async fn observer_sees_appends_after_they_are_durable() {
    // A recording observer, an InMemoryJournal, one InputMessage persisted.
    // Assert the observer received exactly one AgentDomainEvent::InputMessage
    // and that the state it was handed already contains that message.
}
```

Add helpers `state_with_messages(ids: &[&str]) -> AgentState` (pushes `Message` values with those ids) and `ids(&[Message]) -> Vec<&str>` in the test module.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p horsie-workflow history_after`
Expected: FAIL — `HistoryQuery` has no `after` field.

- [ ] **Step 3: Implement**

Rewrite `history_page` to resolve a window from either cursor, defaulting to the tail:

```rust
    pub fn history_page(&self, query: &HistoryQuery) -> AgentHistoryPage {
        let (start, end) = match (&query.before, &query.after) {
            // Forward page: everything after `after`, capped at `limit`.
            (_, Some(id)) => match self.messages.iter().position(|m| &m.id == id) {
                Some(pos) => {
                    let start = pos + 1;
                    (start, (start + query.limit).min(self.messages.len()))
                }
                // Unknown cursor: nothing is owed. The caller re-seeds from the tail.
                None => (self.messages.len(), self.messages.len()),
            },
            // Backward page: the `limit` messages before `before`.
            (Some(id), None) => {
                let end = self
                    .messages
                    .iter()
                    .position(|m| &m.id == id)
                    .unwrap_or(self.messages.len());
                (end.saturating_sub(query.limit), end)
            }
            // Tail.
            (None, None) => {
                let end = self.messages.len();
                (end.saturating_sub(query.limit), end)
            }
        };
        AgentHistoryPage {
            messages: self.messages[start..end].to_vec(),
            has_more_before: start > 0,
            has_more_after: end < self.messages.len(),
        }
    }
```

Add the `AgentObserver` trait and an `observer: Option<Arc<dyn AgentObserver>>` field on `AgentActor`, then implement the hook:

```rust
    async fn on_events_persisted(&mut self, events: &[AgentDomainEvent], state: &AgentState) {
        let Some(observer) = &self.observer else { return };
        for event in events {
            observer.publish(event, state);
        }
    }
```

Add `AgentCommand::GetState`, returning `AgentStateView` built from `state`. Delete `ReplayEvents`, `HeadSeq`, `replay_journal`, and `compact_on_pause` (replacing its three call sites with unconditional `and_snapshot()` / `CommandEffect::snapshot()`).

- [ ] **Step 4: Run tests**

Run: `cargo test -p horsie-workflow`
Expected: PASS. Fix `workflow_e2e.rs` fallout from the `HistoryQuery`/`AgentHistoryPage` shape change.

- [ ] **Step 5: Commit**

```bash
git add workflow/src/agent_actor.rs workflow/tests/workflow_e2e.rs
git commit -m "feat(workflow): forward history cursor and durable-frame observer"
```

---

### Task 3: Wire types

**Files:**
- Modify: `models/fluorite/session.fl`, `models/fluorite/session_api.fl`
- Regenerate: `models/src/generated/`, `clients/web/src/generated/`

- [ ] **Step 1: Edit the fluorite definitions**

Split `SessionEvent` into two unions and reshape the documents:

```
/// One transcript append. `message.id` is the SSE id and the history cursor.
struct AppendedEvent { message: Message }
struct ResyncEvent { }

#[type_tag = "type"]
union AgentEvent {
    Appended(AppendedEvent),
    Delta(DeltaEvent),
    ToolStart(ToolStartEvent),
    TurnCompleted(TurnCompletedEvent),
    TaskListChanged(TaskListEvent),
    Resync(ResyncEvent),
}

#[type_tag = "type"]
union SessionEvent {
    StatusChanged(StatusChangedEvent),
    InboxChanged(InboxChangedEvent),
    Progressed(ProgressionEvent),
    Error(ErrorEvent),
    AgentTreeChanged(AgentTreeEvent),
}

struct AgentTreeEvent { agents: Vec<SubAgentView> }

/// Current values for one agent. Subagent-only fields are absent for `main`.
struct AgentDocument {
    id: String,
    tasks: Vec<TaskItem>,
    usage: UsageView,
    last_turn_usage: Option<Usage>,
    context_tokens: u32,
    context_window: Option<u32>,
    label: Option<String>,
    task: Option<String>,
    parent: Option<String>,
    depth: Option<u32>,
    status: Option<String>,
    output: Option<String>,
    error: Option<String>,
}

struct HistoryPage {
    messages: Vec<Message>,
    has_more_before: bool,
    has_more_after: bool,
}
```

Delete `MessageEvent`, `ToolOutputEvent`, `GetSessionUsageResponse`, `SessionUsageStats`, `AgentUsageView`. Add `usage_total: UsageView`, `agents: Vec<SubAgentView>`, and `progression: Option<ProgressionEvent>` (the current preparation stage) to `SessionDetail`; delete its `pending_question`.

- [ ] **Step 2: Regenerate and confirm the build breaks where expected**

Run: `make models && cargo build --workspace 2>&1 | head -40`
Expected: compile errors at every removed type's use site — that list is Task 4/5's work queue.

- [ ] **Step 3: Commit**

```bash
git add models/ clients/web/src/generated/
git commit -m "feat(models): agent-scoped session wire types"
```

---

### Task 4: Session-side frames and commands

**Files:**
- Modify: `server/src/sessions/mod.rs` (frame enums), `server/src/sessions/events.rs` (sinks), `server/src/sessions/session_actor.rs`, `server/src/sessions/supervisor.rs`

**Interfaces:**
- Produces:
  - `SessionFrame` — `Status`, `Error`, `InboxChanged`, `Progression`, `AgentTreeChanged`
  - `AgentFrame` — `Appended { message }`, `Delta { text }`, `ToolStart { .. }`, `TurnCompleted { .. }`, `TaskListChanged { .. }`, `Resync`
  - `SessionCommand::SubscribeAgent { agent_id, reply: oneshot::Sender<Option<broadcast::Receiver<AgentFrame>>> }`
  - `SessionCommand::AgentState { agent_id, reply }`
  - `SessionSupervisorCommand::{SubscribeAgent, AgentState}` forwarding the same
  - `SessionCommand::History` gains nothing — `HistoryQuery` already carries `after` from Task 2
- Removes: `SessionFrame::Journaled`, `SessionCommand::Events`, `SessionSupervisorCommand::{Events, HeadSeq, SubAgents, UsageStats}`

- [ ] **Step 1: Write the failing test**

In `session_actor.rs` tests, drive a session through one turn against the fake vendor and mock provider, subscribing to the main agent first:

```rust
#[tokio::test]
async fn agent_stream_carries_appends_without_journal_reads() {
    // Wrap the journal in a counting decorator; subscribe via SubscribeAgent;
    // send a user message; assert the received AgentFrames include
    // Appended(user) then Appended(assistant), and that the journal's
    // replay counter is still 0 after the turn.
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p horsie-server agent_stream_carries_appends`
Expected: FAIL — `SubscribeAgent` does not exist.

- [ ] **Step 3: Implement**

Split the frame enum in `mod.rs`. In `events.rs`, replace `SessionEventSink` with `AgentEventSink { frames: broadcast::Sender<AgentFrame> }` mapping `TextChunk → Delta`, `ToolCallStart → ToolStart`, and dropping the coarse arms entirely — those now arrive through the observer. Replace `QuietEventSink` usage for subagents with an `AgentEventSink` over that subagent's own broadcast.

Add a `BroadcastObserver` implementing `horsie_workflow::AgentObserver`, mapping `AgentDomainEvent` → `AgentFrame`:
`InputMessage`/`MessageComplete`/`ToolComplete` → `Appended` (using the message the fold just pushed — take `state.messages.last()`), `RunComplete` → `TurnCompleted`, `TaskListChanged` → `TaskListChanged`, everything else → nothing.

In `SessionActor`, hold `agent_frames: HashMap<AgentKey, broadcast::Sender<AgentFrame>>`, created alongside each agent actor and passed both to its sink and its observer. `SubscribeAgent` resolves `main`/uuid the same way `History` does at `session_actor.rs:1469-1484`, spawning a cold subagent on demand.

- [ ] **Step 4: Run tests**

Run: `cargo test -p horsie-server`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add server/src/sessions/
git commit -m "feat(server): per-agent frames published on persist"
```

---

### Task 5: HTTP surface

**Files:**
- Modify: `server/src/http/mod.rs` (routes), `server/src/http/handlers.rs`, `server/src/http/sse.rs`

- [ ] **Step 1: Write the failing tests**

In `server/src/http/mod.rs` tests (which already spin a real router on an ephemeral port), assert:
- `GET /api/sessions/:id/agents/main` returns an `AgentDocument`
- `GET /api/sessions/:id/agents/main/history?after=<id>` returns only later messages
- `GET /api/sessions/:id/agents/main/events` streams `Appended` frames carrying SSE ids
- `GET /api/sessions/:id` includes `agents` and `usage_total`
- `GET /api/sessions/:id/subagents` and `/usage` return 404

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p horsie-server http::`
Expected: FAIL — routes not found.

- [ ] **Step 3: Implement**

Routes:

```rust
        .route("/api/sessions/:id", get(handlers::get_session))
        .route("/api/sessions/:id/events", get(sse::session_events))
        .route("/api/sessions/:id/agents/:aid", get(handlers::get_agent))
        .route("/api/sessions/:id/agents/:aid/history", get(handlers::get_history))
        .route("/api/sessions/:id/agents/:aid/events", get(sse::agent_events))
```

`sse::agent_events` parses `Last-Event-ID` as a **message id** (a string, no `parse::<u64>()`), backfills via `SessionSupervisorCommand::History { after: Some(id), .. }` in `HISTORY_MAX_LIMIT` pages until `has_more_after` is false, then bridges to the agent broadcast. On `RecvError::Lagged`, send `Resync` rather than replaying. `sse::session_events` loses its cursor logic entirely.

Delete `get_subagents`, `get_session_usage`, `EventsParams`, `last_event_id`, and the `stamped`/`live` split (only `Appended` is stamped now, with `message.id`).

- [ ] **Step 4: Run tests**

Run: `cargo test -p horsie-server`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add server/src/http/
git commit -m "feat(server): agent-scoped HTTP and SSE routes"
```

---

### Task 6: Web client

**Files:**
- Modify: `clients/web/src/api/client.ts`, `clients/web/src/hooks/useSessionStream.ts`, `clients/web/src/hooks/useSessions.ts`, and the session view components that read `tasks`/`usage`

- [ ] **Step 1: Update the API client**

Add `agent(sessionId, agentId)`, change `history` to take `{ before?, after?, limit }` and target `/agents/:aid/history`, add `agentEventsUrl(sessionId, agentId)`, drop `sessionEventsUrl`'s `live` option, drop `usage`/`subagents`.

- [ ] **Step 2: Rework the reducer**

Delete the `toolResults` map and the `ToolResult` case — tool results arrive as `Appended` messages and flow through the same path as any other message. Add cases for `Appended`, `Resync`, `TaskListChanged`, `TurnCompleted`. Seed `tasks`/`usage` from the agent document rather than the history tail page. Track `hasMoreAfter`.

- [ ] **Step 3: Two streams**

Open the session stream and the main agent stream. On the agent stream's `onopen` after a prior error, and on `Resync`, re-fetch the agent document and `history?after=<last loaded id>`.

- [ ] **Step 4: Verify**

Run: `cd clients/web && bun run build && bun run lint`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add clients/web/src
git commit -m "feat(web): agent-scoped streams and state-seeded view"
```

---

### Task 7: CLI

**Files:**
- Modify: `cli/src/session.rs`

- [ ] **Step 1: Rework `tail`**

Replace `scan_last_seq` with `scan_last_message_id` (same buffered forward scan, reading `id` instead of `seq`). Point the request at `/agents/main/events`, send `Last-Event-ID: <message id>`, and handle a `Resync` frame by fetching `/agents/main/history?after=<cursor>` before resuming. Update `SessionSink` to stamp lines with the message id.

- [ ] **Step 2: Verify**

Run: `cargo test -p horsie-cli && cargo clippy -p horsie-cli --all-targets`
Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add cli/src/session.rs
git commit -m "feat(cli): tail the agent stream by message id"
```

---

### Task 8: End-to-end coverage and docs

**Files:**
- Modify: `tests/tests/session_server_e2e.rs`, `clients/web/e2e/`, `docs/guide/`

- [ ] **Step 1: Rust e2e**

Add a test asserting a full turn produces identical transcripts via `/history` and via accumulated `Appended` frames — the invariant the old two-vocabulary design could not state. Add a reconnect test (drop the stream mid-turn, reconnect with `Last-Event-ID`, assert no gap and no duplicate). Wrap the journal in a counting decorator and assert zero replays across the whole run.

- [ ] **Step 2: Web e2e**

Update existing specs for the new endpoints; add a subagent-panel case asserting its stream opens only when the panel opens.

- [ ] **Step 3: Docs**

Update `docs/guide/` for the new endpoints and remove references to `live=1`, `/subagents`, and `/usage`.

- [ ] **Step 4: Full verification**

Run: `make check`, then the web e2e suite per the Global Constraints.
Expected: all green.

- [ ] **Step 5: Commit and open the PR**

```bash
git add -A
git commit -m "test: state-sourced history and stream invariants"
```
