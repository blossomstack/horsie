# PostgreSQL backend Implementation Plan

**Goal:** Run `horsie-server`'s settings store *and* actor journal on either SQLite or PostgreSQL, selected by configuration, with migrations that apply to both.

**Architecture:** One `Db` wrapper over `sqlx::Any` (pool + dialect + placeholder rewriting) shared by the 8 existing stores and a new `SqlJournal`. Per-dialect migration directories embedded at compile time, chosen at runtime.

Design spec: `docs/superpowers/specs/2026-08-03-postgres-backend-design.md`

## Global Constraints

- `clippy.toml` disallows `Journal::replay` outside the actor crate — the journal conformance tests need `#[expect(clippy::disallowed_methods, reason = "…")]`, as the existing suite already does.
- Production code bans panic-prone constructs; test modules opt out per-file.
- `sqlx::Any` constraints (verified against sqlx 0.8.6, see spec): no placeholder rewriting, `last_insert_id` is `None` on SQLite, SQLite never yields `Bool`, unknown SQLite URL params are a connect error.
- Never change the content or version of an existing migration file — moving it between directories is fine, editing it breaks applied checksums.
- Verify with `make check` (fmt + clippy + `cargo test --workspace`). Postgres tests need `HORSIE_TEST_POSTGRES_URL`.

---

### Task 1: `Db` — pool, dialect, placeholder rewriting

**Files:** create `server/src/db/mod.rs`; modify `server/Cargo.toml`, workspace `Cargo.toml` (sqlx features `any`, `postgres`).

- [ ] Unit tests for `Db::q` first: no placeholders; three placeholders; `?` inside `'…'`; `''` escape; the real `LIKE ? ESCAPE '\'` from `model_cards`; identity on SQLite.
- [ ] `Dialect`, `Db`, `open(url, max_connections)`, `pool()`, `dialect()`, `q()`.
- [ ] `open`: `install_default_drivers()`, `?mode=rwc` for SQLite file URLs, `after_connect` PRAGMAs (`journal_mode=WAL`, `busy_timeout=5000`) on SQLite only.

### Task 2: Split migrations per dialect

**Files:** `git mv server/migrations/*.sql server/migrations/sqlite/`; create `server/migrations/postgres/0001…0015`; modify `server/src/db/mod.rs`.

- [ ] Move the 15 files verbatim — no content edits.
- [ ] Write the Postgres equivalents using the translation table in the spec.
- [ ] `Db::open` runs `migrate!("migrations/sqlite")` or `migrate!("migrations/postgres")` by dialect.
- [ ] Parity test: both directories declare the same versions and descriptions.

### Task 3: Port the stores to `Db`

**Files:** `server/src/{config/store.rs,config/model_cards.rs,auth/store.rs,mcp/store.rs,memory/store.rs,plugins/store.rs,github/store.rs,agents/store.rs}`.

- [ ] `SqlitePool` → `Db`, `SqliteRow` → `AnyRow`, each query wrapped in `db.q(…)`.
- [ ] `last_insert_rowid()` → `RETURNING id` (`memory/store.rs`, `auth/store.rs`).
- [ ] `INSERT OR IGNORE` → `ON CONFLICT DO NOTHING` (`model_cards.rs`).
- [ ] Booleans read as `i64` and converted, never `try_get::<bool, _>`.
- [ ] Collapse the ~15 inline test pool helpers into one `test_db()` that yields SQLite always and Postgres when `HORSIE_TEST_POSTGRES_URL` is set; fail if `HORSIE_REQUIRE_POSTGRES_TESTS=1` and the URL is missing.

### Task 4: Reusable journal conformance suite

**Files:** none — #151 already moved the contract into `horsie_actor::testkit::conformance`.

- [ ] Move the contract assertions into `horsie_actor::testkit` as a public module taking `&dyn Journal`.
- [ ] Existing `InMemoryJournal`/`FileJournal` tests call it; the red-catalogue `#[ignore]` markers stay exactly as they are.

### Task 5: `SqlJournal`

**Files:** create `server/src/db/journal.rs`, `server/migrations/{sqlite,postgres}/0017_journal.sql`; test `server/tests/` (or the crate's own `mod tests`).

- [ ] Schema migration for both dialects.
- [ ] Sequence numbers allocated from `journal_logs.last_seq` (the schema #151 shipped), not from a cached or derived `MAX(seq)`.
- [ ] `persist` (one transaction, numbers allocated by `UPDATE … RETURNING`), `replay` (keyset pagination via `unfold`, 1 000/page), `save_snapshot`, `delete_events_before`, `copy_snapshot`, `clear`.
- [ ] Run the Task 4 conformance suite against `SqlJournal` on both backends.

### Task 6: Configuration and wiring

**Files:** `server/src/bin/horsie-server/{config.rs,main.rs}`, `server/src/config/store.rs` (`DbConfigStore::open`), `models/` (`ServerInfo`).

- [ ] `database.max_connections`; `journal.backend` = `file` | `database`, defaulting to `database` on either dialect.
- [ ] Select `FileJournal` or `SqlJournal` at boot; log the resolved backend.
- [ ] `ServerInfo` reports the resolved journal backend.

### Task 7: CI, docs, supply chain

**Files:** `.github/workflows/ci.yml`, `deny.toml` if needed, `docs/guide/*`, ops `RUNBOOK.md`.

- [ ] `postgres:17` service on the `check` job, with `HORSIE_TEST_POSTGRES_URL` and `HORSIE_REQUIRE_POSTGRES_TESTS=1`.
- [ ] `cargo deny` for the new Postgres dependency tree.
- [ ] Document the Postgres URL, `journal.backend`, and the one-way-door cutover.
