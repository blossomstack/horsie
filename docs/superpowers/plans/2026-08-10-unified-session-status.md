# Unified Session Status Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One durable status vocabulary for every session, workflow run or not, so a list can say what became of a run without loading it.

**Architecture:** The session actor stops hand-writing a status report at each transition and instead reports the status its journal just folded, from one call site. The supervisor journals that report into `SessionRecord.status` instead of caching it in memory, so it survives unload and restart. A new `Finished` status covers "a run completed with no error", which is the only thing `SessionStatus` could not already say about a run. `WorkflowStatus` then leaves the wire entirely, and the workflow run list becomes `GET /api/sessions?workflow=<name>`.

**Tech Stack:** Rust (axum, tokio, `horsie-actor` event-sourced actors), fluorite schema codegen for the TypeScript wire types, React + TanStack Query + Playwright for the web client.

## Global Constraints

- **No backward compatibility.** Break wire shapes and persisted shapes freely; go to the right end state. Do not add `#[serde(default)]` compatibility shims for fields introduced by this plan except where a field is genuinely optional in the model.
- **Any `.fl` edit requires `make types`**, and the generated tree is committed. Drift-check with `git status`, not `git diff` — generation never deletes orphans.
- **Rust iteration is `cargo test -p horsie-server --lib <filter>`.** Run the full `make check` once before pushing, never twice in one command.
- **The web client installs with bun**, never npm: `cd clients/web && bun install`.
- **Playwright on macOS needs `TMPDIR=/tmp`**, or global setup dies on a `sun_path` overflow.
- **Never list Claude as an author or co-author** on any commit or PR.
- A regression test must be seen to fail against the unfixed code before the fix lands.

---

### Task 1: One status report call site

Today the supervisor learns a session's status from 13 hand-written `report(LITERAL)` calls, each of which duplicates the status the event on the very next line is about to fold. This task deletes all of them and reports `state.status` from the two places where the state has settled — exactly the construction `report_run_status` uses. Nothing on the wire changes; this is the prerequisite that makes Task 2 safe to persist.

**Files:**
- Modify: `crates/server/src/sessions/session_actor/mod.rs` — `report`, `on_events_persisted`, `on_recovery_complete`
- Modify: `crates/server/src/sessions/session_actor/turns.rs` — remove 6 `self.report` calls
- Modify: `crates/server/src/sessions/session_actor/run.rs` — remove 6 `self.report` calls
- Test: `crates/server/src/sessions/session_actor/testing.rs` (existing helpers), new tests in `crates/server/src/sessions/supervisor.rs` tests module

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `SessionActor::report_status(&self, state: &SessionState)` — reports `state.status` to the supervisor; called from `on_events_persisted` and `on_recovery_complete`. Replaces `SessionActor::report(&self, status: SessionStatus)`, which is deleted.

- [ ] **Step 1: Write the failing test — a subagent-only repair still reports a status**

This is the hole the current code has: `on_recovery_complete` skips its report whenever any repair is queued (`mod.rs:861`), and `SubAgents::on_load` → `Reconcile` is a repair that never reports. Add to the tests module in `crates/server/src/sessions/supervisor.rs`:

```rust
/// A session whose only repair is an interrupted subagent still tells the
/// supervisor what it recovered as.
///
/// `on_recovery_complete` skips its own report whenever any repair is queued,
/// on the grounds that the repair reports the status it lands on — but
/// `SubAgentCommand::Reconcile` only persists `SubAgentFailed` and never
/// reports. So this session used to load and say nothing at all.
#[tokio::test]
async fn a_subagent_only_repair_still_reports_a_status() {
    let f = fixture().await;
    let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
    let sup = spawn_supervisor_on(&f, journal.clone()).await;
    let id = create(&sup).await;
    assert!(await_signal(&f.agent, &format!("create:{id}")).await);

    // Leave exactly one interrupted subagent behind and nothing else to repair.
    let child = loaded_child(&sup, &id).await;
    child
        .ask(|reply| {
            SessionCommand::SubAgent(SubAgentCommand::Spawn {
                label: "worker".into(),
                reply,
            })
        })
        .await
        .unwrap();
    sup.ask(|reply| SessionSupervisorCommand::Shutdown { reply })
        .await
        .unwrap();

    let sup2 = spawn_supervisor_on(&f, journal).await;
    // Loading is what triggers the repair.
    let _ = sup2
        .ask(|reply| SessionSupervisorCommand::Get {
            id: id.clone(),
            reply,
        })
        .await;
    let rows = sup2
        .ask(|reply| SessionSupervisorCommand::List { reply })
        .await
        .unwrap();
    let (_, _, status) = rows
        .into_iter()
        .find(|(row_id, _, _)| row_id == &id)
        .expect("the session still exists");
    assert!(
        status.is_some(),
        "a loaded session must have reported a status, repairs or not"
    );
}
```

If `loaded_child` and `spawn_supervisor_on` do not exist in the tests module, add them:

```rust
async fn spawn_supervisor_on(
    f: &Fixture,
    journal: Arc<dyn Journal>,
) -> ActorRef<SessionSupervisorCommand> {
    let clock: Arc<TestClock> = Arc::new(TestClock::new());
    let (gtx, _) = broadcast::channel(16);
    ActorSystem::new(journal).spawn_persistent(SessionSupervisor::with_config(
        crate::auth::UserId::bootstrap(),
        f.deps.clone(),
        gtx,
        manual_config(&clock),
    ))
}
```

- [ ] **Step 2: Run it and watch it fail**

```bash
cargo test -p horsie-server --lib sessions::supervisor::tests::a_subagent_only_repair_still_reports_a_status
```

Expected: FAIL on `a loaded session must have reported a status, repairs or not`. If it passes, the hole is not where this plan says it is — stop and re-read `on_recovery_complete` before changing anything.

- [ ] **Step 3: Replace `report` with `report_status`**

In `crates/server/src/sessions/session_actor/mod.rs`, replace the `report` method:

```rust
    /// Tell the supervisor the status this session's journal just folded.
    ///
    /// Read off the folded state rather than announced at each transition, and
    /// called only where the state has settled — after a persisted batch, and
    /// once at load. So what the supervisor records is by construction what the
    /// session journaled, and the two cannot drift apart by a missed call site.
    async fn report_status(&self, state: &SessionState) {
        let _ = self
            .parent
            .tell(SessionSupervisorCommand::SessionStatusChanged {
                id: self.id.to_string(),
                status: state.status.clone(),
            })
            .await;
    }
```

- [ ] **Step 4: Call it from the two settled points**

In `on_events_persisted`:

```rust
    async fn on_events_persisted(&mut self, events: &[SessionDomainEvent], state: &SessionState) {
        self.record_lifecycle(events, state).await;
        self.report_status(state).await;
    }
```

In `on_recovery_complete`, delete the `repairing` variable and the conditional report at the end, and report unconditionally:

```rust
        for cmd in repairs {
            let _ = ctx.self_ref().tell(cmd).await;
        }
        // Unconditional, repairs or not. A repair need not persist anything —
        // an interrupted subagent's reconcile is the case that used to leave a
        // loaded session with no status at all — and one that does persist
        // reports again from `on_events_persisted` with the state it landed on.
        self.report_status(state).await;
```

- [ ] **Step 5: Delete the 12 hand-written reports**

In `crates/server/src/sessions/session_actor/turns.rs` delete the `self.report(...).await;` lines at (pre-edit) lines 145, 149, 161, 174, 187, 198 — every one of them is followed by the event whose fold sets the same status (`turns.rs:338-362`). Note line 161's report sits inside a match arm guarded by `state.status == SessionStatus::Running`; keep the guard, delete only the report.

In `crates/server/src/sessions/session_actor/run.rs` delete the `self.report(...).await;` lines at (pre-edit) lines 111, 126, 135, 238, 252, 267 — same reasoning, folds at `run.rs:405-449`.

In `crates/server/src/sessions/session_actor/mod.rs` delete the `self.report(SessionStatus::Running).await;` at (pre-edit) line 708.

- [ ] **Step 6: Fix the compile**

```bash
cargo build -p horsie-server --lib
```

Expected: unused-import errors for `SessionStatus` in `turns.rs` / `run.rs` if nothing else there uses it. Remove only the imports the compiler names; `turns.rs` still uses `SessionStatus::Running` in its guard and in `busy`.

- [ ] **Step 7: Run the session and supervisor suites**

```bash
cargo test -p horsie-server --lib sessions::
```

Expected: PASS, including the new test from Step 1.

A failure to expect here: any test asserting a status arrives *before* its event is persisted will now see it after. That ordering change is the point — fix the test to await the status rather than assume it precedes the write.

- [ ] **Step 8: Commit**

```bash
git add -A crates/server/src/sessions
git commit -m "refactor(sessions): report the status the journal folded, from one place"
```

---

### Task 2: Persist the status in the supervisor

**Files:**
- Modify: `crates/server/src/sessions/supervisor.rs` — module doc, `SessionRecord`, `SessionSupervisorEvent`, `apply_event`, `handle_command`, `List`, `forget`, `offload_idle`, `ensure_loaded`
- Modify: `crates/server/src/http/handlers.rs` — `summary`, `create_session`
- Modify: `crates/server/src/http/agents.rs`, `crates/server/src/http/workflows.rs`, `crates/server/src/http/routines.rs` — call sites
- Test: `crates/server/src/sessions/supervisor.rs` tests module

**Interfaces:**
- Consumes: `SessionActor::report_status` from Task 1.
- Produces:
  - `SessionRecord.status: SessionStatus` — durable, non-optional, defaults to `SessionStatus::Provisioning` for a record created by `SessionCreated`.
  - `SessionSupervisorCommand::List` reply becomes `Vec<(SessionId, SessionRecord)>` — the third tuple element is gone.
  - `handlers::summary(id: &str, rec: &SessionRecord) -> SessionSummary` — two arguments, not three.

- [ ] **Step 1: Write the failing test — a status outlives the process**

In the `crates/server/src/sessions/supervisor.rs` tests module:

```rust
/// A session's status outlives the process that produced it.
///
/// It used to be a cache of loaded sessions, so every row rendered unknown
/// after a restart — and a workflow's list of past runs is a list of sessions
/// that are by definition cold, so every one of them was a dash.
#[tokio::test]
async fn a_status_survives_a_restart_without_loading_the_session() {
    let f = fixture().await;
    let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
    let sup = spawn_supervisor_on(&f, journal.clone()).await;
    let id = create(&sup).await;
    assert!(await_signal(&f.agent, &format!("create:{id}")).await);
    let _ = sup
        .tell(SessionSupervisorCommand::SessionStatusChanged {
            id: id.clone(),
            status: SessionStatus::Failed {
                reason: "the provider said no".into(),
            },
        })
        .await;
    sup.ask(|reply| SessionSupervisorCommand::Shutdown { reply })
        .await
        .unwrap();
    let before = f.agent.signals();

    let sup2 = spawn_supervisor_on(&f, journal).await;
    let rows = sup2
        .ask(|reply| SessionSupervisorCommand::List { reply })
        .await
        .unwrap();
    let (_, rec) = rows
        .into_iter()
        .find(|(row_id, _)| row_id == &id)
        .expect("the session still exists");
    assert_eq!(
        rec.status,
        SessionStatus::Failed {
            reason: "the provider said no".into()
        },
        "the last status the session reported is the one the registry keeps"
    );
    assert_eq!(
        f.agent.signals(),
        before,
        "listing must not wake a session, which would re-attempt its provision"
    );
}

/// Re-reporting an unchanged status journals nothing: every load reports what
/// it recovered, and a busy session reports after every persisted batch.
#[tokio::test]
async fn re_reporting_the_same_status_journals_nothing() {
    let f = fixture().await;
    let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
    let sup = spawn_supervisor_on(&f, journal.clone()).await;
    let id = create(&sup).await;
    assert!(await_signal(&f.agent, &format!("create:{id}")).await);
    let _ = sup
        .tell(SessionSupervisorCommand::SessionStatusChanged {
            id: id.clone(),
            status: SessionStatus::Idle,
        })
        .await;
    let _ = sup
        .ask(|reply| SessionSupervisorCommand::List { reply })
        .await
        .unwrap();
    let pid = PersistenceId::new(
        "session-supervisor",
        crate::auth::UserId::bootstrap().as_str(),
    );
    let before = journal_len(&journal, &pid).await;

    for _ in 0..3 {
        let _ = sup
            .tell(SessionSupervisorCommand::SessionStatusChanged {
                id: id.clone(),
                status: SessionStatus::Idle,
            })
            .await;
    }
    let _ = sup
        .ask(|reply| SessionSupervisorCommand::List { reply })
        .await
        .unwrap();
    assert_eq!(
        journal_len(&journal, &pid).await,
        before,
        "a status that has not changed is not news"
    );
}

async fn journal_len(journal: &Arc<dyn Journal>, pid: &PersistenceId) -> usize {
    use futures_util::StreamExt;
    #[expect(
        clippy::disallowed_methods,
        reason = "test-only inspection of a journal whose actor is running"
    )]
    let stream = journal.replay(pid, 0).await;
    stream.count().await
}
```

- [ ] **Step 2: Run them and watch them fail**

```bash
cargo test -p horsie-server --lib sessions::supervisor::tests::a_status_survives sessions::supervisor::tests::re_reporting
```

Expected: FAIL to compile — `SessionRecord` has no field `status`, and the `List` reply is a 3-tuple. That is the correct first failure.

- [ ] **Step 3: Add the durable field and event**

In `crates/server/src/sessions/supervisor.rs`, on `SessionRecord`:

```rust
    /// What this session last reported it was doing.
    ///
    /// Durable, not a cache. The session's own journal still owns it — the
    /// session folds a transition and then reports it here — so this copy is a
    /// projection, never a source. It is persisted for the reason the title is:
    /// a list has to render it without loading the session, and loading is not
    /// free, because it re-attempts an interrupted provision.
    ///
    /// `Running` and `Provisioning` can go stale under a crash. They go stale
    /// identically in the session's own journal, which also only learns better
    /// when it loads and repairs, so this is no less accurate than the truth.
    pub status: SessionStatus,
```

Add the event beside `SessionNamed`:

```rust
    /// A session reached a new status. Journaled only when it differs from what
    /// is already recorded, so a session that loads and reports the status it
    /// recovered writes nothing.
    SessionStatusChanged {
        id: SessionId,
        status: SessionStatus,
    },
```

Fold it in `apply_event`:

```rust
            SessionSupervisorEvent::SessionStatusChanged { id, status } => {
                if let Some(rec) = state.sessions.get_mut(&id) {
                    rec.status = status;
                }
            }
```

And in the `SessionCreated` arm of `apply_event`, construct the record with `status: SessionStatus::Provisioning` — a fresh session is provisioning and says so until its vendor confirms the runtime.

- [ ] **Step 4: Persist the report, delete the cache**

Replace the `SessionStatusChanged` command arm:

```rust
            SessionSupervisorCommand::SessionStatusChanged { id, status } => {
                self.publish(&id, &status);
                // Idempotent on purpose: every load reports what it recovered,
                // and every persisted batch reports again. Only a real
                // transition is worth a write.
                match state.sessions.get(&id) {
                    Some(rec) if rec.status != status => {
                        CommandEffect::persist(vec![
                            SessionSupervisorEvent::SessionStatusChanged { id, status },
                        ])
                    }
                    _ => CommandEffect::none(),
                }
            }
```

Then delete the `status` field from the `SessionSupervisor` struct and its three remaining uses:
- `forget` (`supervisor.rs:493`) — delete the `self.status.remove(id);` line.
- `Create` (`supervisor.rs:671`) — delete `self.status.insert(id.clone(), SessionStatus::Provisioning);`. Keep the `self.publish(...)` call: the SSE stream still needs to hear about it, and the fold in Step 3 now records it.
- `List` — reply with `(id, rec)` pairs:

```rust
            SessionSupervisorCommand::List { reply } => {
                let sessions = state
                    .sessions
                    .iter()
                    .map(|(id, rec)| (id.clone(), rec.clone()))
                    .collect();
                let _ = reply.send(sessions);
                CommandEffect::none()
            }
```

Change the `List` variant's reply type to `ReplyTo<Vec<(SessionId, SessionRecord)>>` and update its doc comment — `status` is no longer `None` for anything.

- [ ] **Step 5: Give `offload_idle` the state it now needs**

`offload_idle` reads `self.status` to keep a running or provisioning session loaded. Change the signature to take the state, and the `Tick` arm to pass it:

```rust
    async fn offload_idle(&mut self, state: &SessionSupervisorState) {
```

```rust
                if matches!(
                    state.sessions.get(*id).map(|rec| &rec.status),
                    Some(&SessionStatus::Running | &SessionStatus::Provisioning)
                ) {
                    return false;
                }
```

```rust
            SessionSupervisorCommand::Tick => {
                self.offload_idle(state).await;
                CommandEffect::none()
            }
```

- [ ] **Step 6: Collapse `summary` to two arguments**

In `crates/server/src/http/handlers.rs`:

```rust
pub(crate) fn summary(id: &str, rec: &SessionRecord) -> SessionSummary {
    SessionSummary {
        id: id.to_string(),
        // Still `Some(..)`: the wire field stays optional until Task 4 removes
        // the option along with the em-dash path it fed.
        status: Some(status_kind(&rec.status)),
        name: rec.spec.name.clone(),
        created_at: rec.created_at,
        last_error: status_reason(&rec.status),
        workflow: rec.spec.workflow_name().map(str::to_string),
        annotations: wire_annotations(&rec.annotations),
    }
}
```

Update every call site the compiler names: `handlers.rs:135,148`, `agents.rs:182`, `workflows.rs:254,271`, `routines.rs:140`. The three that construct a throwaway `SessionRecord` for a just-created session (`handlers.rs:126`, `agents.rs:173`, `workflows.rs:245`) need the new `status` field — `SessionStatus::Provisioning` for a create, `SessionStatus::Idle` for the agent-invoke path that already passed `Idle`.

- [ ] **Step 7: Run the tests**

```bash
cargo test -p horsie-server --lib sessions:: http::
```

Expected: PASS, including both tests from Step 1.

- [ ] **Step 8: Commit**

```bash
git add -A crates/server/src
git commit -m "feat(sessions): persist a session's status in the registry"
```

---

### Task 3: A run that completed says `Finished`

**Files:**
- Modify: `crates/server/src/sessions/spec.rs` — `SessionStatus`, `status_kind`, `status_reason`
- Modify: `crates/server/src/sessions/session_actor/run.rs` — the `RunFinished` fold
- Modify: `crates/models/fluorite/session.fl` — `SessionStatusKind`
- Modify: `clients/web/src/lib/status.ts` — `META`
- Test: `crates/server/src/sessions/session_actor/` run tests, `clients/web/src/lib/status.test.ts`

**Interfaces:**
- Consumes: `SessionRecord.status` from Task 2.
- Produces: `SessionStatus::Finished` (Rust) / `SessionStatusKind.Finished` (wire). Non-terminal: a retry, fork or new message moves it back to `Running`.

- [ ] **Step 1: Write the failing test**

In the tests module at the bottom of `crates/server/src/sessions/session_actor/run.rs`, beside `a_run_starts_itself_and_routes_on_its_first_steps_output`, which this borrows its provider script from:

```rust
/// A run that reached a terminal step with no error says so, and keeps saying
/// so once it is cold. `Idle` could not tell it apart from a run that stopped
/// part-way and is waiting for someone to retry a step.
#[tokio::test]
async fn a_completed_run_reports_finished() {
    use horsie_agentcore::testkit::{MockProvider, Script};
    let provider = MockProvider::scripted(
        Script::of([Ok(concludes(serde_json::json!({"severity": "p0"})))]).then_repeating_with(
            || {
                Ok(horsie_agentcore::CompletionResponse {
                    parts: vec![horsie_agentcore::ContentPart::Text(
                        horsie_agentcore::TextPart {
                            text: "fixed".to_string(),
                        },
                    )],
                    stop_reason: horsie_agentcore::StopReason::EndTurn,
                    usage: horsie_agentcore::Usage::without_cache(1, 1),
                })
            },
        ),
    );
    let (_f, _session, id, journal) = spawn_run_with_provider(provider).await;
    let state = wait_for_state(&journal, id, "the run to finish", |s| {
        s.run
            .as_ref()
            .is_some_and(|r| r.status == crate::sessions::workflow::WorkflowRunStatus::Finished)
    })
    .await;
    assert_eq!(
        state.status,
        SessionStatus::Finished,
        "a run that completed is not merely idle"
    );
}
```

`wait_for_state` (`testing.rs:488`) folds the session's journal and polls until the predicate holds, so this asserts on the recovered state rather than on a live actor's field.

- [ ] **Step 2: Run it and watch it fail**

```bash
cargo test -p horsie-server --lib a_completed_run_reports_finished
```

Expected: FAIL to compile — no variant `Finished`.

- [ ] **Step 3: Add the variant**

In `crates/server/src/sessions/spec.rs`, after `AwaitingInput`:

```rust
    /// A workflow run reached a terminal step with no error.
    ///
    /// Not terminal for the session: a retry, a fork or a new message moves it
    /// back to `Running`. `Unrecoverable` is the only status a session cannot
    /// leave. Unreachable for a conversation, which is never over.
    Finished,
```

Add it to `status_kind` (→ `SessionStatusKind::Finished`) and to the `None` arm of `status_reason`.

- [ ] **Step 4: Fold it where the run finishes**

In `crates/server/src/sessions/session_actor/run.rs`, the `RunFinished` arm of `apply` currently sets `state.status = SessionStatus::Idle;`. Change it to `SessionStatus::Finished`.

Leave `WorkflowRunStatus` alone. It stays internal run state: the driver's `is_terminal()` gates whether another step starts, which is a different question from what a person can do with the session.

- [ ] **Step 5: Run the Rust tests**

```bash
cargo test -p horsie-server --lib sessions::
```

Expected: PASS. Fix any exhaustive `match` the compiler names — `SessionStatus` is matched exhaustively in several places by design.

- [ ] **Step 6: Add the wire variant and regenerate**

In `crates/models/fluorite/session.fl`, inside `enum SessionStatusKind`, after `AwaitingInput`:

```
    /// A workflow run completed with no error. Not terminal: a retry or a new
    /// message moves it back to `Running`.
    Finished,
```

```bash
make types
git status --short clients/web/src/generated
```

Expected: `sessionStatusKind.ts` modified, nothing orphaned. If `fluorite` is missing, `cargo install fluorite_codegen` at the version CI pins.

- [ ] **Step 7: Teach the web client the new lamp**

In `clients/web/src/lib/status.ts`, add to `META`:

```ts
  [SessionStatusKind.Finished]: {
    label: "Finished",
    tone: "ready",
    busy: false,
    canSend: true,
    hint: "This run completed. Retry a step to take it further.",
  },
```

And a test in `clients/web/src/lib/status.test.ts`:

```ts
it("gives a finished run its own settled lamp", () => {
  const meta = statusMeta(SessionStatusKind.Finished);
  expect(meta.label).toBe("Finished");
  expect(meta.busy).toBe(false);
  expect(meta.tone).not.toBe("off");
});
```

- [ ] **Step 8: Run the web tests**

```bash
cd clients/web && bun install && bun run test src/lib/status.test.ts
```

Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add -A crates/server/src crates/models/fluorite clients/web/src
git commit -m "feat(sessions): a completed run says Finished"
```

---

### Task 4: A status is always known

With the status persisted, `Option<SessionStatusKind>` no longer models anything — there is no session the server has nothing to say about. This task removes the option and the em-dash path it fed.

**Files:**
- Modify: `crates/models/fluorite/session.fl` — `SessionSummary.status`, `SessionDetail.status`
- Modify: `crates/server/src/http/handlers.rs` — `summary`, `detail`
- Modify: `clients/web/src/lib/status.ts` — delete `UNKNOWN_STATUS`, narrow `statusMeta`
- Modify: `clients/web/src/components/StatusBadge.tsx`, `clients/web/src/components/SessionRow.tsx`
- Test: `clients/web/src/lib/status.test.ts`

**Interfaces:**
- Consumes: `summary` from Task 2, `Finished` from Task 3.
- Produces: `statusMeta(status: SessionStatusKind): StatusMeta` — no null accepted, no `UNKNOWN_STATUS` export.

- [ ] **Step 1: Make the wire field non-optional**

In `crates/models/fluorite/session.fl`, on `SessionSummary` and `SessionDetail`, replace `status: Option<SessionStatusKind>` with:

```
    /// What the session last reported. Always known: the registry keeps a
    /// durable copy, so a cold session answers without being loaded.
    status: SessionStatusKind,
```

```bash
make types && git status --short clients/web/src/generated
```

- [ ] **Step 2: Drop the `Some(...)` wrappers**

In `crates/server/src/http/handlers.rs`, `summary` returns `status: status_kind(&rec.status)`. Find the session *detail* builder in the same file and give it the same treatment — its status comes from the actor snapshot, which is non-optional already.

```bash
cargo build -p horsie-server --lib
```

Fix every site the compiler names.

- [ ] **Step 3: Write the failing web test**

In `clients/web/src/lib/status.test.ts`:

```ts
it("has no unknown state — every status the server can send has a lamp", () => {
  for (const kind of Object.values(SessionStatusKind)) {
    const meta = statusMeta(kind);
    expect(meta.label).not.toBe("—");
    expect(meta.tone).not.toBe("off");
  }
});
```

- [ ] **Step 4: Run it and watch it fail**

```bash
cd clients/web && bun run test src/lib/status.test.ts
```

Expected: FAIL — `statusMeta` still routes unknown kinds to `UNKNOWN_STATUS`, and `Finished` is only present if Task 3 landed. If it passes, check that `SessionStatusKind` is a real object at runtime and not a type-only import.

- [ ] **Step 5: Delete the unknown path**

In `clients/web/src/lib/status.ts`, delete the `UNKNOWN_STATUS` export and narrow the signature:

```ts
export function statusMeta(status: SessionStatusKind): StatusMeta {
  return META[status];
}
```

Then fix the two consumers the type checker names: `StatusBadge.tsx` (its `status` prop drops `| null | undefined`) and `SessionRow.tsx` (the `meta.label !== "—"` guard in the subtitle goes away — the label is always worth showing).

- [ ] **Step 6: Typecheck and test**

```bash
cd clients/web && bun run build && bun run test
```

`bun run build` is the typecheck that matters; `tsc --noEmit` alone is a no-op in this project.

- [ ] **Step 7: Commit**

```bash
git add -A crates/models/fluorite crates/server/src clients/web/src
git commit -m "feat(sessions): a status is always known, so stop modelling it as absent"
```

---

### Task 5: `WorkflowStatus` leaves the wire

The run detail document carries a second status vocabulary that now says nothing session status cannot. `Suspended` is the one word it had that session status lacks, and the run log already carries the fact underneath it: a run is suspended exactly when its newest execution was cancelled.

**Files:**
- Modify: `crates/models/fluorite/workflow.fl` — delete `WorkflowStatus` and its six marker structs, drop `WorkflowRunGraph.status`
- Modify: `crates/server/src/http/workflows.rs` — delete `wire_status`, drop the field from `get_run_graph`
- Modify: `clients/web/src/pages/workflows/WorkflowRunView.tsx` — derive from session status and the log
- Modify: `clients/web/src/hooks/useWorkflows.ts` — the poll-stop predicate
- Test: `clients/web/src/pages/workflows/WorkflowRunView.test.ts`

**Interfaces:**
- Consumes: `SessionStatusKind.Finished` from Task 3, non-optional `status` from Task 4.
- Produces: `resumePoint(graph: WorkflowRunGraph)` and `parkedStep(graph, status: SessionStatusKind)` — both keep their return types; `parkedStep` gains the session status as a second argument, `resumePoint` loses its status gate entirely.

- [ ] **Step 1: Write the failing tests**

In `clients/web/src/pages/workflows/WorkflowRunView.test.ts`:

```ts
it("offers a resume point when the newest execution was cancelled", () => {
  const graph = graphWith([
    { step: "triage", runs: [{ index: 0, status: { type: "Concluded" } }] },
    { step: "fix", runs: [{ index: 1, status: { type: "Cancelled" } }] },
  ]);
  expect(resumePoint(graph)).toEqual({ step: "fix", index: 1 });
});

it("offers no resume point once a later execution has run", () => {
  const graph = graphWith([
    { step: "fix", runs: [
      { index: 1, status: { type: "Cancelled" } },
      { index: 2, status: { type: "Concluded" } },
    ] },
  ]);
  expect(resumePoint(graph)).toBeUndefined();
});
```

Use whatever `graphWith`-shaped helper the existing tests in this file already build; if there is none, build the `WorkflowRunGraph` literal inline without a `status` field.

- [ ] **Step 2: Run them and watch them fail**

```bash
cd clients/web && bun run test src/pages/workflows/WorkflowRunView.test.ts
```

Expected: the first fails (`resumePoint` returns undefined — the literal has no `status: "Suspended"`), and the second is the new behaviour that the old status gate never had to express.

- [ ] **Step 3: Derive suspension from the log**

In `WorkflowRunView.tsx`:

```ts
/**
 * Where a suspended run stopped, so the page can offer to resume it.
 *
 * A run is suspended when its newest execution was cancelled — by Interrupt, or
 * by the server restarting under it — and it is deliberately not resumed on its
 * own, because how far that step got is unknowable. Read off the log rather
 * than a status word: the log is where the fact lives, and a retry appends
 * rather than truncating, so a later execution is what says the run moved on.
 */
export function resumePoint(
  graph: WorkflowRunGraph,
): { step: string; index: number } | undefined {
  let newest: { step: string; index: number; cancelled: boolean } | undefined;
  for (const node of graph.nodes) {
    for (const run of node.runs) {
      if (newest === undefined || run.index > newest.index) {
        newest = {
          step: node.step,
          index: run.index,
          cancelled: run.status.type === "Cancelled",
        };
      }
    }
  }
  return newest?.cancelled ? { step: newest.step, index: newest.index } : undefined;
}
```

Change `parkedStep` to take the session status:

```ts
export function parkedStep(
  graph: WorkflowRunGraph,
  status: SessionStatusKind,
): { step: string; agentId: string } | undefined {
  if (status !== SessionStatusKind.AwaitingInput || graph.current === undefined) {
    return undefined;
  }
  ...
}
```

Replace `STATUS_TEXT` / `STATUS_TONE` with `statusMeta(status).label` and `TONE_TEXT[statusMeta(status).tone]` from `../../lib/status` — one vocabulary, one lamp. The component reads the session status from `useSession(sessionId)`.

- [ ] **Step 4: Stop the poll on the session's status**

In `clients/web/src/hooks/useWorkflows.ts`, `useWorkflowRun`'s `refetchInterval` currently reads `query.state.data?.status.type`. The graph no longer carries a status, so move the stop condition to the caller: keep polling at 2s unconditionally in the hook, and have `WorkflowRunView` pass `enabled`/`refetchInterval: false` once `statusMeta(status)` reports a settled state (`Finished`, `Failed`, `Unrecoverable`). Document why in a comment: the graph is a snapshot of the log, and only the session knows whether anything can still change.

- [ ] **Step 5: Delete the wire type**

In `crates/models/fluorite/workflow.fl`, delete the `WorkflowStatus` union and `PendingStatus`, `RunningStatus`, `SuspendedStatus`, `AwaitingInputStatus`, `FinishedStatus`, `FailedStatus`, and remove `status: WorkflowStatus` from `WorkflowRunGraph`.

In `crates/server/src/http/workflows.rs`, delete `wire_status` and the `status` field from the `WorkflowRunGraph` it builds, plus the now-unused imports.

```bash
cd clients/web && bun run generate-types && cd ../.. && git status --short clients/web/src/generated
```

Expected: the six marker files and `workflowStatus.ts` are **not** deleted by generation — remove them by hand and check `workflow/index.ts` no longer re-exports them.

- [ ] **Step 6: Verify**

```bash
cargo test -p horsie-server --lib http::
cd clients/web && bun run build && bun run test
```

- [ ] **Step 7: Commit**

```bash
git add -A crates/models/fluorite crates/server/src clients/web
git commit -m "refactor(workflows): one status vocabulary, so drop the run's own"
```

---

### Task 6: One list, filtered

**Files:**
- Modify: `crates/server/src/http/handlers.rs` — `list_sessions` gains a query filter
- Modify: `crates/server/src/http/mod.rs` — delete the `GET /api/workflows/{name}/runs` route
- Delete: `crates/server/src/http/workflows.rs::list_runs`, `crates/server/src/http/routines.rs::routine_sessions` duplication
- Modify: `crates/models/fluorite/workflow.fl` — delete `WorkflowRunsResponse`
- Modify: `clients/web/src/hooks/useWorkflows.ts`, `clients/web/src/pages/workflows/WorkflowDetailPage.tsx`, `clients/web/src/api/client.ts`
- Test: `crates/tests/tests/session_server_e2e.rs`, `clients/web/e2e/t-workflows.spec.ts`

**Interfaces:**
- Consumes: `summary` from Task 2, non-optional status from Task 4.
- Produces: `GET /api/sessions?workflow=<name>` and `GET /api/sessions?routine=<name>`, both returning the existing `ListSessionsResponse`. With neither parameter, routine runs stay excluded, as today.

- [ ] **Step 1: Write the failing e2e test**

Replace `a_workflows_run_list_reports_the_outcome_of_a_cold_run` in `crates/tests/tests/session_server_e2e.rs` (or add it, if that test only exists on the superseded branch) with one that reads the filtered session list:

The `define_e2e_workflow` helper does not exist yet — extract it first from the top of `a_workflow_run_is_created_driven_and_retried_over_http`, which currently inlines the same four requests (configure provider, configure model, create the `wf-step` preset, create the two-step `e2e-flow` definition). Extracting it is a mechanical move with no behaviour change, and it is what lets this second test exist at all.

```rust
/// A workflow's runs are sessions, so they are read from the session list with
/// a filter — and a cold run still says what became of it, because the registry
/// keeps its status rather than caching it for as long as it happens to be
/// loaded.
///
/// Cold is the state every run on the page is in: nothing is loaded at boot
/// either, and the idle sweep gets whatever is. Reading a row through the
/// session actor is not the fix, because loading re-attempts an interrupted
/// provision — so the vendor must hear nothing at all from a list request.
#[tokio::test]
async fn a_cold_run_reports_finished_in_the_filtered_session_list() {
    let mock = MockLlmServer::builder().build().await;
    for _ in 0..4 {
        mock.queue_response("step done");
    }
    let tmp = tempfile::tempdir().unwrap();
    let agent = FakeRuntimeVendor::builder("mock")
        .serve_in_process()
        .await
        .expect("fake agent");
    let clock = Arc::new(TestClock::new());
    let server = start_server_with(
        tmp.path(),
        Some(agent.link()),
        &mock.url(),
        Some(clock.clone()),
    )
    .await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", server.addr);
    define_e2e_workflow(&client, &base, &mock.url()).await;

    let res = client
        .post(format!("{base}/api/workflows/e2e-flow/runs"))
        .json(&serde_json::json!({
            "input": "the build is red",
            "environment": {"type": "Runtime", "value": {"vendor": "mock"}}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 201, "start a run");
    let v: serde_json::Value = res.json().await.unwrap();
    let id = v["session"]["id"].as_str().unwrap().to_string();
    wait_for_session_status(&client, &base, &id, "Finished").await;

    // Let it go cold.
    clock.advance(Duration::from_secs(600));
    let _ = server.supervisor.tell(SessionSupervisorCommand::Tick).await;
    wait_until("the finished run to be unloaded", async || {
        agent
            .signals()
            .contains(&format!("hibernate:{id}"))
            .then_some(())
    })
    .await;
    let before = agent.signals();

    let res = client
        .get(format!("{base}/api/sessions?workflow=e2e-flow"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 200);
    let body: serde_json::Value = res.json().await.unwrap();
    let row = body["sessions"]
        .as_array()
        .expect("a list of sessions")
        .iter()
        .find(|s| s["id"] == serde_json::json!(id))
        .unwrap_or_else(|| panic!("the run is missing from the filtered list: {body}"));
    assert_eq!(
        row["status"],
        serde_json::json!("Finished"),
        "a cold run still knows how it ended: {body}"
    );
    assert_eq!(
        agent.signals(),
        before,
        "listing runs woke one, which re-attempts its provision"
    );

    server.shutdown().await;
}

/// Poll a session until its status is `want` (10s cap).
///
/// Replaces `wait_for_run_status`, which polled the run graph — the graph no
/// longer carries a status, because a run's status is the session's.
async fn wait_for_session_status(
    client: &reqwest::Client,
    base: &str,
    id: &str,
    want: &str,
) {
    wait_until(&format!("the run to reach {want}"), async || {
        let body: serde_json::Value = client
            .get(format!("{base}/api/sessions/{id}"))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        (body["status"] == serde_json::json!(want)).then_some(())
    })
    .await;
}
```

Delete `wait_for_run_status` and repoint its other caller in `a_workflow_run_is_created_driven_and_retried_over_http` at `wait_for_session_status`. That test asserts `"Finished"` and `"Suspended"` at different points; `Suspended` becomes `"Idle"` — which is the collapse this plan makes, and the resume affordance it was standing in for is asserted by the retry request that follows.

- [ ] **Step 2: Run it and watch it fail**

```bash
cargo test -p horsie-tests a_cold_run_reports_finished_in_the_filtered_session_list
```

Expected: FAIL — the filter is ignored, so the assertion finds the row but nothing scoped it; more likely a 200 with every session. Confirm the failure is about the filter, not about the status, before continuing.

- [ ] **Step 3: Add the filter**

In `crates/server/src/http/handlers.rs`:

```rust
/// Which sessions to list.
///
/// A run of a workflow or a routine is an ordinary session, so it is listed
/// here rather than from a second endpoint that would re-derive the same row.
/// With neither filter, routine runs are excluded: a routine on a timer would
/// otherwise bury the sessions somebody is actually having.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSessionsQuery {
    workflow: Option<String>,
    routine: Option<String>,
}

pub async fn list_sessions(
    Scope(state): Scope,
    Query(q): Query<ListSessionsQuery>,
) -> Result<impl IntoResponse, Api> {
    let sessions = ask(&state, |reply| SessionSupervisorCommand::List { reply }).await?;
    let mut sessions: Vec<_> = sessions
        .iter()
        .filter(|(_, rec)| match (&q.workflow, &q.routine) {
            (Some(w), _) => rec.spec.workflow_name() == Some(w.as_str()),
            (_, Some(r)) => rec.spec.routine() == Some(r.as_str()),
            _ => rec.spec.routine().is_none(),
        })
        .map(|(id, rec)| summary(id, rec))
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.created_at));
    Ok(Json(ListSessionsResponse { sessions }))
}
```

Note the sort: `list_runs` sorted newest-first and `list_sessions` did not. Newest-first is right for both.

- [ ] **Step 4: Delete the two duplicates**

- Delete `list_runs` from `crates/server/src/http/workflows.rs` and the `get(workflows::list_runs)` half of the route in `crates/server/src/http/mod.rs:354` — keep `.post(workflows::start_run)`.
- Delete `WorkflowRunsResponse` from `crates/models/fluorite/workflow.fl`, regenerate, and remove the orphaned generated file by hand.
- Delete the `GET /api/routines/{name}/sessions` route and its `get_routine_sessions` handler (`routines.rs:117`) and `RoutineSessionsResponse` from the schema — `?routine=<name>` is the same list. Keep the private `routine_sessions` helper: its other caller at `routines.rs:80` needs the ids to delete a routine's sessions along with it. Repoint the web client's routine detail page at `api.sessions.list({ routine: name })`.

The 404-on-unknown-workflow behaviour `list_runs` had is dropped deliberately: the workflow page already calls `GET /api/workflows/:name`, which is where its 404 comes from.

- [ ] **Step 5: Point the web client at the session list**

In `clients/web/src/hooks/useWorkflows.ts`:

```ts
/** A workflow's runs, newest first — sessions, filtered. Polled while the page
 * is open: a run's status is the session's, and it changes under us. */
export function useWorkflowRuns(name: string | undefined) {
  return useQuery({
    queryKey: name ? workflowKeys.runs(name) : ["workflows", "none", "runs"],
    queryFn: () => api.sessions.list({ workflow: name as string }),
    enabled: !!name,
    refetchInterval: 5_000,
    select: (r) => r.sessions,
  });
}
```

Add the optional filter argument to `api.sessions.list` in `clients/web/src/api/client.ts`, and delete `api.workflows.runs`. In `WorkflowDetailPage.tsx` the rows are already `SessionSummary`, so the only change is `<StatusBadge status={s.status} />` now receiving a non-optional status.

- [ ] **Step 6: Update the Playwright expectation**

In `clients/web/e2e/t-workflows.spec.ts`, the run row must say what became of the run:

```ts
  await page.goto(`${appBase}/workflows/${WORKFLOW}`);
  await expect(page.getByTestId("workflow-run-row")).toHaveCount(1);
  const status = page.getByTestId("workflow-run-row").getByTestId("run-status");
  await expect(status).toHaveText("Finished");
```

Add the `data-testid="run-status"` to the badge in `WorkflowDetailPage.tsx` if it is not already there.

- [ ] **Step 7: Verify the whole workspace**

`-p horsie-server` is a false green for a route change: the integration tests and the web e2e call these routes too.

```bash
make check
cd clients/web && bun run build && bun run test
TMPDIR=/tmp bun run test:e2e
```

- [ ] **Step 8: Commit and open the PR**

```bash
git add -A
git commit -m "refactor(api): a workflow's runs are sessions, so list them as sessions"
git push -u origin qa/status-unify
gh pr create --title "One status vocabulary for every session" --body "..."
```

Do not enable auto-merge. A green PR is the finish line.

---

## Notes for the reviewer

- **PR #335 is superseded.** Once this lands, `SessionRecord.run_status`, `WorkflowRunSummary` and the second badge vocabulary it introduced are all unnecessary — the bug it fixed (every past run rendering an em dash) is fixed here by the persisted status plus `Finished`. Close it rather than rebasing it.
- **`WorkflowRunStatus` survives as internal state.** The driver's `is_terminal()` decides whether another step starts. That is not the same question as what a person can still do with the session, which is why nothing but `Unrecoverable` is terminal at the session level.
- **`Suspended` folds into `Idle` deliberately.** The distinction it carried is now `Idle` (stopped part-way) versus `Finished` (ran to completion), and the run page's resume affordance reads the fact off the log rather than a status word.
