# SQLite journal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A durable `Journal` backend with real snapshots and compaction, so recovery stops re-folding whole transcripts and `replay(after_seq)` stops re-reading whole logs.

**Architecture:** `SqliteJournal` in the server crate over the settings pool, with stored (not positional) sequence numbers. `Journal::replay` yields `(seq, bytes)` so `recover()` reads the number instead of counting. Backend chosen by config, leaving room for Postgres.

**Tech Stack:** Rust, sqlx 0.8 (sqlite, runtime-tokio, migrate), tokio.

Design spec: `docs/superpowers/specs/2026-08-03-sqlite-journal-design.md`

## Global Constraints

- `FileJournal` stays and keeps working. Server only switches backend.
- Schema lives in `server/migrations/` — one migrator per database, so the journal cannot own its own.
- No importer, no fallback, no backward compatibility.
- Every field of every snapshotted state gets `#[serde(default)]`.
- `synchronous = FULL` (matches `FileJournal`'s `sync_all` durability, which `PersistAndAck` promises) plus `journal_mode = WAL`.
- Verify with `make check`; web e2e per the repo's documented bun/playwright flow.

---

### Task 1: `replay` yields its sequence number

**Files:** `actor/src/journal.rs`, `actor/src/file_journal.rs`, `actor/src/runtime.rs`, `actor/src/testkit.rs`, `actor/tests/journal_conformance.rs`, `supervisor/src/history.rs`, `server/src/sessions/{events.rs,session_actor.rs}`

- [ ] **Step 1: Change the trait and both backends**

```rust
async fn replay(&self, pid: &PersistenceId, after_seq: u64)
    -> BoxStream<'_, JournalResult<(u64, Vec<u8>)>>;
```

`InMemoryJournal` already stores `(seq, bytes)` — yield the pair. `FileJournal::decode_after` already computes `seq` — yield it instead of discarding it.

- [ ] **Step 2: `recover()` takes the number instead of counting**

```rust
    let mut stream = journal.replay(pid, seq_nr).await;
    while let Some(item) = stream.next().await {
        let (seq, bytes) = item?;
        let event = serde_json::from_slice::<A::Event>(&bytes)
            .map_err(|e| JournalError::Serialization(e.to_string()))?;
        state = A::apply_event(state, event);
        seq_nr = seq;
    }
```

- [ ] **Step 3: Update the remaining readers** — `supervisor/src/history.rs` (two loops) and the two `#[cfg(test)]` fold helpers in the server destructure the pair and ignore the seq.

- [ ] **Step 4: `cargo test --workspace`, then commit**

```bash
git commit -m "refactor(actor): replay yields stored sequence numbers"
```

---

### Task 2: Schema and WAL

**Files:** `server/migrations/0016_journal.sql` (create), `server/src/config/store.rs`

- [ ] **Step 1: Write the migration** — the three tables from the spec, verbatim.

- [ ] **Step 2: Enable WAL in `open_pool`**

```rust
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Full)
```

- [ ] **Step 3: Assert the pragmas took**, since a silently-ignored pragma is exactly the failure this guards against:

```rust
#[tokio::test]
async fn the_pool_runs_in_wal_mode() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}/t.db", dir.path().display());
    let pool = open_pool(&url).await.unwrap();
    let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(mode.to_lowercase(), "wal");
}
```

- [ ] **Step 4: Commit**

---

### Task 3: `SqliteJournal`

**Files:** `server/src/journal/mod.rs`, `server/src/journal/sqlite.rs` (create), `server/src/lib.rs`

**Interfaces:** `SqliteJournal::new(pool: SqlitePool) -> Self`, implementing `horsie_actor::Journal`.

- [ ] **Step 1: Implement it** — `persist` in one `BEGIN IMMEDIATE` transaction bumping `last_seq` then inserting rows; `replay` as an indexed range scan; the rest as plain statements. Resolve `log_id` via the `(kind, id)` unique index, inserting the log row on demand.

- [ ] **Step 2: Unit tests** for numbering across compaction and for `copy_snapshot` seeding a fork.

- [ ] **Step 3: Add the backend to the conformance suite** — a `sqlite` module in `actor/tests/journal_conformance.rs` cannot see the server crate, so the suite moves to a shared location the server test can call, or the server grows its own conformance test invoking the same assertions. Prefer exporting the assertions from `horsie_actor::testkit` under the existing `test-util` feature and calling them from both.

- [ ] **Step 4: All ten conformance assertions pass. Commit.**

---

### Task 4: Wiring and snapshot policy

**Files:** `server/src/bin/horsie-server/{config.rs,main.rs}`, `workflow/src/agent_actor.rs`

- [ ] **Step 1: Config knob** — `storage.journal` (`"file" | "sqlite"`, default `"sqlite"`), resolved in `main.rs` into `Arc<dyn Journal>`.

- [ ] **Step 2: Throttled snapshot at turn boundaries** — `AgentActor` counts events in `on_events_persisted` and requests a snapshot at the next turn boundary once past `SNAPSHOT_EVERY_EVENTS` (200), resetting on request.

- [ ] **Step 3: `#[serde(default)]` on every snapshotted state field** — `AgentState.messages` is the one missing it; audit `WorkflowState`, `SessionState`, `SessionSupervisorState` too.

- [ ] **Step 4: Test that a long session snapshots and recovers from it. Commit.**

---

### Task 5: End-to-end and the gate

**Files:** `tests/tests/session_server_e2e.rs`

- [ ] **Step 1:** A server on a SQLite journal runs turns, restarts, and reports an identical transcript.
- [ ] **Step 2:** `make check` and the web e2e suite.
- [ ] **Step 3:** Commit and open the PR.
