# Answerable asks — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make an `ask_user` question answerable in every state a session can be in — mixed with other tool calls, several at once, before and after idle offload.

**Architecture:** Three PRs, each green and useful alone. PR 1 fixes agentcore so a park (`ask_user`) may be issued alongside other tool calls and so a rejected handoff is journaled. PR 2 fixes the session lifecycle: the frame channel outlives the actor, session reads go through the actor, and the journal handle leaves the HTTP layer. PR 3 makes several asks answerable at once, atomically, end to end.

**Tech Stack:** Rust (tokio, axum, serde), an event-sourced actor runtime (`horsie_actor`), React + TanStack Query + Playwright.

**Spec:** `docs/superpowers/specs/2026-08-02-answerable-asks-design.md`

## Global Constraints

- Persisted event variants are never renamed or retyped. New variants and `#[serde(default)]` fields only — a rename took the homelab down in #101.
- `make check` (fmt + clippy `-D warnings` + tests) must pass before every commit. `cargo fmt` runs *before* clippy.
- Wire types are generated from `models/fluorite/*.fl`; regenerate TS with `make ts-types` after changing them and commit the generated output.
- Handoff tools are never executed by a toolbox.
- No journal read outside the actor that owns it (enforced by Task 7).
- Never list Claude as an author or co-author.

---

# PR 1 — a park is not a conclusion (agentcore)

Branch: `fix/answerable-asks`. Fixes the duplicate-question defect at its root.

### Task 1: An optional handoff may be called alongside other tools

**Files:**
- Modify: `agentcore/src/agent.rs` (`validate_handoff`, the handoff branch of `run`, the tool batch)
- Test: `agentcore/src/agent.rs` (inline `mod tests`)

**Interfaces:**
- Produces: `Agent::execute_tool_calls(&self, calls: &[(String, String, Value)], events: &dyn EventSink, cancel: &CancellationToken) -> Result<Vec<Message>, AgentError>` — runs a batch concurrently, emitting `ToolExecuting`/`ToolComplete` per call, results in request order.
- Produces: `validate_handoff` rejects only when the handoff tool is called more than once, when a *forced* handoff has siblings, or when input fails the schema.

- [ ] **Step 1: Write the failing tests**

Three tests in `agentcore/src/agent.rs`'s test module, with helpers `tool_results(&CollectingEventSink) -> Vec<(String, String, bool)>` (every `ToolComplete` as `(tool_call_id, output, is_error)`), `toolbox_where(names: &[&str], never: &'static str)` (executing `never` asserts), and `calls_response(Vec<(&str, &str, Value)>) -> CompletionResponse`:

- `an_optional_handoff_alongside_other_tools_runs_them_then_hands_off` — provider returns one response calling `notes` and `ask`; expect `AgentResult::Handoff` naming `ask`, exactly one `ToolComplete` (for `notes`, `is_error == false`), and `provider.calls() == 1` (no nudge round trip).
- `a_rejected_handoff_records_a_result_for_every_call_in_the_turn` — forced handoff `finish` called with `notes`, then `finish` alone; expect `Handoff`, and `ToolComplete` for both `t1` and `h1` with `is_error == true`.
- `two_calls_to_the_handoff_tool_in_one_turn_are_rejected` — two `ask` calls, then one; expect `Handoff`, and `ToolComplete` for both rejected ids.

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test -p horsie-agentcore --lib`
Expected: test 1 fails with the script exhausted (the rejection caused an extra provider call); tests 2 and 3 fail with `left: []` (no `ToolComplete` emitted).

- [ ] **Step 3: Split `validate_handoff` by handoff kind**

```rust
if tool_calls.iter().filter(|(_, n, _)| n == handoff_name).count() > 1 {
    return Some(format!(
        "Call '{handoff_name}' at most once per turn — wait for the answer before asking again."
    ));
}
if self.force_handoff_choice && tool_calls.len() > 1 {
    return Some(format!(
        "The '{handoff_name}' tool must be called on its own, with no other tool calls in the same turn."
    ));
}
```

- [ ] **Step 4: Extract the tool batch into `execute_tool_calls`**

Move the `join_all` batch out of `run` verbatim into a method returning `Result<Vec<Message>, AgentError>`, returning `Ok(Vec::new())` for an empty slice, keeping the `tokio::select!` cancellation arm. `run`'s normal path becomes:

```rust
for message in self.execute_tool_calls(&tool_calls, events, &cancel).await? {
    self.history.push(message);
}
```

- [ ] **Step 5: Run the siblings before parking**

In the accepted-handoff arm, before emitting `RunComplete`:

```rust
let siblings: Vec<(String, String, Value)> = tool_calls
    .iter()
    .filter(|(_, n, _)| n != &handoff_name)
    .cloned()
    .collect();
for message in self.execute_tool_calls(&siblings, events, &cancel).await? {
    self.history.push(message);
}
```

- [ ] **Step 6: Emit the rejection results**

In the rejection arm, before each `self.history.push(Message::tool_result(...))`:

```rust
events.emit(AgentEvent::ToolComplete(ToolCompleteEvent {
    message_id: format!("result:{tool_call_id}"),
    tool_call_id: tool_call_id.clone(),
    output: content.clone(),
    is_error: true,
})).await?;
```

- [ ] **Step 7: Run the full suite**

Run: `cargo test -p horsie-agentcore --lib`
Expected: PASS, including the pre-existing handoff tests (`test_handoff_schema_validation_retries_then_succeeds`, `test_handoff_validation_fails_after_max_retries`).

- [ ] **Step 8: Run the gate and commit**

```bash
make fmt && make clippy && cargo test -p horsie-agentcore -p horsie-workflow -p horsie-server
git commit -am "An optional handoff may be called alongside other tools"
```

### Task 2: Open PR 1

- [ ] **Step 1: Full gate**

Run: `make check`
Expected: PASS.

- [ ] **Step 2: Push and open**

```bash
git push -u origin fix/answerable-asks
gh pr create --title "A park is not a conclusion: ask_user alongside other tools" --body "<body>"
```

Body states: the model bundled `task_list` with `ask_user`; the turn was rejected and re-issued, leaving two asks in the transcript with the rejection unjournaled. Links the spec. Notes multi-ask follows in a later PR.

- [ ] **Step 3: Watch CI to green**

Run: `gh pr checks --watch`

---

# PR 2 — session lifecycle (server)

Branch: `fix/session-reads-via-actor`, off PR 1's head.

### Task 3: The frame channel outlives the actor

**Files:**
- Modify: `server/src/sessions/supervisor.rs` (add the registry, `ensure_loaded`, `forget`, `Subscribe`, `Delete`)
- Modify: `server/src/sessions/session_actor.rs` (`SessionActor::new` takes the sender)
- Test: `server/src/sessions/supervisor.rs` (inline `mod tests`)

**Interfaces:**
- Produces: `SessionActor::new(id: Uuid, spec: SessionSpec, deps: SessionDeps, parent: ActorRef<SessionSupervisorCommand>, frames: broadcast::Sender<SessionFrame>)`.
- Produces: `SessionSupervisor.frames: BTreeMap<SessionId, broadcast::Sender<SessionFrame>>`.

- [ ] **Step 1: Write the failing test**

`a_subscriber_survives_an_offload`: subscribe to a session, drive an idle offload (the existing tests' clock + `Tick` pattern), assert the receiver has *not* closed (`rx.try_recv()` is `Err(TryRecvError::Empty)`, not `Closed`), then send a user message and assert a frame arrives on the same receiver.

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p horsie-server sessions::supervisor -- a_subscriber_survives_an_offload`
Expected: FAIL — the receiver reports `Closed` once the actor stops.

- [ ] **Step 3: Move the channel to the supervisor**

`ensure_loaded` looks up or inserts `broadcast::channel(FRAME_BROADCAST_CAPACITY).0` and passes a clone to `SessionActor::new`. `SessionActor::new` takes it instead of creating one.

- [ ] **Step 4: Keep watched channels, drop unwatched ones**

```rust
fn forget(&mut self, id: &SessionId) {
    self.children.remove(id);
    self.status.remove(id);
    self.last_activity.remove(id);
    // A live subscriber outlives the actor: the stream is transport, and an
    // unloaded session simply has nothing to say until something reloads it.
    if self.frames.get(id).is_none_or(|tx| tx.receiver_count() == 0) {
        self.frames.remove(id);
    }
}
```

`Delete` removes the entry unconditionally.

- [ ] **Step 5: Answer `Subscribe` from the registry**

`Subscribe` returns `self.frames.entry(id).or_insert_with(...).subscribe()` without `ensure_loaded`.

- [ ] **Step 6: Run and commit**

Run: `cargo test -p horsie-server` then `make fmt && make clippy`
Commit: `git commit -am "Session frame channel outlives its actor"`

### Task 4: `Get` asks the actor

**Files:**
- Modify: `server/src/sessions/session_actor.rs` (new `SessionCommand::Snapshot`, `on_recovery_complete`)
- Modify: `server/src/sessions/supervisor.rs` (`Get` → `ensure_loaded` + `Snapshot`)
- Modify: `server/src/http/handlers.rs` (`get_session` consumes the snapshot)
- Delete: `fold_session_state` from `server/src/sessions/events.rs` and its test
- Test: `server/src/sessions/session_actor.rs`, `server/src/http/handlers.rs`

**Interfaces:**
- Produces: `SessionCommand::Snapshot { reply: oneshot::Sender<SessionSnapshot> }` where `SessionSnapshot { status: SessionStatus, pending_question: Option<String>, pending_ask: Option<String>, inbox: Vec<InboxMessage> }`.

- [ ] **Step 1: Write the failing test**

`an_offloaded_session_still_reports_awaiting_input`: journal `AskRecorded`, offload the session, then `Get` and assert `status == Some(AwaitingInput)` and the pending question comes back.

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p horsie-server -- an_offloaded_session_still_reports_awaiting_input`
Expected: FAIL — status is `None`.

- [ ] **Step 3: Add `Snapshot` to the actor**

Returns the fields straight off the folded `state` the framework already recovered.

- [ ] **Step 4: Route `Get` through `ensure_loaded`**

Mirror the `History` arm: `ensure_loaded` → `tell(Snapshot)` → forward the reply. Sessions the supervisor doesn't know still return `None`.

- [ ] **Step 5: Report the recovered status on load**

In `on_recovery_complete`, after `spawn_main_agent`, when the recovered status is not `Running`, `self.report(state.status.clone()).await`. `Running` still goes through `ReconcileInterrupted`, which reports `Idle`.

- [ ] **Step 6: Delete the bypass**

Remove `fold_session_state`, its test, and the import in `handlers.rs`; `get_session` builds `SessionDetail` from the snapshot.

- [ ] **Step 7: Run and commit**

Run: `cargo test -p horsie-server` then `make fmt && make clippy`
Commit: `git commit -am "Session detail comes from the actor, not a second journal reader"`

### Task 5: SSE replay becomes an actor command

**Files:**
- Modify: `server/src/sessions/session_actor.rs` (`SessionCommand::Events`)
- Modify: `server/src/sessions/supervisor.rs` (forwarding arm)
- Modify: `server/src/http/sse.rs` (call the command instead of the journal)
- Test: `server/src/http/sse.rs` or `server/tests/`

**Interfaces:**
- Produces: `SessionCommand::Events { after_seq: u64, reply: oneshot::Sender<Vec<SeqEvent>> }` returning durable events after the cursor, each stamped with its journal sequence.
- Produces: `SessionCommand::HeadSeq { reply: oneshot::Sender<u64> }` for `live=1`.

- [ ] **Step 1: Write the failing test**

`sse_replays_durable_events_after_a_cursor`: journal two coarse events, ask `Events { after_seq: 0 }`, assert both come back with ascending ids; then `Events { after_seq: first_id }` returns only the second.

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p horsie-server -- sse_replays_durable_events_after_a_cursor`
Expected: FAIL — no such command.

- [ ] **Step 3: Implement the commands in the actor**

Move `replay_session_events` / `journal_head_seq` calls inside the actor, which owns the journal handle.

- [ ] **Step 4: Rewrite the SSE handler**

The spawned stream task holds an `ActorRef` (via the supervisor) instead of `Arc<dyn Journal>`; both the initial replay and the per-`Journaled` catch-up become `Events { after_seq: last }`.

- [ ] **Step 5: Run and commit**

Run: `cargo test -p horsie-server` then `make fmt && make clippy`
Commit: `git commit -am "SSE replay goes through the session actor"`

### Task 6: The HTTP layer cannot reach a journal

**Files:**
- Modify: `server/src/http/mod.rs` (drop `AppState.journal`)
- Modify: `server/src/sessions/events.rs` (scope the readers)
- Create: `clippy.toml`

- [ ] **Step 1: Delete the field**

Remove `pub journal: Arc<dyn Journal>` from `AppState`; keep the local in the constructor and hand it to `spawn_root` and the actor deps only.

- [ ] **Step 2: Compile and fix the fallout**

Run: `cargo check -p horsie-server`
Expected: errors only at sites that should be commands by now. Any survivor is a bypass that Tasks 4–5 missed — convert it, don't re-add the field.

- [ ] **Step 3: Scope the readers**

`replay_session_events` and `journal_head_seq` become `pub(in crate::sessions)`.

- [ ] **Step 4: Add the lint backstop**

```toml
# clippy.toml
disallowed-methods = [
    { path = "horsie_actor::Journal::replay", reason = "read a journal through its actor (see docs/superpowers/specs/2026-08-02-answerable-asks-design.md)" },
]
```

Annotate each legitimate call with `#[expect(clippy::disallowed_methods, reason = "...")]`.

- [ ] **Step 5: Run the gate and commit**

Run: `make check`
Commit: `git commit -am "The journal handle leaves the HTTP layer"`

### Task 7: Open PR 2

- [ ] **Step 1:** `make check`
- [ ] **Step 2:** `gh pr create --title "Session reads go through the actor; streams outlive offload"` — body explains the offload→reconnect→reload churn loop and the deleted bypass.
- [ ] **Step 3:** `gh pr checks --watch`

---

# PR 3 — several asks, answered atomically

Branch: `feat/multi-ask`, off PR 2's head.

### Task 8: A handoff carries every parked call

**Files:**
- Modify: `models/fluorite/agent.fl` (`HandoffOutput`, `AgentInput`), regenerate
- Modify: `agentcore/src/agent.rs` (build `calls`, drop the duplicate-call rejection)
- Modify: `workflow/src/agent_actor.rs` (`RunOutcome::Concluded`, delete `find_tool_call_id`)

**Interfaces:**
- Produces: `HandoffOutput { tool_name: String, calls: Vec<HandoffCall> }`, `HandoffCall { tool_call_id: String, data: Value }`.
- Produces: `AgentInput::ToolResults(ToolResultsInput { results: Vec<ToolResultInput> })`, whose `to_message()` is one `Role::Tool` message with one `ToolResult` part per result and id `result:{first}`.

- [ ] **Step 1: Write the failing test**

`several_asks_in_one_turn_park_together` in `agentcore/src/agent.rs`: one response calling `ask` twice plus `notes`; expect `Handoff` with `calls.len() == 2` in request order, and exactly one `ToolComplete` (for `notes`).

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test -p horsie-agentcore --lib -- several_asks_in_one_turn_park_together`
Expected: FAIL — currently rejected by the duplicate-call rule from Task 1.

- [ ] **Step 3: Make the output plural**

Collect every call whose name matches the handoff tool; validate each one's input against the schema (a schema failure still rejects the turn). Delete the duplicate-call rejection *only* for the optional handoff; a forced handoff still rejects more than one `conclude`.

- [ ] **Step 4: Follow the type through workflow**

`RunOutcome::Concluded { data, tool_call_id }` becomes `{ calls: Vec<HandoffCall> }`; `find_tool_call_id` and its event scan are deleted — the ids come from the output now.

- [ ] **Step 5: Run and commit**

Run: `cargo test -p horsie-agentcore -p horsie-workflow` then `make fmt && make clippy`
Commit: `git commit -am "A handoff carries every parked call"`

### Task 9: One resume command

**Files:**
- Modify: `workflow/src/agent_actor.rs` (`AgentCommand::Resume` replacing `Run` + `InjectToolResult`, `repair_unanswered_tool_calls_except`)
- Modify: `server/src/sessions/session_actor.rs` (`drain` sends `Resume`)

**Interfaces:**
- Produces: `AgentCommand::Resume { results: Vec<ToolResultInput>, message: Option<String> }`. `message: None` → input is `AgentInput::ToolResults(results)` (results must be non-empty). `message: Some(text)` → `results` are journaled onto the history first, then the input is the user message.
- Produces: `repair_unanswered_tool_calls_except(messages: Vec<Message>, answered: &HashSet<String>) -> Vec<Message>`.

- [ ] **Step 1: Write the failing tests**

`resume_with_several_results_answers_every_parked_call`: park an agent on two asks, `Resume { results: [both], message: None }`, assert the provider's next request carries a `tool_result` for both ids and none synthetic.
`resume_with_a_message_abandons_parked_calls`: park on two asks, `Resume { results: [error results for both], message: Some("do this instead") }`, assert both ids get an error result *and* the user message starts the turn.

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p horsie-workflow -- resume_with`
Expected: FAIL — no such command.

- [ ] **Step 3: Implement `Resume`**

Keep `reject_if_running`. Journal an `InputMessage` per message written, exactly as `InjectToolResult` does today.

- [ ] **Step 4: Take a set in the repair**

`repair_unanswered_tool_calls_except` takes `&HashSet<String>`; call sites pass the ids being answered.

- [ ] **Step 5: Run and commit**

Run: `cargo test -p horsie-workflow -p horsie-server` then `make fmt && make clippy`
Commit: `git commit -am "One resume command for answers, abandonment and plain turns"`

### Task 10: The session tracks every pending ask

**Files:**
- Modify: `server/src/sessions/spec.rs` (`SessionStatus::AwaitingInput { asks: Vec<PendingAsk> }`)
- Modify: `server/src/sessions/session_actor.rs` (`SessionState.pending_asks`, `apply_event`, `on_agent_outcome`, `drain`)
- Modify: `models/fluorite/session.fl` (status payload on `SessionDetail` + `StatusChangedEvent`), regenerate

**Interfaces:**
- Produces: `PendingAsk { tool_call_id: String, question: String }`.
- Produces: `SessionState.pending_asks: Vec<PendingAsk>`; `AskRecorded` unchanged, one journaled per ask, folded by appending.
- Produces: `SessionDomainEvent::TurnBegan { consumed, answering: Option<String>, #[serde(default)] answered: Vec<String> }` — `answering` stays readable for old journals; the fold clears all pending asks either way.

- [ ] **Step 1: Write the failing test**

`two_asks_fold_into_two_pending_entries`: fold `AskRecorded` twice, assert `pending_asks.len() == 2` and `status == AwaitingInput` carrying both; then fold `TurnBegan` and assert the list is empty.

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test -p horsie-server -- two_asks_fold_into_two_pending_entries`
Expected: FAIL — `pending_ask` is a single `Option<String>`.

- [ ] **Step 3: Make the state plural**

`AgentOutcome::Asked` carries every parked call and journals one `AskRecorded` each.

- [ ] **Step 4: Put the payload on the wire**

`status_kind` keeps its discriminant; the detail and `StatusChanged` gain `asks: Vec<PendingAskView>`. Old journals: an `AskRecorded` with `tool_call_id: None` (pre-#62) folds to a pending question with no answerable id, exactly as today.

- [ ] **Step 5: Run and commit**

Run: `cargo test -p horsie-server && make ts-types` then `make fmt && make clippy`
Commit: `git commit -am "A session tracks every pending ask"`

### Task 11: Answering is atomic

**Files:**
- Modify: `server/src/http/mod.rs` (route), `server/src/http/handlers.rs` (handler)
- Modify: `server/src/sessions/session_actor.rs` (`SessionCommand::Answer`, `drain`)
- Modify: `models/fluorite/session.fl` (`AnswerAsksRequest`), regenerate

**Interfaces:**
- Produces: `POST /api/sessions/:id/answers` with `{ answers: [{ toolCallId, text }] }` → 204, or 400 when the set doesn't cover the pending asks exactly.
- Produces: `SessionCommand::Answer { answers: Vec<AskAnswer>, reply: oneshot::Sender<Result<(), AnswerError>> }`.

- [ ] **Step 1: Write the failing tests**

`a_partial_answer_set_is_refused`: park on two asks, answer one, assert `Err(AnswerError::Incomplete)` and the status is still `AwaitingInput` with both asks.
`a_complete_answer_set_resumes_the_turn`: answer both, assert one `Resume` with two results and status `Running`.
`a_plain_message_abandons_pending_asks`: park on two asks, send a user message, assert `Resume` carries an error result per ask plus the message, and no ask stays pending.

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p horsie-server -- answer`
Expected: FAIL — no such command.

- [ ] **Step 3: Implement `Answer`**

Compare the answered ids against `state.pending_asks` as sets; on mismatch reply `Incomplete` and journal nothing. On success journal `TurnBegan { consumed: [], answering: None, answered: ids }` and send `Resume { results, message: None }`.

- [ ] **Step 4: Abandon on a plain message**

`drain` with a non-empty inbox and pending asks sends `Resume { results: <one error result per pending ask, "not answered">, message: Some(merged) }` and journals `TurnBegan { consumed, answering: None, answered: vec![] }`.

- [ ] **Step 5: Add the route and handler**

400 carries which ids were missing or unexpected.

- [ ] **Step 6: Run and commit**

Run: `cargo test -p horsie-server && make ts-types` then `make fmt && make clippy`
Commit: `git commit -am "Answering every pending ask, atomically"`

### Task 12: The web answers them together

**Files:**
- Modify: `clients/web/src/pages/SessionView.tsx`, `clients/web/src/components/AskUserCard.tsx`, `clients/web/src/hooks/useSessions.ts`, `clients/web/src/api/client.ts`
- Delete: `findPendingAsk` from `clients/web/src/lib/askUser.ts` (keep `composeAnswer`, `pickedChoices`, `askInputOf`)
- Test: `clients/web/e2e/`

**Interfaces:**
- Consumes: `detail.status.asks` / the `StatusChanged` payload.
- Produces: `useAnswerAsks(id)` posting the full answer set.

- [ ] **Step 1: Write the failing e2e**

`answers a two-ask turn together`: mock-LLM turn calling `ask_user` twice; assert two cards, that submit is disabled until both have an answer, and that one request carries both answers.
`answers an ask after a reload`: park on an ask, reload the page, assert the card is answerable without waiting for a status transition.

- [ ] **Step 2: Run and watch them fail**

Run: `cd clients/web && npx playwright test e2e/ask.spec.ts`
Expected: FAIL — one card, gated on live status.

- [ ] **Step 3: Drive pendingness off the status payload**

`AskAnswerApi` gains `pendingIds: string[]` and a per-card `setAnswer(id, text)`; `AskUserCard` reports its answer upward rather than submitting alone.

- [ ] **Step 4: Group the submit**

One submit for the pending set, disabled until every ask has a non-empty answer.

- [ ] **Step 5: Render a dead ask as superseded**

`call.isError` on an `ask_user` call renders the reason as a muted note, not as an answer.

- [ ] **Step 6: Run and commit**

Run: `cd clients/web && npm run lint && npm run build && npx playwright test`
Commit: `git commit -am "Answer every pending ask from the transcript"`

### Task 13: Open PR 3

- [ ] **Step 1:** `make check` and the web suite.
- [ ] **Step 2:** `gh pr create --title "Several asks, answered atomically"`.
- [ ] **Step 3:** `gh pr checks --watch`.

---

## Verification against the live symptom

After PR 2 deploys, the reported session (`74145a86-…`) must show its second question as answerable with the server idle. Its *first* question stays dead — it was journaled before the fix with no result — which is the correct outcome, not a regression.
