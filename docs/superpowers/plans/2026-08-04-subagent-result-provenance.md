# Subagent Result Provenance Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Carry a finished subagent's result to the client as a structured message part instead of text merged into the parent's user message, so the web UI can render it as a collapsed row in the assistant thread rather than a user bubble.

**Architecture:** A new `ContentPart::SubAgentResult` variant carries `{subagent_id, label, status, text, spawned_at_ms, ended_at_ms}`. The pure `Orchestrator` (`server/src/sessions/orchestrator.rs`) stops joining notification strings into `TurnInput.message` and puts structured parts in a new `TurnInput.subagent_results` instead. Both provider serializers flatten the part back through the existing `notification_text()`, so the bytes on the wire are unchanged. The web client turns those parts into `WorkItem`s, which means the existing `WorkGroup` collapse/expand/duration machinery renders them for free.

**Tech Stack:** Rust (fluorite codegen, tokio actors, serde), TypeScript/React 19 + Tailwind v4 (Bun, Vite, Vitest, Playwright).

**Spec:** `docs/superpowers/specs/2026-08-04-subagent-result-provenance-design.md`

## Global Constraints

- Work in the worktree `.claude/worktrees/subagent-provenance` on branch `feat/subagent-result-provenance`.
- The provider wire must stay byte-identical. Any serializer change is pinned by a test asserting equality against `notification_text()`.
- Clippy denies `wildcard_enum_match_arm`; every exhaustive `ContentPart` match gets an explicit arm. Never add `_ =>`.
- Fluorite is the source of truth for `models/src/lib.rs` generated types and for `clients/web/src/generated` + `clients/ts/src/generated`. Regenerate, never hand-edit generated files.
- `make check` is the Rust gate. `cd clients/web && bun run test` is the web unit gate. `bun run test:e2e` is the Playwright gate.
- Run `cargo fmt` before `cargo clippy` — clippy reports formatting-adjacent lint noise otherwise.
- New persisted fields on `SubAgentRecord` are `#[serde(default)]` so pre-existing journal rows load.

---

### Task 1: The wire type and its message projection

**Files:**
- Modify: `models/fluorite/agent.fl`
- Modify: `models/src/lib.rs` (the `impl agent::AgentInput` block, `to_message`)
- Test: `models/src/lib.rs` (inline `#[cfg(test)]`)

**Interfaces:**
- Produces: `horsie_models::agent::SubAgentResultPart { subagent_id: String, label: String, status: String, text: String, spawned_at_ms: u64, ended_at_ms: u64 }`; `ContentPart::SubAgentResult(SubAgentResultPart)`; `UserMessageInput.subagent_results: Vec<SubAgentResultPart>`; `AgentInput::user_message_with_results(id, text, results)`.

- [ ] **Step 1: Add the fluorite type**

In `models/fluorite/agent.fl`, above `union ContentPart`:

```
/// A finished subagent's result, delivered to the agent that spawned it.
/// Carried as its own part rather than merged into the user text so a client
/// can render it as agent activity; providers flatten it back to the same
/// text block they have always received.
struct SubAgentResultPart {
    subagent_id: String,
    label: String,
    /// "completed" | "failed" — the SubAgentView.status vocabulary.
    status: String,
    /// Output on success, error text on failure. Already capped at 50 KB.
    text: String,
    spawned_at_ms: u64,
    ended_at_ms: u64,
}
```

Add `SubAgentResult(SubAgentResultPart),` as the last variant of `union ContentPart`, and add to `struct UserMessageInput`:

```
    /// Finished subagents' results delivered with this turn.
    subagent_results: Vec<SubAgentResultPart>,
```

- [ ] **Step 2: Regenerate and confirm the Rust types exist**

Run: `make generate` (or the repo's fluorite codegen target — check `Makefile` for the target that writes `models/src/generated`).
Expected: `horsie_models::agent::SubAgentResultPart` exists and `ContentPart` has the new variant. `cargo build -p horsie-models` fails at this point only in *other* crates, which is expected until Task 2.

- [ ] **Step 3: Write the failing tests for `to_message`**

Append to the `#[cfg(test)]` module in `models/src/lib.rs`:

```rust
fn result_part(label: &str) -> agent::SubAgentResultPart {
    agent::SubAgentResultPart {
        subagent_id: "11111111-1111-1111-1111-111111111111".into(),
        label: label.into(),
        status: "completed".into(),
        text: "did the thing".into(),
        spawned_at_ms: 10,
        ended_at_ms: 50,
    }
}

#[test]
fn a_user_message_appends_subagent_results_after_its_text() {
    let input = agent::AgentInput::user_message_with_results(
        "m1",
        "keep going",
        vec![result_part("audit")],
    );
    let msg = input.to_message(0);
    assert_eq!(msg.parts.len(), 2);
    assert!(matches!(&msg.parts[0], agent::ContentPart::Text(t) if t.text == "keep going"));
    assert!(matches!(&msg.parts[1], agent::ContentPart::SubAgentResult(r) if r.label == "audit"));
}

/// An owed-only turn has no typed text. An empty text block is not just noise:
/// Anthropic rejects it outright, so the part must be omitted, not blank.
#[test]
fn an_empty_user_text_produces_no_text_part() {
    let input = agent::AgentInput::user_message_with_results("m1", "", vec![result_part("audit")]);
    let msg = input.to_message(0);
    assert_eq!(msg.parts.len(), 1);
    assert!(matches!(&msg.parts[0], agent::ContentPart::SubAgentResult(_)));
}

#[test]
fn a_plain_user_message_is_unchanged() {
    let msg = agent::AgentInput::user_message("m1", "hello").to_message(0);
    assert_eq!(msg.parts.len(), 1);
    assert!(matches!(&msg.parts[0], agent::ContentPart::Text(t) if t.text == "hello"));
}
```

- [ ] **Step 4: Run to verify they fail**

Run: `cargo test -p horsie-models`
Expected: FAIL — `user_message_with_results` not found.

- [ ] **Step 5: Implement**

In `models/src/lib.rs`, `impl agent::AgentInput`, change `user_message` to default the vec and add the new constructor:

```rust
    pub fn user_message(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self::UserMessage(agent::UserMessageInput {
            id: id.into(),
            text: text.into(),
            subagent_results: Vec::new(),
        })
    }

    /// A user turn that also delivers finished subagents' results. `text` may
    /// be empty — an owed-only turn carries results and nothing typed.
    pub fn user_message_with_results(
        id: impl Into<String>,
        text: impl Into<String>,
        subagent_results: Vec<agent::SubAgentResultPart>,
    ) -> Self {
        Self::UserMessage(agent::UserMessageInput {
            id: id.into(),
            text: text.into(),
            subagent_results,
        })
    }
```

In `to_message`, replace the `Self::UserMessage(u)` arm's fixed `parts` vec with:

```rust
            Self::UserMessage(u) => {
                // An empty text block is rejected by Anthropic, so an owed-only
                // turn carries its results and no text part at all.
                let mut parts = Vec::with_capacity(1 + u.subagent_results.len());
                if !u.text.is_empty() {
                    parts.push(agent::ContentPart::Text(agent::TextPart {
                        text: u.text.clone(),
                    }));
                }
                parts.extend(
                    u.subagent_results
                        .iter()
                        .cloned()
                        .map(agent::ContentPart::SubAgentResult),
                );
                agent::Message {
                    id: u.id.clone(),
                    role: agent::Role::User,
                    parts,
                    created_at_ms,
                    started_at_ms: None,
                }
            }
```

- [ ] **Step 6: Run to verify they pass**

Run: `cargo test -p horsie-models`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add models/ && git commit -m "models: a subagent result is its own content part"
```

---

### Task 2: Teach every ContentPart consumer the new variant

**Files:**
- Modify: `providers/anthropic/src/lib.rs` (`parts_to_api_content`, ~line 411)
- Modify: `providers/openai/src/wire.rs` (`to_wire_messages`, ~line 194)
- Modify: `supervisor/src/history.rs` (`render_message`, ~line 136)
- Modify: `agentcore/src/agent.rs` (~lines 182, 192)
- Modify: `workflow/src/agent_actor.rs` (~lines 1641, 1654, 1712, 1729)
- Modify: `server/src/sessions/subagents.rs` (make `notification_text` take the part)
- Test: `providers/anthropic/src/lib.rs`, `providers/openai/src/wire.rs` inline test modules

**Interfaces:**
- Consumes: `SubAgentResultPart` from Task 1.
- Produces: `horsie_server::sessions::subagents::notification_text(part: &SubAgentResultPart) -> String` — the single renderer both providers call.

- [ ] **Step 1: Move the renderer to a part-shaped signature**

The providers cannot depend on the server crate. Put the renderer in `models/src/lib.rs` instead, as an inherent method, and have `subagents.rs` call it:

```rust
impl agent::SubAgentResultPart {
    /// The text a provider sees. This is the exact string that was merged into
    /// the parent's user message before results became their own part — the
    /// wire must not notice this change.
    #[must_use]
    pub fn to_wire_text(&self) -> String {
        if self.text.is_empty() {
            format!("[subagent \"{}\" {}]", self.label, self.status)
        } else {
            format!("[subagent \"{}\" {}]\n\n{}", self.label, self.status, self.text)
        }
    }
}
```

- [ ] **Step 2: Write the failing wire tests**

In `providers/openai/src/wire.rs` tests:

```rust
#[test]
fn a_subagent_result_reaches_the_wire_as_its_notification_text() {
    let part = horsie_models::agent::SubAgentResultPart {
        subagent_id: "id".into(),
        label: "audit".into(),
        status: "completed".into(),
        text: "three stale crates".into(),
        spawned_at_ms: 0,
        ended_at_ms: 1,
    };
    let msgs = to_wire_messages(&[Message {
        id: "u".into(),
        role: Role::User,
        parts: vec![ContentPart::SubAgentResult(part.clone())],
        created_at_ms: 0,
        started_at_ms: None,
    }]);
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].content.as_deref(), Some(part.to_wire_text().as_str()));
    assert_eq!(part.to_wire_text(), "[subagent \"audit\" completed]\n\nthree stale crates");
}
```

In `providers/anthropic/src/lib.rs` tests, the equivalent through `parts_to_api_content`, asserting the produced `MessageContent::Text`'s `text` equals `part.to_wire_text()`.

- [ ] **Step 3: Run to verify they fail**

Run: `cargo test -p horsie-provider-openai -p horsie-provider-anthropic`
Expected: FAIL — non-exhaustive match / missing variant.

- [ ] **Step 4: Add the arms**

- `providers/anthropic/src/lib.rs`: `ContentPart::SubAgentResult(r) => MessageContent::Text(Text { text: r.to_wire_text(), ..Default::default() }),`
- `providers/openai/src/wire.rs`: `ContentPart::SubAgentResult(r) => text.push_str(&r.to_wire_text()),`
- `supervisor/src/history.rs`: `ContentPart::SubAgentResult(r) => { out.push_str(&format!("· subagent {} [{}]\n", r.label, r.status)); }`
- `agentcore/src/agent.rs` both sites: add `| ContentPart::SubAgentResult(_)` to the existing `None`-returning group.
- `workflow/src/agent_actor.rs` all four sites: same, add `| ContentPart::SubAgentResult(_)` to the `None`-returning group.

- [ ] **Step 5: Point `subagents.rs` at the shared renderer**

Replace the body of `notification_text` so it builds a `SubAgentResultPart` and delegates, or delete it once Task 3 makes the tree return parts directly. Keep its existing unit tests passing by asserting on `to_wire_text()`.

- [ ] **Step 6: Run the workspace build**

Run: `cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p horsie-provider-openai -p horsie-provider-anthropic -p horsie-models`
Expected: PASS, no non-exhaustive-match errors anywhere.

- [ ] **Step 7: Commit**

```bash
git add -A && git commit -m "providers: flatten a subagent result to its notification text"
```

---

### Task 3: Timestamps on the tree, parts out of it

**Files:**
- Modify: `server/src/sessions/subagents.rs`
- Modify: `models/fluorite/session.fl` (`SubAgentView`)
- Modify: `server/src/sessions/session_actor.rs` (the `SubAgentSpawned`/`Completed`/`Failed` fold arms)
- Modify: `server/src/http/handlers.rs` (wherever `SubAgentView` is built)

**Interfaces:**
- Consumes: `SubAgentResultPart`.
- Produces: `SubAgentTree::owed_for(parent) -> Vec<(Uuid, SubAgentResultPart)>`; `owed_by_sub_parent() -> BTreeMap<Uuid, Vec<(Uuid, SubAgentResultPart)>>`; `SubAgentRecord.spawned_at_ms: u64`, `.ended_at_ms: u64`; `apply_spawned(..., at_ms)`, `apply_completed(id, output, at_ms)`, `apply_failed(id, error, at_ms)`.

- [ ] **Step 1: Write the failing fold tests**

In `server/src/sessions/subagents.rs` tests:

```rust
#[test]
fn a_terminal_node_records_when_it_started_and_finished() {
    let id = Uuid::new_v4();
    let mut tree = SubAgentTree::default();
    tree.apply_spawned(id, SubAgentParent::Main, "audit".into(), "task".into(), 1, 100);
    tree.apply_completed(id, "done".into(), 400);
    let rec = tree.get(&id).unwrap();
    assert_eq!(rec.spawned_at_ms, 100);
    assert_eq!(rec.ended_at_ms, 400);
}

#[test]
fn owed_results_carry_the_label_status_and_span() {
    let id = Uuid::new_v4();
    let mut tree = SubAgentTree::default();
    tree.apply_spawned(id, SubAgentParent::Main, "audit".into(), "task".into(), 1, 100);
    tree.apply_completed(id, "three stale crates".into(), 400);
    let owed = tree.owed_for(SubAgentParent::Main);
    assert_eq!(owed.len(), 1);
    let part = &owed[0].1;
    assert_eq!(part.label, "audit");
    assert_eq!(part.status, "completed");
    assert_eq!(part.text, "three stale crates");
    assert_eq!((part.spawned_at_ms, part.ended_at_ms), (100, 400));
    assert_eq!(part.subagent_id, id.to_string());
}

#[test]
fn a_failed_node_owes_its_error_as_the_result_text() {
    let id = Uuid::new_v4();
    let mut tree = SubAgentTree::default();
    tree.apply_spawned(id, SubAgentParent::Main, "audit".into(), "task".into(), 1, 100);
    tree.apply_failed(id, "provider 500".into(), 200);
    let owed = tree.owed_for(SubAgentParent::Main);
    assert_eq!(owed[0].1.status, "failed");
    assert_eq!(owed[0].1.text, "provider 500");
}

/// Rows journaled before timestamps existed must still load.
#[test]
fn a_record_without_timestamps_deserializes() {
    let json = r#"{"parent":"Main","label":"a","task":"t","depth":1,
        "status":"Completed","output":"o","error":null,"notified":false}"#;
    let rec: SubAgentRecord = serde_json::from_str(json).unwrap();
    assert_eq!((rec.spawned_at_ms, rec.ended_at_ms), (0, 0));
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p horsie-server subagents::`
Expected: FAIL — arity mismatch on `apply_spawned`, no `spawned_at_ms`.

- [ ] **Step 3: Implement**

Add to `SubAgentRecord`:

```rust
    /// When the node was spawned / reached its current terminal state. Zero on
    /// rows journaled before these fields existed; the client shows no duration
    /// rather than a wrong one.
    #[serde(default)]
    pub spawned_at_ms: u64,
    #[serde(default)]
    pub ended_at_ms: u64,
```

Thread `at_ms` through `apply_spawned`/`apply_completed`/`apply_failed` (the callers in `session_actor.rs` already have the event's `at_ms` in scope). Replace the `notification_text(...)` call in `owed_for`/`owed_by_sub_parent` with a private helper:

```rust
    fn owed_part(id: &Uuid, rec: &SubAgentRecord) -> SubAgentResultPart {
        let (status, text) = match rec.status {
            SubAgentStatus::Failed => ("failed", rec.error.clone().unwrap_or_default()),
            _ => ("completed", rec.output.clone().unwrap_or_default()),
        };
        SubAgentResultPart {
            subagent_id: id.to_string(),
            label: rec.label.clone(),
            status: status.to_string(),
            text: truncate_result(&text),
            spawned_at_ms: rec.spawned_at_ms,
            ended_at_ms: rec.ended_at_ms,
        }
    }
```

Add `spawned_at_ms: u64` and `ended_at_ms: u64` to `SubAgentView` in `models/fluorite/session.fl`, regenerate, and populate them where the view is built.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p horsie-server subagents::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "sessions: subagent results carry label, status and span"
```

---

### Task 4: The orchestrator stops merging

**Files:**
- Modify: `server/src/sessions/orchestrator.rs` (`TurnInput`, `main_turn`, `wake_owed_parents`)
- Modify: `server/src/sessions/session_actor.rs` (performing `AgentAction::StartTurn`)
- Modify: `workflow/src/agent_actor.rs` (`AgentCommand::Resume`)

**Interfaces:**
- Consumes: `owed_for`/`owed_by_sub_parent` returning parts (Task 3); `AgentInput::user_message_with_results` (Task 1).
- Produces: `TurnInput { message: Option<String>, results: Vec<ToolResultInput>, subagent_results: Vec<SubAgentResultPart> }`; `AgentCommand::Resume { results, message, subagent_results }`.

- [ ] **Step 1: Write the failing orchestrator tests**

In `server/src/sessions/orchestrator.rs` tests (build a state with an owed node using the `SubAgentTree` helpers from Task 3):

```rust
/// The typed text and the results are separate now: the message is the inbox
/// alone, so a client can tell what the person said from what a subagent did.
#[test]
fn owed_results_ride_a_turn_without_joining_its_text() {
    let s = with_inbox_and_owed(&["check the lockfile too"], "audit", "three stale crates");
    let AgentAction::StartTurn { input, notified, .. } = only_turn(&s);
    assert_eq!(input.message.as_deref(), Some("check the lockfile too"));
    assert_eq!(input.subagent_results.len(), 1);
    assert_eq!(input.subagent_results[0].label, "audit");
    assert_eq!(notified.len(), 1);
}

#[test]
fn an_owed_only_turn_has_no_message() {
    let s = with_inbox_and_owed(&[], "audit", "three stale crates");
    let AgentAction::StartTurn { input, .. } = only_turn(&s);
    assert_eq!(input.message, None);
    assert_eq!(input.subagent_results.len(), 1);
}

#[test]
fn a_woken_subagent_parent_is_resumed_with_results_and_no_message() {
    let s = with_owed_to_sub_parent("audit", "three stale crates");
    let AgentAction::StartTurn { who, input, .. } = only_turn(&s);
    assert!(matches!(who, AgentKey::Sub(_)));
    assert_eq!(input.message, None);
    assert_eq!(input.subagent_results.len(), 1);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p horsie-server orchestrator::`
Expected: FAIL — no field `subagent_results` on `TurnInput`.

- [ ] **Step 3: Implement**

Add `pub subagent_results: Vec<SubAgentResultPart>` to `TurnInput`. In `main_turn`, replace the `parts` join with:

```rust
    let message = (!state.inbox.is_empty()).then(|| {
        state
            .inbox
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>()
            .join(MERGE_SEPARATOR)
    });
```

and set `subagent_results: owed.iter().map(|(_, part)| part.clone()).collect()`. In `wake_owed_parents`, set `message: None` and fill `subagent_results` the same way. Update the existing test `several_queued_messages_merge_into_one_turn` — inbox merging is unchanged, so it still passes.

In `session_actor.rs`, where `AgentAction::StartTurn` is performed, pass the results through to `AgentCommand::Resume`. In `workflow/src/agent_actor.rs`, add `subagent_results: Vec<SubAgentResultPart>` to `AgentCommand::Resume`, extend the "nothing to do" guard to `results.is_empty() && message.is_none() && subagent_results.is_empty()`, and build the input with:

```rust
                let agent_input = match (message, subagent_results.is_empty()) {
                    (None, true) => AgentInput::tool_results(results),
                    (message, _) => {
                        if !results.is_empty() {
                            let recorded = AgentInput::tool_results(results).to_message(now_ms());
                            events.push(AgentDomainEvent::InputMessage { message: recorded.clone() });
                            history.push(recorded);
                        }
                        AgentInput::user_message_with_results(
                            new_message_id(),
                            message.unwrap_or_default(),
                            subagent_results,
                        )
                    }
                };
```

Fix every other `AgentCommand::Resume` construction (search the workspace) to pass `subagent_results: Vec::new()`.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p horsie-server orchestrator:: && cargo test -p horsie-workflow`
Expected: PASS.

- [ ] **Step 5: Assert it end-to-end in the actor tests**

In the existing subagent actor tests in `session_actor.rs`, add an assertion that the parent's recorded input message has a `SubAgentResult` part (and, for a mixed turn, a `Text` part first). Run `cargo test -p horsie-server`.

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "orchestrator: deliver subagent results as parts, not merged text"
```

---

### Task 5: The client reads the new part

**Files:**
- Modify: `clients/web/src/hooks/useSessionStream.ts`
- Test: covered by Task 6's tests (this task has no behaviour of its own to assert)

**Interfaces:**
- Produces: `RenderedSubAgent { subagentId: string; label: string; status: string; text: string; spawnedAtMs: number; endedAtMs: number }` exported from `useSessionStream.ts`; `RenderedMessage.subagentResults: RenderedSubAgent[]`.

- [ ] **Step 1: Regenerate the TS types**

Run: `cd clients/web && bun run generate-types`
Expected: `src/generated/agent/subAgentResultPart.ts` exists and `ContentPart` includes it.

- [ ] **Step 2: Add the extractor and thread it through**

Add beside `textOf`/`thinkingOf`/`toolCallsOf`:

```ts
export interface RenderedSubAgent {
  subagentId: string;
  label: string;
  status: string;
  text: string;
  /** Zero for subagents journaled before spans were recorded — the row then
   * shows no duration rather than an invented one. */
  spawnedAtMs: number;
  endedAtMs: number;
}

function subAgentResultsOf(parts: ContentPart[]): RenderedSubAgent[] {
  return parts
    .filter((p) => p.type === "SubAgentResult")
    .map((p) => ({
      subagentId: p.value.subagentId,
      label: p.value.label,
      status: p.value.status,
      text: p.value.text,
      spawnedAtMs: p.value.spawnedAtMs,
      endedAtMs: p.value.endedAtMs,
    }));
}
```

Add `subagentResults: RenderedSubAgent[]` to both `RenderedMessage` and the reducer's `StoredMessage`, populate it where `textOf(msg.parts)` is called, and default it to `[]` in every other `RenderedMessage` construction (the optimistic and queued branches near the end of the hook).

- [ ] **Step 3: Typecheck**

Run: `cd clients/web && bun run build`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "web: read subagent result parts off the stream"
```

---

### Task 6: Grouping and segments

**Files:**
- Modify: `clients/web/src/components/Transcript.tsx` (`groupTurns`, `UserTurn` call site)
- Modify: `clients/web/src/lib/transcriptSegments.ts`
- Modify: `clients/web/src/components/WorkGroup.tsx` (`summary`, `getItemKey`, `renderItem`)
- Test: `clients/web/src/lib/transcriptSegments.test.ts`

**Interfaces:**
- Consumes: `RenderedSubAgent`, `RenderedMessage.subagentResults`.
- Produces: `WorkItem` variant `{ kind: "subagent"; result: RenderedSubAgent }`; exported `groupTurns` (export it so it can be unit-tested).

- [ ] **Step 1: Write the failing tests**

In `clients/web/src/lib/transcriptSegments.test.ts`:

```ts
/** An owed-only turn is agent activity, not something the person said — it
 *  must not produce a user bubble. */
it("attaches an owed-only result to the preceding assistant entry", () => {
  const turns = groupTurns([
    assistantMsg("a1", { text: "delegating" }),
    userMsg("u1", { text: "", subagentResults: [sub("audit")] }),
  ]);
  expect(turns).toHaveLength(1);
  expect(turns[0].kind).toBe("assistant");
});

it("puts results above the bubble when a turn carries both", () => {
  const turns = groupTurns([
    assistantMsg("a1", { text: "delegating" }),
    userMsg("u1", { text: "check the lockfile too", subagentResults: [sub("audit")] }),
  ]);
  expect(turns.map((t) => t.kind)).toEqual(["assistant", "user"]);
  const assistant = turns[0] as Extract<TurnGroup, { kind: "assistant" }>;
  expect(assistant.msgs.at(-1)?.subagentResults).toHaveLength(1);
  expect(assistant.msgs.at(-1)?.text).toBe("");
});

it("opens an assistant entry when a result has nothing to attach to", () => {
  const turns = groupTurns([userMsg("u1", { text: "", subagentResults: [sub("audit")] })]);
  expect(turns).toHaveLength(1);
  expect(turns[0].kind).toBe("assistant");
});

it("renders a subagent result as a work item", () => {
  const segments = buildSegments([userMsg("u1", { text: "", subagentResults: [sub("audit")] })]);
  expect(segments).toHaveLength(1);
  const work = segments[0] as Extract<Segment, { kind: "work" }>;
  expect(work.items[0]).toEqual({ kind: "subagent", result: sub("audit") });
  expect(work.startedAtMs).toBe(100);
  expect(work.endedAtMs).toBe(400);
});
```

with local helpers `sub(label)` returning a `RenderedSubAgent` with `spawnedAtMs: 100, endedAtMs: 400`, and `assistantMsg`/`userMsg` builders matching the existing file's style.

- [ ] **Step 2: Run to verify they fail**

Run: `cd clients/web && bun run test transcriptSegments`
Expected: FAIL — `groupTurns` is not exported.

- [ ] **Step 3: Implement**

Export `groupTurns` and `TurnGroup` from `Transcript.tsx` and rewrite the loop:

```tsx
function groupTurns(messages: RenderedMessage[]): TurnGroup[] {
  const turns: TurnGroup[] = [];
  const intoAssistant = (m: RenderedMessage) => {
    const last = turns[turns.length - 1];
    if (last?.kind === "assistant") last.msgs.push(m);
    else turns.push({ kind: "assistant", id: m.id, msgs: [m] });
  };
  for (const m of messages) {
    if (m.role === "User") {
      // A subagent's result is the agent's own work landing, not something the
      // person said — it belongs to the assistant thread whatever the wire
      // (which must keep them in one user message) says.
      if (m.subagentResults.length > 0) {
        intoAssistant({ ...m, id: `${m.id}:sub`, text: "", thinking: [], toolCalls: [] });
      }
      if (m.text) turns.push({ kind: "user", msg: m });
      continue;
    }
    intoAssistant(m);
  }
  return turns;
}
```

In `transcriptSegments.ts`, add the `WorkItem` variant and, at the top of the per-message loop in `buildSegments`:

```ts
    for (const r of m.subagentResults) {
      work.push({ kind: "subagent", result: r });
      if (r.spawnedAtMs > 0 && r.endedAtMs > 0) extend(r.spawnedAtMs, r.endedAtMs);
    }
```

In `WorkGroup.tsx`, handle the variant in `getItemKey` (`` `subagent-${item.result.subagentId}` ``) and `renderItem` (`<SubAgentCard …>`, Task 7), and extend `summary` so a group counts subagents:

```ts
function summary(items: WorkItem[]): string {
  const thinking = items.filter((i) => i.kind === "thinking").length;
  const tools = items.filter((i) => i.kind === "tool").length;
  const subs = items.filter((i) => i.kind === "subagent").length;
  const clauses: string[] = [];
  if (tools > 0) clauses.push(`ran ${tools} tool${tools === 1 ? "" : "s"}`);
  if (subs > 0) clauses.push(`${subs} subagent${subs === 1 ? "" : "s"} finished`);
  if (clauses.length === 0) return "Thought for a moment";
  const body = clauses.join(" and ");
  const lead = thinking > 0 ? "Thought and " : "";
  return `${lead}${body}`.replace(/^./, (c) => c.toUpperCase());
}
```

Also make `WorkGroup`'s `visibleWithIndices` filter treat `subagent` items as always visible (only `thinking` is gated by `showThinking`).

- [ ] **Step 4: Run to verify they pass**

Run: `cd clients/web && bun run test transcriptSegments`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "web: subagent results render as agent work, not user turns"
```

---

### Task 7: The row

**Files:**
- Create: `clients/web/src/components/SubAgentCard.tsx`
- Modify: `clients/web/src/components/WorkGroup.tsx` (import)

**Interfaces:**
- Consumes: `RenderedSubAgent`, `formatDuration` from `../lib/time`.
- Produces: `SubAgentCard({ result }: { result: RenderedSubAgent })`.

- [ ] **Step 1: Write the component**

Model it on `ToolCallCard.tsx` — read that file first and match its chrome, `data-testid` conventions, and disclosure pattern. Required behaviour:

- Collapsed row: `Subagent "<label>" <status>` plus `· <duration>` only when both stamps are non-zero.
- `data-testid="subagent-card"`, `data-status={result.status}` so e2e can select it.
- Expanding reveals `result.text` in the same pre-wrap treatment `ToolCallCard` uses for tool output.
- `status === "failed"` gets the error styling used elsewhere in the app (check `ToolCallCard`'s `isError` branch and reuse those tokens). Any other status renders neutral — an unknown status must not borrow success or failure styling.
- Empty `result.text` renders the row with no disclosure control rather than an empty expandable.

- [ ] **Step 2: Wire it into `WorkGroup.renderItem`**

- [ ] **Step 3: Typecheck and run the unit suite**

Run: `cd clients/web && bun run build && bun run test`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "web: a collapsed row for a finished subagent"
```

---

### Task 8: End-to-end

**Files:**
- Create: `clients/web/e2e/s-subagent-results.spec.ts`
- Read first: `clients/web/e2e/b-tool-call.spec.ts`, `clients/web/e2e/harness.ts`, `clients/web/e2e/helpers.ts`

**Interfaces:**
- Consumes: the mock-LLM scripting helpers in `harness.ts`.

- [ ] **Step 1: Read the harness**

`spawn_agent` is a server-owned tool, so no new plumbing is needed: script the mock LLM to emit a `spawn_agent` tool call on the first turn, a final text on the subagent's turn, and a final text on the parent's woken turn.

- [ ] **Step 2: Write the spec**

Three assertions:
1. After the subagent finishes, a `[data-testid="subagent-card"]` appears with the label and `data-status="completed"`.
2. No new `[data-role="User"]` turn appears between the user's own message and the agent's final answer — the owed-only turn produced no bubble. Count user turns before and after.
3. Clicking the row reveals the subagent's result text.

- [ ] **Step 3: Run it**

Run: `cd clients/web && bun run test:e2e s-subagent-results`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "web: e2e cover the subagent result row"
```

---

### Task 9: Green and up

- [ ] **Step 1: Full Rust gate**

Run: `cargo fmt && make check`
Expected: PASS.

- [ ] **Step 2: Full web gate**

Run: `cd clients/web && bun run build && bun run test && bun run test:e2e`
Expected: PASS.

- [ ] **Step 3: Confirm the generated-type drift job would pass**

Run: `bun run generate-types && git diff --exit-code clients/web/src/generated clients/ts/src/generated`
Expected: no diff (CI has a drift job that fails otherwise).

- [ ] **Step 4: Push and open the PR**

Body: what changed and why, the byte-identical-wire guarantee, and an explicit note that journals become forward-only at this commit (rolling back past it bricks subagent sessions — accepted deliberately, mitigations were considered and declined).

---

## Self-Review

**Spec coverage:** wire type → Task 1; provider flattening → Task 2; durations → Task 3; orchestrator de-merge → Task 4; client extraction → Task 5; assistant-side grouping + WorkItem → Task 6; the row and failure styling → Task 7; e2e → Task 8; compatibility note → Task 9 PR body. The spec's "old rows render as today" needs no task: untouched journals contain only `Text` parts and take the existing path.

**Type consistency:** `SubAgentResultPart` field names (`subagent_id`, `label`, `status`, `text`, `spawned_at_ms`, `ended_at_ms`) are used identically in Tasks 1–4; their camelCase twins (`subagentId`, `spawnedAtMs`, `endedAtMs`) in Tasks 5–7. `to_wire_text()` is defined in Task 2 and used only there. `user_message_with_results` is defined in Task 1 and consumed in Task 4. `groupTurns`/`TurnGroup` are exported in Task 6 and consumed by its own tests.

**Known risk:** Task 2 Step 1 moves the renderer into `models` because the provider crates cannot depend on the server crate. If `notification_text`'s existing callers in `subagents.rs` prove hard to retire cleanly, keep the function as a thin wrapper — do not duplicate the format string in two places, which would let the wire drift silently.
