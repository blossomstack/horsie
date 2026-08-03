# SQLite-backed journaling for the event-sourced actor runtime

**Status:** design approved, implementation in progress
**Date:** 2026-08-03
**Scope:** a durable `Journal` backend with real snapshots, plus the sequence-number contract the trait already claims
**Depends on:** #149 (state-sourced read APIs) — `recover()` is now the only journal reader in the server, which is what makes snapshots and compaction internal details

## Problem

`FileJournal` (`actor/src/file_journal.rs`) is the only durable backend, and four of its seven trait methods are lies:

```rust
async fn save_snapshot(...)        -> JournalResult<()>            { Ok(()) }
async fn latest_snapshot(...)      -> JournalResult<Option<...>>   { Ok(None) }
async fn delete_events_before(...) -> JournalResult<()>            { Ok(()) }
async fn copy_snapshot(...)        -> JournalResult<()>            { Ok(()) }
```

Consequences, in order of severity:

**Every recovery is a full replay.** An agent's state is its entire transcript, so loading a long session re-reads and re-folds every event ever written. `snapshot_state` in `actor/src/runtime.rs` faithfully calls `save_snapshot` and gets `Ok(())` back, so the machinery exists and does nothing.

**Workflow fork silently produces an empty agent.** `workflow_actor.rs` calls `copy_snapshot` to seed a forked session, gets `Ok(())`, and spawns an agent with zero history — then sends it a correction message. Worse than an error.

**Journals grow without bound.** Nothing is ever compacted, because compaction is a no-op.

**Sequence numbers are positional, not stored.** `FileJournal` counts events during replay (`decode_after`), while the trait's own doc says "an event's sequence number is stable for the life of the log even after older events are compacted away." Compaction would renumber survivors and break `replay(after_seq)`. `InMemoryJournal` stores them, so all tests pass and nothing catches it — five conformance tests in `actor/tests/journal_conformance.rs` are red for `FileJournal` alone.

**Read amplification on every replay.** `decode_after` reads the whole file and base64-decodes every record from position 1, discarding those at or below the cursor. `replay(pid, 5000)` on a 5000-event log costs 5000 decodes to yield nothing.

## Design

### Placement

`SqliteJournal` lives in the **server crate** (`server/src/journal/`), implementing `horsie_actor::Journal`. `FileJournal` stays in `horsie-actor`, unchanged, so the CLI/daemon path is untouched.

The server crate specifically, because the journal shares the settings database: two `sqlx::migrate!()` migrators on one database collide on the `_sqlx_migrations` table, so the schema must live in the server's existing `server/migrations/` chain. Keeping the DDL and the queries in one crate beats splitting them across a crate boundary. `SqliteJournal::new(pool)` takes the already-migrated pool from `DbConfigStore::open`.

Backend selection is configuration, so the planned Postgres backend is a third arm rather than a rewrite:

```toml
[storage]
journal = "sqlite"   # "file" | "sqlite"
```

Default is `sqlite`. No importer and no fallback: a `sqlite` install starts with an empty log, as decided.

### Schema

```sql
CREATE TABLE journal_logs (
  log_id   INTEGER PRIMARY KEY,
  kind     TEXT    NOT NULL,
  id       TEXT    NOT NULL,
  last_seq INTEGER NOT NULL DEFAULT 0,
  UNIQUE (kind, id)
);

CREATE TABLE journal_events (
  log_id  INTEGER NOT NULL REFERENCES journal_logs(log_id) ON DELETE CASCADE,
  seq     INTEGER NOT NULL,
  payload BLOB    NOT NULL,
  PRIMARY KEY (log_id, seq)
) WITHOUT ROWID;

CREATE TABLE journal_snapshots (
  log_id INTEGER PRIMARY KEY REFERENCES journal_logs(log_id) ON DELETE CASCADE,
  seq    INTEGER NOT NULL,
  state  BLOB    NOT NULL
);
```

`last_seq` on the log row, rather than `MAX(seq)` over the events, is the crux: it is why deleting events cannot renumber survivors, and why `copy_snapshot` can seed a fork that continues numbering from the source's snapshot.

`WITHOUT ROWID` makes the primary key the storage order, so `WHERE log_id = ? AND seq > ? ORDER BY seq` is a contiguous range scan with no secondary-index indirection and no row-payload hop.

Payloads are `BLOB`. The events are already `serde_json::to_vec` bytes, so unlike `FileJournal` there is no base64 layer — roughly 25% less storage and no encode/decode per event.

### Operations

- **`persist`** — one `BEGIN IMMEDIATE` transaction: bump `last_seq` by the batch size, insert the rows. Atomicity gives the "a torn batch is dropped whole" property that `FileJournal` gets from line framing, as a real guarantee rather than a parsing convention.
- **`replay(after_seq)`** — `WHERE log_id = ? AND seq > ? ORDER BY seq`, an index range scan. This is the read fix: resuming at 5000 reads the rows after 5000 rather than decoding 5000 and discarding them.
- **`latest_snapshot`** — one indexed row.
- **`delete_events_before`** — `DELETE WHERE log_id = ? AND seq <= ?`. Correct, and now actually reachable.
- **`copy_snapshot`** — insert a destination log with `last_seq` = the source snapshot's seq, copy the snapshot row, leave the event log empty. Fixes the silent-empty-fork bug.
- **`clear`** — delete the log row; the cascade removes events and snapshot.

`log_id` is resolved per call through the `(kind, id)` unique index; sqlx's statement cache keeps every one of these prepared.

### WAL is not optional

`open_pool` (`server/src/config/store.rs`) currently sets only `create_if_missing` and `busy_timeout`, leaving the database in `journal_mode = DELETE`, where every write takes an exclusive lock over the whole file. Adding journal traffic to the settings database in that mode would serialize journal writes against every authenticated request's token write.

So this work sets `journal_mode = WAL` and `synchronous = FULL`. FULL preserves the durability `FileJournal`'s per-batch `sync_all` provides today, which `CommandEffect::PersistAndAck` promises its callers; WAL is what keeps readers from blocking behind it.

### The sequence-number contract

`Journal::replay` changes to yield the sequence number alongside the payload:

```rust
async fn replay(&self, pid: &PersistenceId, after_seq: u64)
    -> BoxStream<'_, JournalResult<(u64, Vec<u8>)>>;
```

`recover()` currently derives `seq_nr` by counting (`seq_nr += 1` per replayed event). That is only correct while numbering is contiguous from the snapshot, and it is the number a later snapshot is recorded at — so a drift would record a snapshot at the wrong sequence and make the next `replay(after_seq)` skip or duplicate events. Taking the number from the journal removes the invariant instead of relying on it.

`FileJournal` keeps its positional count, now reported explicitly rather than reconstructed by its callers.

### Snapshot policy

After #149, `AgentActor` snapshots at ask, park, and cancel — pause points, not turn boundaries. A session that simply converses never pauses that way, so it would never snapshot and every recovery would stay a full replay.

So a completed turn snapshots too, throttled: the actor counts events since its last snapshot request in `on_events_persisted` and asks for one at the next turn boundary once the count passes `SNAPSHOT_EVERY_EVENTS` (200), resetting the counter when it asks. Throttling matters because an agent's state is its whole transcript — snapshotting every turn is O(transcript) per turn, which is quadratic over a session.

The counter resets on request rather than on confirmed success, so a failed snapshot simply waits another interval. That is the right trade: a snapshot is an optimization, and retrying it aggressively on a failing journal would be the wrong instinct.

### State is now a serialization contract

Nothing in production has ever written a snapshot, so no `A::State` has ever been serialized and read back. The moment this lands, every snapshotted state is a compatibility surface: `recover()` deserializes it before replaying, so a renamed or newly-required field breaks recovery for every existing session — exactly how renamed event variants killed the supervisor on 2026-08-02.

Every field of every snapshotted state (`AgentState`, `WorkflowState`, `SessionState`, `SessionSupervisorState`) therefore carries `#[serde(default)]`. `AgentState.messages` is missing it today and gets it here.

This is mitigation, not immunity — a field whose *meaning* changes still breaks. The durable rule is that state structs are append-only: add optional fields, never rename or repurpose.

## Testing

- **Conformance:** the existing suite in `actor/tests/journal_conformance.rs` gains a `SqliteJournal` backend module. All ten assertions must pass, including the five currently red for `FileJournal` — that is the headline result.
- **Numbering across compaction:** persist, snapshot, compact, persist again; assert the survivors keep their original numbers and new events continue from `last_seq`, not from `MAX(seq)` over what remains.
- **Read amplification:** a decorator counting rows scanned, asserting `replay(after_seq)` on a long log yields only the tail. The behavioural proxy is that a cursored replay returns the right events; the point is that it no longer costs the whole log.
- **Recovery from snapshot:** drive an agent, snapshot, compact, respawn on the same journal, assert the recovered transcript is identical and the replay covered only post-snapshot events.
- **Fork:** `copy_snapshot` seeds a destination that recovers the source's state — the bug that previously produced a silent empty agent.
- **Server end-to-end:** a session server on a SQLite journal completes turns, survives restart with its transcript intact, and reports the same history as before the restart.

## Consequences

- Compaction becomes real for interactive sessions. That is safe only because #149 removed every journal reader except `recover()`; before it, `compact_on_pause()` existed precisely to prevent this.
- The settings database now carries session write volume. WAL plus `synchronous = FULL` is the mitigation; if contention ever shows up, splitting the journal into its own file is a configuration change, not a redesign.
- `FileJournal` keeps its five red conformance tests. It stays for the CLI/daemon path, where short-lived runs make full replay acceptable.
