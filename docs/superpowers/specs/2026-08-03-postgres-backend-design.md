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
server/migrations/sqlite/0001_init.sql   … 0015_agents.sql   (moved verbatim)
server/migrations/postgres/0001_init.sql … 0015_agents.sql   (new, equivalent)
server/migrations/{sqlite,postgres}/0016_journal.sql          (new, both)
```

Moving a file does not change its content, so SQLite checksums survive the
reorganisation untouched.

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

### Journal schema (`0016_journal.sql`)

```sql
CREATE TABLE journal_events (
    actor_kind TEXT   NOT NULL,
    actor_id   TEXT   NOT NULL,
    seq        BIGINT NOT NULL,
    payload    BLOB   NOT NULL,          -- BYTEA on Postgres
    PRIMARY KEY (actor_kind, actor_id, seq)
);

CREATE TABLE journal_snapshots (
    actor_kind TEXT   NOT NULL,
    actor_id   TEXT   NOT NULL,
    seq        BIGINT NOT NULL,
    state      BLOB   NOT NULL,          -- BYTEA on Postgres
    PRIMARY KEY (actor_kind, actor_id)
);
```

The composite primary key is the only index either table needs. Every access
path is `(kind, id)` equality plus a `seq` range or ordering, which the primary
key index serves directly — a left-prefix scan, no sort, no extra structure to
maintain on the write path. Postgres stores the payload out-of-line in TOAST
once it exceeds ~2 KB, which is the desired behaviour for large events: the
index stays dense and only the rows actually replayed pay for decompression.

### `SqlJournal`

Sequence numbers are assigned by the journal, not the caller. Rather than a
`MAX(seq)` round-trip per write, `SqlJournal` keeps a head cache —
`Mutex<HashMap<PersistenceId, u64>>` — seeded lazily on first touch of a
persistence id:

```sql
SELECT GREATEST(
    COALESCE((SELECT MAX(seq) FROM journal_events    WHERE …), 0),
    COALESCE((SELECT seq      FROM journal_snapshots WHERE …), 0))
```

Both terms are required: after compaction the event table can be empty while
the snapshot sits at seq 42, and seeding from events alone would restart
numbering and corrupt the log. (`GREATEST` is spelled `MAX` in SQLite; this is
one of the few places the two dialects genuinely differ, so it is computed in
Rust from two scalar reads instead — one code path, no dialect branch.)

The cache is an optimisation over a value the database already holds, and it is
only correct because one process owns each persistence id. The composite
primary key makes a violation of that assumption a loud constraint error rather
than silent interleaving.

| Method | Implementation |
| --- | --- |
| `persist` | One transaction; a single multi-row `INSERT` (chunked at 8 000 rows to stay under Postgres's 65 535 bind-parameter limit — real batches are single digits). The head cache advances **only after commit**. |
| `replay` | Keyset pagination via `futures_util::stream::unfold`: `WHERE kind=? AND id=? AND seq > ? ORDER BY seq LIMIT 1000`, the last `seq` of each page seeding the next. Bounded memory on a 100 000-event log, no borrow of the query string into the stream, and no new dependency. |
| `save_snapshot` | Upsert on `(actor_kind, actor_id)`; head cache raised to `max(cache, seq)`, mirroring `InMemoryJournal`. |
| `delete_events_before` | `DELETE … WHERE seq <= ?`. |
| `copy_snapshot` | One transaction: read source snapshot (absent → `JournalError::Backend`), delete the target's events, upsert the target's snapshot. Head cache for the target set to the copied seq, so it continues numbering from there. |
| `clear` | Delete from both tables; drop the cache entry. |

**Write path cost.** One round-trip per `persist` — the same count as
`FileJournal`'s one `write` + `fsync`, but a network hop instead of a local
one. Against that, snapshots now actually work: `FileJournal::save_snapshot` is
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
- `journal.backend` — `"file"` | `"database"`. Absent → `"database"` when the
  resolved URL is Postgres, `"file"` otherwise.

The default is asymmetric on purpose. Existing SQLite deployments must keep
their file journals or they would silently lose every session on upgrade. A
Postgres deployment, by contrast, has no existing journal to preserve and no
durable volume to assume, so the file default would be the wrong answer every
time. The resolved choice is logged at startup so it is never a guess.

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
  it; the head cache is not advanced, so a retry re-uses the same sequence
  numbers.
- **Pool exhaustion** surfaces as a sqlx timeout and is reported like any other
  store error. Journal writes and settings reads share the pool, so
  `max_connections` is configurable; the default of 10 is sized for a single
  server process.
- **Constraint violations on `journal_events`** mean two writers touched one
  persistence id — the invariant this design rests on. It is surfaced as a
  backend error rather than swallowed.

## Testing

- **Placeholder rewriting** — unit tests over `Db::q`: no placeholders, several,
  `?` inside a single-quoted literal, `''` escapes, and the real
  `LIKE ? ESCAPE '\'` string from `model_cards`.
- **Migration parity** — a test asserting both directories declare the same
  versions and descriptions.
- **Journal conformance** — the contract assertions currently inline in
  `actor/tests/journal_conformance.rs` move into `horsie_actor::testkit` as a
  reusable module, so `InMemoryJournal`, `FileJournal`, and `SqlJournal` (on
  both backends) are all held to one spec. This is the highest-value test in
  the change: the trait's doc comments are the real specification, and the SQL
  implementation is the first one where snapshots and compaction do anything.
- **Store tests on both backends** — the ~15 inline `SqliteConnectOptions` test
  helpers collapse into one `test_db()` helper. It always yields a SQLite pool
  and additionally yields a Postgres one when `HORSIE_TEST_POSTGRES_URL` is
  set, each test running against every available backend.
- **CI** — a `postgres:17` service is added to the `check` job, with
  `HORSIE_TEST_POSTGRES_URL` set. `HORSIE_REQUIRE_POSTGRES_TESTS=1` is also set
  there, so a missing URL fails the run instead of quietly skipping the half of
  the suite that is the entire point of this change.

## Consequences

- Every store gains a `Db` instead of a `SqlitePool`; the query text is
  otherwise unchanged apart from the two `RETURNING id` conversions, the one
  `INSERT OR IGNORE` → `ON CONFLICT DO NOTHING`, and reading booleans as `i64`.
- Adding a migration now means writing two files. The parity test makes
  forgetting one a CI failure rather than a production one.
- A Postgres deployment needs no persistent volume for sessions or settings —
  only for plugin artifacts, which remain on disk under the data dir.
