# PostgreSQL backend: settings store and actor journal

**Status:** design
**Date:** 2026-08-03

## Problem

`horsie-server` keeps its durable state in two unrelated places, both tied to
the local filesystem:

- **Backend data** — providers, models, vendors, settings, GitHub, MCP,
  plugins, memory, auth, agents, model cards — in a SQLite file
  (`data_dir/server/config.db`) reached through `sqlx` with concrete
  `SqlitePool`/`SqliteRow` types.
- **The actor journal** — every session's and agent's event log — as base64
  JSONL files under `data_dir/server/actors/<kind>/<id>/journal.jsonl`
  (`FileJournal`).

That shape only works when the process owns a durable disk. For managed
hosting, the whole point is that it does not: the container is disposable and
the database is the thing that gets backed up, replicated, and
point-in-time-restored. Today, pointing horsie at a managed Postgres is
impossible for the settings and gets you nothing for the journal.

This design makes both work on either SQLite or PostgreSQL, selected by
configuration.

## Scope

**In scope**

1. A `Db` abstraction over `sqlx::Any` so the existing stores run unchanged on
   either backend.
2. Migrations that apply to both, without breaking the checksums of the 15
   migrations already applied in the wild.
3. A `SqlJournal` implementing `horsie_actor::Journal` on the same pool —
   including snapshots and compaction, which `FileJournal` silently no-ops.
4. Configuration: `database.url` selects the backend; `journal.backend`
   selects file-or-database.
5. CI coverage: the full server test suite runs against both backends.

**Out of scope, deliberately**

- **Multi-instance horsie-server.** One process owns the database. No actor
  ownership leases, no fencing tokens, no cross-node SSE fan-out. Journal
  writes are single-writer by construction; the primary key is a correctness
  backstop, not a coordination mechanism.
- **Importing existing file journals.** Switching `journal.backend` to
  `database` starts from an empty log. This is a documented one-way door
  (see [Cutover](#cutover)).
- **Changing when actors request snapshots.** Since #149 the agent actor
  snapshots unconditionally; nothing here needs to change.
- **Postgres-specific features** — `LISTEN`/`NOTIFY`, arrays, `jsonb`. Every
  query stays in the portable subset.

## Why `sqlx::Any`

The stores use runtime `sqlx::query` throughout — **zero** compile-time
`query!` macros across 87 call sites — so there is no offline-metadata problem
and no per-backend codegen. Every value horsie stores is `TEXT`, `INTEGER`, or
`BLOB`, which is exactly the set `AnyValueKind` covers (`Bool`, `SmallInt`,
`Integer`, `BigInt`, `Real`, `Double`, `Text`, `Blob`).

The alternative — a hand-rolled `enum Db { Sqlite(SqlitePool), Postgres(PgPool) }` —
forces an abstraction over `SqliteRow` vs `PgRow` as well, which is
re-implementing `Any` with more code and less testing. Rejected.

`Any` costs us driver-specific error codes (unique-violation detection becomes
string matching, which no current store does) and driver-specific types
(nothing here uses any). Both are acceptable.

### Four constraints the Any driver imposes

These were verified against sqlx 0.8.6 sources, not assumed. Each one is a
place where a natural implementation would compile and then fail at runtime.

1. **`Any` does not rewrite placeholders.** SQL text passes through to the
   driver verbatim, so `?` reaches Postgres and fails. We rewrite `?` → `$n`
   ourselves (see `Db::q`).
2. **`last_insert_id` is `None` on SQLite through `Any`**
   (`sqlx-sqlite/src/any.rs`: `map_result` hardcodes it). The two call sites
   using `last_insert_rowid()` (`memory/store.rs`, `auth/store.rs`) must switch
   to `RETURNING id`, which both backends support.
3. **SQLite never produces `AnyValueKind::Bool`.** Values are mapped by
   runtime type, and SQLite has no boolean type, so `try_get::<bool, _>` is a
   runtime error there while working fine on Postgres. Booleans are stored as
   `INTEGER` 0/1 in both dialects and read as `i64`. Integer decoding itself is
   safe: `try_integer` converts across `SmallInt`/`Integer`/`BigInt`, so
   Postgres `INTEGER` reads into `i64` correctly.
4. **Unknown SQLite URL parameters are a hard connect error** — the parser
   accepts only `mode`, `cache`, `immutable`, `vfs`. So `?journal_mode=WAL`
   does not work, and `AnyConnectOptions` cannot carry
   `SqliteConnectOptions::busy_timeout`. Both move into an `after_connect` hook
   that issues the PRAGMAs; `create_if_missing` becomes `?mode=rwc` on the URL.

## Architecture

```
                    config.json
              database.url ──────────────┐
              journal.backend ───────┐   │
                                     │   │
                                     ▼   ▼
  ┌──────────────────────────────────────────────────┐
  │ server/src/db/mod.rs                             │
  │   Db { pool: AnyPool, dialect: Dialect }         │
  │   · open(url)   install drivers, PRAGMAs, migrate│
  │   · q(sql)      "?" → "$n" on Postgres           │
  └───────────┬──────────────────────────┬───────────┘
              │ same pool                │
   ┌──────────▼──────────┐    ┌──────────▼─────────────┐
   │ 7 stores + model    │    │ server/src/db/journal.rs│
   │ cards (unchanged    │    │   SqlJournal            │
   │ logic, Db-typed)    │    │   impl horsie_actor::   │
   └─────────────────────┘    │        Journal          │
                              └─────────────────────────┘
                                         ▲
                              file │ database
                                   └── FileJournal (unchanged)
```

`horsie-actor` gains no dependency on sqlx: `SqlJournal` lives in the server
crate, which is the only crate that has a pool. The CLI and supervisor keep
`FileJournal`.

### `Db`

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dialect { Sqlite, Postgres }

#[derive(Clone)]
pub struct Db { pool: AnyPool, dialect: Dialect }

impl Db {
    pub async fn open(url: &str, max_connections: u32) -> Result<Db, String>;
    pub fn pool(&self) -> &AnyPool;
    pub fn dialect(&self) -> Dialect;
    /// Rewrite `?` placeholders to `$1..$n` on Postgres; identity on SQLite.
    pub fn q<'a>(&self, sql: &'a str) -> Cow<'a, str>;
}
```

Call sites become two lines instead of one:

```rust
let sql = self.db.q("SELECT id FROM auth_users WHERE username = ?");
let row = sqlx::query(&sql).bind(username).fetch_optional(self.db.pool()).await?;
```

`q` is a small scanner, not a regex: it walks the string tracking whether it is
inside a single-quoted literal (with `''` escaping) and numbers only the `?`
outside one. `model_cards` has `LIKE ? ESCAPE '\'`, so literal-awareness is
load-bearing, and it gets its own unit tests.

`open` performs, in order: `sqlx::any::install_default_drivers()` (idempotent,
process-global), URL normalization (append `?mode=rwc` for SQLite file URLs),
pool construction with an `after_connect` hook that runs
`PRAGMA journal_mode = WAL` and `PRAGMA busy_timeout = 5000` on SQLite only,
then migration.

`PRAGMA foreign_keys` stays **off**, as today. Two migrations
(`0009_memory.sql`, `0014_auth.sql`) explicitly document that they omit
`REFERENCES` because a constraint that is silently ignored is worse than none,
and `MemoryStore` enforces the relationships in explicit transactions. Turning
foreign keys on for Postgres only would mean the two backends enforce different
invariants — a worse bug than the one it fixes. Left as-is.

### Migrations

`sqlx::migrate!()` embeds a directory at compile time and records a checksum of
each file's contents. The 15 existing files are already applied in the wild, so
their **content and version numbers must not change**.

Two embedded directories, selected at runtime:

```
server/migrations/sqlite/0001_init.sql   … 0017_journal.sql  (moved verbatim)
server/migrations/postgres/0001_init.sql … 0017_journal.sql  (new, equivalent)
```

Moving a file does not change its content, so SQLite checksums survive the
reorganisation untouched — including `0016_routines.sql` and `0017_journal.sql`,
which landed on `main` while this work was in flight and are moved, not
rewritten.

Postgres files mirror the SQLite ones 1:1 by version, translating:

| SQLite | Postgres |
| --- | --- |
| `INTEGER PRIMARY KEY AUTOINCREMENT` | `BIGSERIAL PRIMARY KEY` |
| `BLOB` | `BYTEA` |
| `strftime('%s','now')` | `EXTRACT(EPOCH FROM now())::bigint::text` |
| `datetime('now')` | `to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS')` |
| `INTEGER` (boolean 0/1) | `INTEGER` — unchanged, deliberately not `BOOLEAN` |

`0012` and `0013` are data corrections against seeded rows. On a fresh Postgres
database their `UPDATE`/`DELETE` statements match nothing; they are kept as
real files at the same versions so the two directories stay aligned
version-for-version.

A unit test asserts **parity**: the two directories contain the same set of
version numbers with the same descriptions. Adding a migration to one and
forgetting the other fails CI rather than a deployment.

### Journal schema (`0017_journal.sql`)

Unchanged from the SQLite journal that shipped in #151 — this work translates it
to PostgreSQL rather than redesigning it:

```sql
CREATE TABLE journal_logs (
    log_id   INTEGER PRIMARY KEY,        -- BIGSERIAL on Postgres
    kind     TEXT    NOT NULL,
    id       TEXT    NOT NULL,
    last_seq INTEGER NOT NULL DEFAULT 0,
    UNIQUE (kind, id)
);

CREATE TABLE journal_events (
    log_id  INTEGER NOT NULL REFERENCES journal_logs(log_id) ON DELETE CASCADE,
    seq     INTEGER NOT NULL,
    payload BLOB    NOT NULL,            -- BYTEA on Postgres
    PRIMARY KEY (log_id, seq)
) WITHOUT ROWID;                         -- SQLite only; no Postgres equivalent

CREATE TABLE journal_snapshots (
    log_id INTEGER PRIMARY KEY REFERENCES journal_logs(log_id) ON DELETE CASCADE,
    seq    INTEGER NOT NULL,
    state  BLOB    NOT NULL              -- BYTEA on Postgres
);
```

The primary key is the only index either table needs. Every access path is
`log_id` equality plus a `seq` range or ordering, which the primary key index
serves directly — a left-prefix scan, no sort, no extra structure to maintain on
the write path. `WITHOUT ROWID` makes that index *be* the storage order on
SQLite; PostgreSQL has no clustered-index equivalent to declare, so it pays a
heap fetch per row. Postgres stores the payload out-of-line in TOAST once it
exceeds ~2 KB, which is the desired behaviour for large events: the index stays
dense and only the rows actually replayed pay for decompression.

### `SqlJournal`

Sequence numbers are assigned by the journal, not the caller, and the allocator
is `journal_logs.last_seq` rather than a `MAX(seq)` over the events. A whole
batch takes its numbers in one statement:

```sql
UPDATE journal_logs SET last_seq = last_seq + ? WHERE log_id = ? RETURNING last_seq
```

Storing the allocator instead of deriving it is what makes compaction safe:
`delete_events_before` leaves `last_seq` alone, so the survivors keep their
numbers and the next event continues from where the log actually is. A derived
head would restart numbering the moment the event table went empty behind a
snapshot. It also means the number is correct without this type having to assume
it is the only writer — no in-process cache to go stale.

`save_snapshot` raises `last_seq` to the snapshot's sequence if the snapshot came
from elsewhere (a fork), so later events never reuse a covered number. That is
the one statement where the dialects differ — SQLite spells the two-argument
maximum `MAX(a, b)` and PostgreSQL spells it `GREATEST(a, b)` — and it goes
through `Db::greatest`.

| Method | Implementation |
| --- | --- |
| `persist` | One transaction (`BEGIN IMMEDIATE` on SQLite, via `Db::begin_write`, so two writers queue instead of deadlocking on a lock upgrade): upsert the log row, allocate the batch's numbers in one `UPDATE … RETURNING`, insert the events. |
| `replay` | Keyset pagination via `futures_util::stream::unfold`: `WHERE log_id = ? AND seq > ? ORDER BY seq LIMIT 1000`, the last `seq` of each page seeding the next. Bounded memory on a 100 000-event log, no borrow of the query string into the stream, and no new dependency. |
| `save_snapshot` | Upsert on `log_id`, with `last_seq` raised to the snapshot's sequence. |
| `delete_events_before` | `DELETE … WHERE log_id = ? AND seq <= ?`, leaving `last_seq` alone. |
| `copy_snapshot` | One transaction: read source snapshot (absent → `JournalError::Backend`), delete the target's events, set the target's `last_seq` to the copied seq, upsert the target's snapshot. |
| `clear` | Delete the log row; the foreign keys cascade to events and snapshot. |

**Write path cost.** One transaction per `persist`, against `FileJournal`'s one
`write` + `fsync` — and a network hop instead of a local one on PostgreSQL.
Against that, snapshots now actually work: `FileJournal::save_snapshot` is
a no-op, so every recovery replays the entire log from seq 0, and since #101
the server offloads idle actors and re-recovers them on wake. On the SQL
journal a wake reads one snapshot row plus the events after it.

## Configuration

```json
{
  "database": { "url": "postgres://user:pw@host/horsie", "max_connections": 10 },
  "journal":  { "backend": "database" }
}
```

- `database.url` — absent → SQLite file under the data dir, exactly as today.
  `max_connections` defaults to 10.
- `journal.backend` — `"file"` | `"database"`. Absent → `"database"` on either
  dialect.

This was originally specified as an asymmetric default — `"database"` for a
Postgres URL, `"file"` for SQLite — so that an existing SQLite deployment could
not lose its on-disk sessions on upgrade. #151 then shipped the SQLite journal
with `"database"` as its default, which makes the asymmetric version the more
destructive of the two: it would abandon the database journal of every
deployment that has upgraded since. So the default is now uniform, and `"file"`
is an explicit opt-out (and still what the CLI uses). The resolved choice is
logged at startup so it is never a guess.

`ServerInfo` gains the resolved journal backend alongside the already-redacted
database URL, so `/api/config` shows what the server actually did.

## Cutover

Switching an existing deployment's `journal.backend` from `file` to `database`
abandons the on-disk history: sessions already in the UI disappear, and the
JSONL tree stays on the volume as a manual archive. Nothing is deleted, and
nothing is imported.

This is documented as a one-way door in the runbook and the self-hosting guide,
and the startup log states which backend was selected. There is precedent —
recovering the #101 outage meant wiping sessions — and the alternative (an
import path that must faithfully reproduce sequence numbering across two
storage layouts) is a larger correctness surface than the feature itself.

## Error handling

- **Connect and migrate failures are fatal at startup.** They already are for
  SQLite; a bad Postgres URL or an unreachable host should fail loudly at boot
  rather than on first request.
- **Journal write failures propagate as `JournalError::Backend`.** The actor
  runtime already treats a failed `persist` as "state left unchanged" and logs
  it; the whole batch is one transaction, so a failure rolls back the `last_seq`
  allocation with the rows and a retry re-uses the same sequence numbers.
- **Pool exhaustion** surfaces as a sqlx timeout and is reported like any other
  store error. Journal writes and settings reads share the pool, so
  `max_connections` is configurable; the default of 10 is sized for a single
  server process.
- **Constraint violations on `journal_events`** mean two writers allocated the
  same sequence number. Allocation happens inside the write transaction, so this
  should not be reachable; it is surfaced as a backend error rather than
  swallowed.

## Testing

- **Placeholder rewriting** — unit tests over `Db::q`: no placeholders, several,
  `?` inside a single-quoted literal, `''` escapes, and the real
  `LIKE ? ESCAPE '\'` string from `model_cards`.
- **Migration parity** — a test asserting both directories declare the same
  versions and descriptions.
- **Journal conformance** — the contract assertions in
  `horsie_actor::testkit::conformance` (moved there by #151) hold
  `InMemoryJournal`, `FileJournal` and `SqlJournal` (on both backends) to one
  spec. This is the highest-value test in
  the change: the trait's doc comments are the real specification, and the SQL
  implementation is the first one where snapshots and compaction do anything.
- **The whole suite on both backends** — the ~15 inline `SqliteConnectOptions`
  helpers collapse into one `db::testing::db()`, which picks its backend from
  `HORSIE_TEST_POSTGRES_URL`: unset means SQLite, set means a freshly created
  PostgreSQL database (one per test, since store tests assert on whole-table
  contents).

  Selecting per *run* rather than looping per *test* is what makes this
  affordable: every test that touches storage becomes a portability test
  without being rewritten, so a query that breaks on PostgreSQL fails in
  whichever test already covers that code path. `DbConfigStore` grows an
  `open_on(db, deps)` seam so its tests go through the same selection instead
  of a hardcoded URL.
- **CI** — a second job runs `cargo test --workspace --all-features` against a
  `postgres:17` service with `HORSIE_TEST_POSTGRES_URL` set. A whole job, not a
  conditional inside one, so the PostgreSQL run cannot quietly skip: if the
  service fails to come up, the job goes red.

## Consequences

- Every store gains a `Db` instead of a `SqlitePool`; the query text is
  otherwise unchanged apart from the two `RETURNING id` conversions, the one
  `INSERT OR IGNORE` → `ON CONFLICT DO NOTHING`, and reading booleans as `i64`.
- Adding a migration now means writing two files. The parity test makes
  forgetting one a CI failure rather than a production one.
- A Postgres deployment needs no persistent volume for sessions or settings —
  only for plugin artifacts, which remain on disk under the data dir.
