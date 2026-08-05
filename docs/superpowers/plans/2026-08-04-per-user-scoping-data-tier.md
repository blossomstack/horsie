# Per-user scoping, data tier — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make every durable row in horsie server belong to a user, so that a later change can serve several users from one deployment.

**Architecture:** A `UserId` (a short random string) becomes the scope key on sixteen tables. Each store binds its scope once, at construction, rather than taking it per call. Two mechanisms prove the result: a CI check that fails any SQL literal touching a scoped table without naming `user_id`, and a harness that writes as two users and asserts neither can see the other. The server still runs with exactly one account when this lands, and behaves identically.

**Tech Stack:** Rust 2024, sqlx 0.9 over `sqlx::Any` (SQLite and PostgreSQL), axum 0.8, tokio.

Source spec: `docs/superpowers/specs/2026-08-04-per-user-scoping-design.md`. This plan covers spec items 1–3 and 8 (the data tier). The runtime tier — per-user service bundle, supervisor, vendor map, event channel — is a separate plan and is **out of scope here**.

## Global Constraints

- **Both dialects, always.** Every migration exists in `server/migrations/sqlite/` *and* `server/migrations/postgres/`, with identical version numbers and identical descriptions, or the `migrations_are_in_parity` test in `server/src/db/mod.rs` fails.
- **Postgres files carry a header line:** `-- PostgreSQL mirror of migrations/sqlite/<file>.` — follow `0006_drop_api_key_env.sql`.
- **No foreign keys.** `PRAGMA foreign_keys` is never enabled on the pool, so a declared constraint is silently ignored on SQLite. Do not add `REFERENCES` clauses.
- **All SQL is a literal in this repo, written in SQLite placeholder style (`?`), and passed through `db.q(...)`.** Never interpolate a caller-supplied value. No `sqlx::query!` macros — there is no offline metadata.
- **Booleans are `INTEGER` 0/1 in both dialects** and are read with `crate::db::get_bool`, never `try_get::<bool, _>`.
- **Inserts that need the assigned id use `RETURNING id`** — `last_insert_id` is always `None` for SQLite through `Any`.
- **Iterate with `cargo test -p horsie-server --lib <filter>`.** Run `make check` (fmt-check + clippy `-D warnings` + `cargo test --workspace`) once before opening the PR, not on every step.
- **Never `-c user.name` / `-c user.email` on a commit.**

---

### Task 1: `UserId` and random id generation

**Files:**
- Create: `server/src/auth/user.rs`
- Modify: `server/src/auth/mod.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `crate::auth::UserId` — `UserId::generate() -> UserId`, `UserId::new(impl Into<String>) -> UserId`, `UserId::as_str(&self) -> &str`. Derives `Clone, PartialEq, Eq, Hash, Debug`. Every later task binds one of these into SQL with `.bind(user.as_str())`.

- [ ] **Step 1: Write the failing test**

Create `server/src/auth/user.rs` with only the test module for now:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn a_generated_id_is_twelve_crockford_base32_characters() {
        let id = UserId::generate();
        assert_eq!(id.as_str().len(), 12, "{}", id.as_str());
        for c in id.as_str().chars() {
            assert!(
                ALPHABET.contains(&(c as u8)),
                "{c:?} is not in the alphabet, in {}",
                id.as_str()
            );
        }
    }

    /// The four letters Crockford drops are the ones that are misread when
    /// somebody copies an id out of a log by hand.
    #[test]
    fn the_alphabet_excludes_the_ambiguous_letters() {
        for c in [b'i', b'l', b'o', b'u'] {
            assert!(!ALPHABET.contains(&c), "{} should be excluded", c as char);
        }
        assert_eq!(ALPHABET.len(), 32);
    }

    /// Not a proof of randomness — a guard against a generator that returns a
    /// constant, which is the way this fails in practice.
    #[test]
    fn generated_ids_differ() {
        let ids: HashSet<String> = (0..1000)
            .map(|_| UserId::generate().as_str().to_string())
            .collect();
        assert_eq!(ids.len(), 1000);
    }

    #[test]
    fn an_id_round_trips_through_new_and_as_str() {
        assert_eq!(UserId::new("abc123").as_str(), "abc123");
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p horsie-server --lib auth::user`
Expected: FAIL to compile — `cannot find type UserId in this scope`.

- [ ] **Step 3: Write the implementation**

Put this above the test module in `server/src/auth/user.rs`:

```rust
//! The scope key: which account a durable row belongs to.
//!
//! A random string rather than an autoincrementing integer, because a
//! sequential key published as a scope leaks how many accounts a deployment has
//! and makes the set enumerable.

use rand::Rng;

/// Crockford base32, lowercase: the ten digits and the twenty-six letters less
/// `i`, `l`, `o` and `u`.
///
/// Case-*insensitive* on purpose. A user id becomes a directory name under
/// `<state_dir>/server/users/<id>/`, and macOS APFS is case-insensitive by
/// default — so a case-sensitive alphabet could collide two distinct ids on one
/// filesystem.
const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// 12 characters at 5 bits each: 60 bits, so a collision becomes likely
/// somewhere past a billion accounts.
const LEN: usize = 12;

/// The owner of every scoped row.
///
/// Not a secret and not a credential — a caller still presents a token, which
/// is what is actually verified. Unguessability here is defence in depth.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct UserId(String);

impl UserId {
    /// A fresh random id.
    #[must_use]
    pub fn generate() -> UserId {
        let mut bytes = [0u8; LEN];
        // The same generator `auth::token::generate` uses, for the same reason:
        // rand 0.10 removed `rngs::OsRng` and its replacement is fallible,
        // whereas `rand::rng()` is an infallible `CryptoRng` seeded from the OS.
        rand::rng().fill_bytes(&mut bytes);
        // Masking to 5 bits indexes the alphabet directly. The bias a modulo
        // would introduce is avoided because 32 divides 256 exactly.
        let s = bytes
            .iter()
            .map(|b| ALPHABET[(b & 0x1f) as usize] as char)
            .collect();
        UserId(s)
    }

    /// Wrap an id read from storage or a request.
    pub fn new(s: impl Into<String>) -> UserId {
        UserId(s.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for UserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
```

Add to `server/src/auth/mod.rs`, next to the existing `mod`/`pub use` lines:

```rust
mod user;
pub use user::UserId;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p horsie-server --lib auth::user`
Expected: PASS, 4 tests.

- [ ] **Step 5: Commit**

```bash
git add server/src/auth/user.rs server/src/auth/mod.rs
git commit -m "auth: UserId, a short random scope key"
```

---

### Task 2: Retype `auth_users.id` to TEXT

**Files:**
- Create: `server/migrations/sqlite/0023_user_id_text.sql`
- Create: `server/migrations/postgres/0023_user_id_text.sql`
- Modify: `server/src/auth/token.rs` (the `Principal` enum and its `to_db`/`from_db`)
- Modify: `server/src/auth/store.rs` (`UserRow`, `create_user`, `get_user`)
- Modify: `server/src/auth/service.rs` (call sites of `Principal::User`)

**Interfaces:**
- Consumes: `UserId` from Task 1.
- Produces: `Principal::User(UserId)` replacing `Principal::User(i64)`; `AuthStore::create_user(...) -> Result<UserId, String>` which generates the id itself; `UserRow.id: UserId`.

- [ ] **Step 1: Write the failing migration test**

Add to the test module in `server/src/db/mod.rs`:

```rust
/// The bootstrap account keeps a usable id across the retype — `'1'`, the text
/// of the integer it had. It is a legitimate id, not a sentinel: accounts
/// created after this migration get a random one.
#[tokio::test]
async fn retyping_the_user_id_preserves_the_bootstrap_row() {
    let db = testing::db().await;
    sqlx::query(&db.q(
        "INSERT INTO auth_users (username, password_hash, password_is_generated, \
         created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
    ))
    .bind("admin")
    .bind("$argon2id$fake")
    .bind(0_i64)
    .bind(1_i64)
    .bind(1_i64)
    .execute(db.pool())
    .await
    .unwrap();

    let id: String = sqlx::query_scalar(&db.q("SELECT id FROM auth_users WHERE username = ?"))
        .bind("admin")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(id, "1");
}
```

Note this asserts the *post-migration* shape: `testing::db()` migrates all the way up, so before the migration exists the column is an INTEGER and `query_scalar::<String>` fails at runtime.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p horsie-server --lib db::tests::retyping_the_user_id`
Expected: FAIL — a decode error, because `id` is still `INTEGER`.

- [ ] **Step 3: Write the migrations**

`server/migrations/sqlite/0023_user_id_text.sql` — SQLite cannot change a column's type or its primary key, so the table is rebuilt:

```sql
-- A user id is a short random string, not an autoincrementing integer: a
-- sequential key published as a scope leaks how many accounts a deployment has
-- and makes the set enumerable.
--
-- SQLite cannot retype a column or alter a primary key, so this rebuilds the
-- table. The single bootstrap row keeps `'1'` — the text of the integer it had,
-- a legitimate id rather than a sentinel. Accounts created after this migration
-- get a random one from `create_user`.
--
-- No REFERENCES clauses: `PRAGMA foreign_keys` is never enabled in `open_pool`,
-- so a declared constraint would be silently ignored. See 0009_memory.sql.

CREATE TABLE auth_users_new (
    id                    TEXT PRIMARY KEY,
    username              TEXT NOT NULL UNIQUE,
    password_hash         TEXT NOT NULL,
    password_is_generated INTEGER NOT NULL DEFAULT 0,
    created_at            INTEGER NOT NULL,
    updated_at            INTEGER NOT NULL
);

INSERT INTO auth_users_new (id, username, password_hash, password_is_generated,
                            created_at, updated_at)
SELECT CAST(id AS TEXT), username, password_hash, password_is_generated,
       created_at, updated_at
FROM auth_users;

DROP TABLE auth_users;
ALTER TABLE auth_users_new RENAME TO auth_users;
```

`server/migrations/postgres/0023_user_id_text.sql`:

```sql
-- PostgreSQL mirror of migrations/sqlite/0023_user_id_text.sql.
--
-- A user id is a short random string, not an autoincrementing integer: a
-- sequential key published as a scope leaks how many accounts a deployment has
-- and makes the set enumerable. The single bootstrap row keeps `'1'`, the text
-- of the integer it had.
--
-- PostgreSQL can retype in place, so no rebuild is needed. Dropping the
-- identity/default is what stops the sequence quietly continuing to supply
-- values for a column that is no longer an integer.

ALTER TABLE auth_users ALTER COLUMN id DROP DEFAULT;
ALTER TABLE auth_users ALTER COLUMN id TYPE TEXT USING id::TEXT;
DROP SEQUENCE IF EXISTS auth_users_id_seq;
```

Before writing the PostgreSQL file, confirm the existing column's exact shape — `0014_auth.sql`'s PostgreSQL mirror may use `BIGSERIAL` or `GENERATED ... AS IDENTITY`, and the two need different drop statements:

Run: `grep -A 4 "CREATE TABLE auth_users" server/migrations/postgres/0014_auth.sql`

If it is `GENERATED BY DEFAULT AS IDENTITY`, replace the first and third statements with `ALTER TABLE auth_users ALTER COLUMN id DROP IDENTITY IF EXISTS;`.

- [ ] **Step 4: Retype `Principal`**

In `server/src/auth/token.rs`, replace the enum and its two conversions:

```rust
/// Who a request is acting as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Principal {
    Anonymous,
    User(UserId),
}

impl Principal {
    pub fn to_db(&self) -> String {
        match self {
            Self::Anonymous => "anonymous".to_string(),
            Self::User(id) => format!("user:{id}"),
        }
    }

    pub fn from_db(s: &str) -> Result<Self, String> {
        if s == "anonymous" {
            return Ok(Self::Anonymous);
        }
        match s.split_once(':') {
            // No parse step any more: the id is already the string form. An
            // empty id is still rejected, because `user:` is not an account.
            Some(("user", id)) if !id.is_empty() => Ok(Self::User(UserId::new(id))),
            _ => Err(format!("unrecognized principal {s:?}")),
        }
    }
}
```

Add `use crate::auth::UserId;` at the top of the file. Update the existing round-trip tests in that module, which assert on `Principal::User(7)`:

```rust
#[test]
fn a_principal_round_trips_through_the_database_form() {
    let p = Principal::User(UserId::new("k3m9x0abc7qr"));
    assert_eq!(p.to_db(), "user:k3m9x0abc7qr");
    assert_eq!(Principal::from_db("user:k3m9x0abc7qr"), Ok(p));
    assert!(Principal::from_db("user:").is_err());
    assert!(Principal::from_db("wat").is_err());
}
```

- [ ] **Step 5: Make `create_user` mint the id**

In `server/src/auth/store.rs`, change `UserRow.id` from `i64` to `UserId`, and have `create_user` generate and insert the id rather than relying on `AUTOINCREMENT`:

```rust
/// Insert an account, returning the id it was given.
///
/// The id is minted here rather than by the database: it is random, not
/// sequential, so there is no sequence to draw from.
pub async fn create_user(
    &self,
    username: &str,
    password_hash: &str,
    password_is_generated: bool,
    now: i64,
) -> Result<UserId, String> {
    let id = UserId::generate();
    sqlx::query(&self.db.q(
        "INSERT INTO auth_users (id, username, password_hash, \
         password_is_generated, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)",
    ))
    .bind(id.as_str())
    .bind(username)
    .bind(password_hash)
    .bind(i64::from(password_is_generated))
    .bind(now)
    .bind(now)
    .execute(self.db.pool())
    .await
    .map_err(|e| format!("create user '{username}': {e}"))?;
    Ok(id)
}
```

Update `get_user`'s row mapping to read `id` as `String` and wrap it: `UserId::new(row.try_get::<String, _>("id")?)`.

Then follow the compiler through `server/src/auth/service.rs` and the http handlers: every `Principal::User(user.id)` now moves or clones a `UserId` rather than copying an `i64`.

- [ ] **Step 6: Run the tests**

Run: `cargo test -p horsie-server --lib auth db::tests`
Expected: PASS, including `retyping_the_user_id_preserves_the_bootstrap_row`.

- [ ] **Step 7: Commit**

```bash
git add server/migrations server/src/auth
git commit -m "auth: user ids are text, minted by create_user"
```

---

### Task 3: The scoping migration

**Files:**
- Create: `server/migrations/sqlite/0024_user_scoping.sql`
- Create: `server/migrations/postgres/0024_user_scoping.sql`
- Modify: `server/src/db/mod.rs` (test module)

**Interfaces:**
- Consumes: `auth_users.id` as `TEXT` from Task 2.
- Produces: a `user_id TEXT NOT NULL` column on sixteen tables, thirteen of them as the first half of a composite primary key. **No column has a default after this migration** — see Step 3.

- [ ] **Step 1: Capture the current schema**

The current shape of each table is its original `CREATE TABLE` plus every later `ALTER`, so do not reconstruct it by reading migration files. Derive it:

```bash
cargo install sqlx-cli --no-default-features --features sqlite   # if not present
rm -f /tmp/horsie-schema.db
sqlx migrate run --source server/migrations/sqlite \
  --database-url "sqlite:///tmp/horsie-schema.db?mode=rwc"
sqlite3 /tmp/horsie-schema.db ".schema providers"   # repeat per table
```

The thirteen tables that need a composite primary key, with the column that is the primary key today:

| Table | Current PK column |
| --- | --- |
| `providers` | `name` |
| `models` | `alias` |
| `settings` | `key` |
| `mcp_servers` | `name` |
| `plugins` | `name` |
| `memory_spaces` | `name` |
| `agents` | `name` |
| `routines` | `name` |
| `environments` | `name` |
| `workflows` | `name` |
| `provider_oauth` | `provider` |
| `marketplaces` | `name` |
| `model_cards` | `model_id` |

Three tables need only a column: `memories`, `github_credentials`, `journal_logs`.

One table is dropped: `vendors`. It has no query sites in `server/src` — `config/store.rs` states that the vendor map "starts empty at boot and is never repopulated from the database".

Three are deliberately untouched: `github_app` (deployment config — one App registration per deployment), `journal_events` and `journal_snapshots` (scoped through `journal_logs.log_id`).

- [ ] **Step 2: Write the failing test**

Add to the test module in `server/src/db/mod.rs`:

```rust
/// Every scoped table rejects an insert that omits the scope, and the rebuild
/// carried existing rows across with the bootstrap account's id.
#[tokio::test]
async fn the_scope_column_is_required_and_backfilled() {
    let db = testing::db().await;

    // A row inserted the old way, without a scope, must fail — no default.
    let no_scope = sqlx::query(&db.q("INSERT INTO memory_spaces (name, description, \
                                      created_at, updated_at) VALUES (?, ?, ?, ?)"))
        .bind("notes")
        .bind("")
        .bind("2026-01-01 00:00:00")
        .bind("2026-01-01 00:00:00")
        .execute(db.pool())
        .await;
    assert!(no_scope.is_err(), "a missing user_id must be a constraint error");

    // Two users may hold the same name — that is the whole point of the
    // composite key.
    for user in ["1", "k3m9x0abc7qr"] {
        sqlx::query(&db.q("INSERT INTO memory_spaces (user_id, name, description, \
                           created_at, updated_at) VALUES (?, ?, ?, ?, ?)"))
            .bind(user)
            .bind("notes")
            .bind("")
            .bind("2026-01-01 00:00:00")
            .bind("2026-01-01 00:00:00")
            .execute(db.pool())
            .await
            .unwrap();
    }

    let n: i64 = sqlx::query_scalar(&db.q("SELECT COUNT(*) FROM memory_spaces"))
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(n, 2);
}

#[tokio::test]
async fn the_vestigial_vendors_table_is_gone() {
    let db = testing::db().await;
    assert!(db.execute("SELECT 1 FROM vendors").await.is_err());
}
```

- [ ] **Step 3: Run it to verify it fails**

Run: `cargo test -p horsie-server --lib db::tests::the_scope_column db::tests::the_vestigial`
Expected: FAIL — the first insert succeeds (no such column, no constraint), and `vendors` still exists.

- [ ] **Step 4: Write the SQLite migration**

`server/migrations/sqlite/0024_user_scoping.sql`. For each of the thirteen, the pattern below — shown fully for `providers`, and applied identically to the rest using the schema captured in Step 1:

```sql
-- Every durable row belongs to a user. `user_id` is the first half of the
-- primary key wherever the key was a natural name, so two accounts may hold the
-- same provider, model, skill bundle or memory space without collision.
--
-- SQLite cannot alter a primary key, so each of these is a rebuild.
--
-- Deliberately NO DEFAULT on user_id after the backfill. A default would make
-- an INSERT that forgets the scope land silently in the bootstrap account's
-- data; without one it is a NOT NULL violation, which is a test failure rather
-- than a cross-account leak.

CREATE TABLE providers_new (
    user_id TEXT NOT NULL,
    name    TEXT NOT NULL,
    -- ...every remaining column, copied verbatim from Step 1's `.schema`...
    PRIMARY KEY (user_id, name)
);
INSERT INTO providers_new SELECT '1', * FROM providers;
DROP TABLE providers;
ALTER TABLE providers_new RENAME TO providers;
```

`INSERT INTO providers_new SELECT '1', * FROM providers` relies on the new table's columns being the old ones in the same order with `user_id` prepended — which is why `user_id` goes first in every `CREATE TABLE` here. If a table's rebuild reorders anything, name the columns explicitly instead.

Then the three plain additions, and the drop:

```sql
ALTER TABLE memories ADD COLUMN user_id TEXT NOT NULL DEFAULT '1';
ALTER TABLE github_credentials ADD COLUMN user_id TEXT NOT NULL DEFAULT '1';
ALTER TABLE journal_logs ADD COLUMN user_id TEXT NOT NULL DEFAULT '1';

-- Vestigial: no query site in server/src, and the vendor map is never
-- populated from the database. See config/store.rs.
DROP TABLE vendors;
```

SQLite has no `ALTER COLUMN`, so those three keep their default — `ADD COLUMN ... NOT NULL` requires one. Remove it by rebuilding those three tables the same way as the thirteen, so that all sixteen behave identically. Do that rather than leaving three tables with a silent fallback.

Add indexes for the scoped lookups that are no longer covered by the primary key:

```sql
CREATE INDEX idx_memories_user ON memories(user_id, space);
CREATE INDEX idx_github_credentials_user ON github_credentials(user_id);
CREATE INDEX idx_journal_logs_user ON journal_logs(user_id);
```

- [ ] **Step 5: Write the PostgreSQL migration**

`server/migrations/postgres/0024_user_scoping.sql` — no rebuild needed, but the default must be dropped after the backfill for the same reason:

```sql
-- PostgreSQL mirror of migrations/sqlite/0024_user_scoping.sql.
--
-- Same shape, without the rebuilds: PostgreSQL can alter a primary key in
-- place. The DROP DEFAULT after each backfill is load-bearing — see the SQLite
-- file for why a default would turn a forgotten scope into a silent leak.

ALTER TABLE providers ADD COLUMN user_id TEXT NOT NULL DEFAULT '1';
ALTER TABLE providers DROP CONSTRAINT providers_pkey;
ALTER TABLE providers ADD PRIMARY KEY (user_id, name);
ALTER TABLE providers ALTER COLUMN user_id DROP DEFAULT;
-- ...repeat for the other twelve, substituting the table and its PK column...

ALTER TABLE memories ADD COLUMN user_id TEXT NOT NULL DEFAULT '1';
ALTER TABLE memories ALTER COLUMN user_id DROP DEFAULT;
-- ...same for github_credentials and journal_logs...

DROP TABLE vendors;

CREATE INDEX idx_memories_user ON memories(user_id, space);
CREATE INDEX idx_github_credentials_user ON github_credentials(user_id);
CREATE INDEX idx_journal_logs_user ON journal_logs(user_id);
```

Confirm each constraint name before writing it — PostgreSQL's default is `<table>_pkey`, but `0001_init.sql`'s mirror may have named some explicitly:

Run: `grep -n "PRIMARY KEY\|CONSTRAINT" server/migrations/postgres/*.sql`

- [ ] **Step 6: Run the tests**

Run: `cargo test -p horsie-server --lib db::tests`
Expected: PASS, including `migrations_are_in_parity` and `migration_versions_are_unique`.

Then against PostgreSQL, which is the half most likely to differ:

Run: `HORSIE_TEST_POSTGRES_URL=postgres://localhost/postgres cargo test -p horsie-server --lib db::tests`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add server/migrations server/src/db/mod.rs
git commit -m "db: user_id on every scoped table"
```

---

### Task 4: Scope the config stores

**Files:**
- Modify: `server/src/config/store.rs` (16 query sites)
- Modify: `server/src/config/model_cards.rs` (12 query sites)
- Modify: `server/src/config/chatgpt_login.rs` (3 query sites)
- Modify: `server/src/bin/horsie-server/main.rs` (construction sites)

**Interfaces:**
- Consumes: `UserId` from Task 1; the schema from Task 3.
- Produces: `DbConfigStore::open_with(url, max_conns, deps, user: UserId)`, `ModelCardStore::new(db: Db, user: UserId)`, `ChatGptLoginService::new(db, store, user: UserId)`. Every later task follows this shape: **the scope is a constructor argument, never a method argument.**

- [ ] **Step 1: Write the failing test**

Add to the test module in `server/src/config/model_cards.rs`:

```rust
#[tokio::test]
async fn a_card_is_invisible_to_another_user() {
    let db = crate::db::testing::db().await;
    let mine = ModelCardStore::new(db.clone(), UserId::new("1"));
    let theirs = ModelCardStore::new(db, UserId::new("k3m9x0abc7qr"));

    mine.upsert(&card("claude-opus-5")).await.unwrap();

    assert!(mine.get("claude-opus-5").await.unwrap().is_some());
    assert!(theirs.get("claude-opus-5").await.unwrap().is_none());
    assert!(theirs.list().await.unwrap().is_empty());
}
```

Use whatever fixture helper that module already has in place of `card(...)`; if it has none, build a `ModelCardInput` inline with the fields the existing tests use.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p horsie-server --lib config::model_cards`
Expected: FAIL to compile — `ModelCardStore::new` takes one argument.

- [ ] **Step 3: Bind the scope at construction**

For each of the three stores: add a `user: UserId` field, take it in the constructor, and add the predicate to every statement. The three shapes, all of which appear in these files:

```rust
// SELECT and DELETE — an extra predicate, bound first so the existing
// `.bind(...)` order after it is unchanged.
"SELECT {COLS} FROM model_cards WHERE user_id = ? AND model_id = ?"
  .bind(self.user.as_str())
  .bind(model_id)

// INSERT — an extra column.
"INSERT INTO model_cards (user_id, model_id, ...) VALUES (?, ?, ...)"
  .bind(self.user.as_str())

// UPDATE — the predicate goes in the WHERE, never in the SET.
"UPDATE model_cards SET ... WHERE user_id = ? AND model_id = ?"
```

`config/store.rs` needs two further changes beyond the mechanical ones:

- `DbConfigStore::open_with` both opens the pool *and* reads providers, models and settings to build the registry. Opening stays global; the three reads become scoped. Take `user: UserId` as a parameter and store it on the struct.
- `read_setting(&db, pool, "default_vendor")` and its writer are per-user like every other setting.

- [ ] **Step 4: Update the construction sites**

In `server/src/bin/horsie-server/main.rs`, the single account is the scope for now. Add near the top of `run`, after the auth bootstrap:

```rust
// One account until the runtime tier lands, so every service is built for it.
// `bootstrap` has already created it if this is a first boot.
let user = auth
    .sole_user()
    .await
    .map_err(BootError::Config)?
    .ok_or_else(|| BootError::Config("no account exists".into()))?;
```

This needs two new methods. In `server/src/auth/store.rs`, next to `user_count`:

```rust
/// The id of the only account, or `None` when there is none yet.
///
/// Deliberately not "the first account": while there is exactly one, ordering
/// is not a question, and this call is replaced outright when the runtime tier
/// resolves a scope per request.
pub async fn sole_user(&self) -> Result<Option<UserId>, String> {
    let id: Option<String> = sqlx::query_scalar(&self.db.q("SELECT id FROM auth_users LIMIT 1"))
        .fetch_optional(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
    Ok(id.map(UserId::new))
}
```

And the one-line delegation in `server/src/auth/service.rs`, so `main.rs` keeps talking to the service rather than reaching past it to the store:

```rust
pub async fn sole_user(&self) -> Result<Option<UserId>, String> {
    self.store.sole_user().await
}
```

Move the `DbConfigStore::open_with` call below the auth bootstrap so `user` exists before it — the bootstrap only needs the `Db`, which `open_with` returns, so open the pool first, bootstrap, then build the config store. If that reordering proves awkward, split `open_with` into `open` (pool + migrations) and `load(user)` (registry).

- [ ] **Step 5: Run the tests**

Run: `cargo test -p horsie-server --lib config`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add server/src/config server/src/auth server/src/bin
git commit -m "config: bind the user scope at store construction"
```

---

### Task 5: Scope the memory, plugin and marketplace stores

**Files:**
- Modify: `server/src/memory/store.rs` (19 query sites)
- Modify: `server/src/plugins/store.rs` (7 query sites)
- Modify: `server/src/plugins/marketplace_store.rs` (5 query sites)
- Modify: `server/src/bin/horsie-server/main.rs` (construction sites)

**Interfaces:**
- Consumes: `UserId`; the constructor pattern from Task 4.
- Produces: `MemoryStore::new(db, user)`, `PluginStore::new(db, user)`, `MarketplaceStore::new(db, user)`.

- [ ] **Step 1: Write the failing test**

Add to the test module in `server/src/memory/store.rs`:

```rust
#[tokio::test]
async fn two_users_may_hold_the_same_space_name() {
    let db = crate::db::testing::db().await;
    let mine = MemoryStore::new(db.clone(), UserId::new("1"));
    let theirs = MemoryStore::new(db, UserId::new("k3m9x0abc7qr"));

    let row = MemorySpaceRow {
        name: "notes".into(),
        description: "mine".into(),
        created_at: "2026-01-01 00:00:00".into(),
        updated_at: "2026-01-01 00:00:00".into(),
    };
    mine.create_space(&row).await.unwrap();
    theirs
        .create_space(&MemorySpaceRow { description: "theirs".into(), ..row })
        .await
        .unwrap();

    assert_eq!(mine.get_space("notes").await.unwrap().unwrap().description, "mine");
    assert_eq!(theirs.get_space("notes").await.unwrap().unwrap().description, "theirs");
    assert_eq!(mine.list_spaces().await.unwrap().len(), 1);
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p horsie-server --lib memory::store`
Expected: FAIL to compile — `MemoryStore::new` takes one argument.

- [ ] **Step 3: Bind the scope**

Apply Task 4's three shapes to every statement in the three files. Two places in `memory/store.rs` need more than a predicate:

- `rename_space` runs three statements in one transaction, because the space name is the join key for `memories`. Every one of them gains `user_id = ?`, including the `UPDATE memories SET space = ?`.
- `delete_space` fixes up children the same way.

In `plugins/store.rs`, leave the artifact-hash query that feeds `ArtifactStore::gc` **unscoped**, and say why at the call site:

```rust
/// Every artifact hash any account still references.
///
/// Deliberately NOT scoped: artifacts are content-addressed and shared between
/// accounts, so a scoped `keep` set would make GC delete bundle bytes another
/// account is still using.
pub async fn all_referenced_hashes(&self) -> Result<HashSet<String>, String> {
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p horsie-server --lib memory plugins`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add server/src/memory server/src/plugins server/src/bin
git commit -m "memory, plugins: bind the user scope at store construction"
```

---

### Task 6: Scope the definition stores

**Files:**
- Modify: `server/src/agents/store.rs` (5 query sites)
- Modify: `server/src/routines/store.rs` (10 query sites)
- Modify: `server/src/workflows/store.rs` (5 query sites)
- Modify: `server/src/environments/store.rs` (6 query sites)
- Modify: `server/src/routines/scheduler.rs`
- Modify: `server/src/bin/horsie-server/main.rs` (construction sites)

**Interfaces:**
- Consumes: `UserId`; the constructor pattern from Task 4.
- Produces: `AgentStore::new(db, user)`, `RoutineStore::new(db, user)`, `WorkflowStore::new(db, user)`, `EnvironmentStore::new(db, user)`, and `RoutineStore::due_across_all_users(now_ms) -> Vec<(UserId, RoutineRow)>`.

- [ ] **Step 1: Write the failing test**

Add to the test module in `server/src/routines/store.rs`:

```rust
/// The scheduler is a deployment-wide timer, so finding what is due must cross
/// accounts — and must say whose each one is, because running it is scoped.
#[tokio::test]
async fn due_crosses_accounts_and_reports_the_owner() {
    let db = crate::db::testing::db().await;
    let mine = RoutineStore::new(db.clone(), UserId::new("1"));
    let theirs = RoutineStore::new(db.clone(), UserId::new("k3m9x0abc7qr"));

    mine.create(&routine("nightly", 100)).await.unwrap();
    theirs.create(&routine("nightly", 100)).await.unwrap();

    let due = RoutineStore::due_across_all_users(&db, 200).await.unwrap();
    let owners: Vec<String> = due.iter().map(|(u, _)| u.as_str().to_string()).collect();
    assert_eq!(due.len(), 2);
    assert!(owners.contains(&"1".to_string()));
    assert!(owners.contains(&"k3m9x0abc7qr".to_string()));

    // The scoped read still sees only its own.
    assert_eq!(mine.list().await.unwrap().len(), 1);
}
```

Use the module's existing routine fixture in place of `routine(...)`; if it has none, build a `RoutineRow` inline matching the fields the neighbouring tests set.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p horsie-server --lib routines::store`
Expected: FAIL to compile — no `due_across_all_users`, and `RoutineStore::new` takes one argument.

- [ ] **Step 3: Bind the scope, and make `due` an associate function**

Apply Task 4's three shapes to all four stores. `routines/store.rs` is the exception that matters:

- `due(now_ms)` becomes `due_across_all_users(db: &Db, now_ms) -> Result<Vec<(UserId, RoutineRow)>, String>` — an associated function taking the `Db` directly, because it belongs to no one account. Select `user_id` alongside the rest and return it.
- `arm(name, next)` stays scoped, so claiming a routine happens as its owner.

In `server/src/routines/scheduler.rs`, `tick` iterates the pairs and does its work as each owner:

```rust
/// Deliberately unscoped: one timer serves the deployment, and each routine
/// runs as whoever owns it.
pub async fn tick(&self, now_ms: u64) {
    let due = match RoutineStore::due_across_all_users(&self.db, now_ms).await {
        Ok(due) => due,
        Err(e) => {
            tracing::error!(error = %e, "reading due routines failed");
            return;
        }
    };
    for (owner, routine) in due {
        let store = RoutineStore::new(self.db.clone(), owner.clone());
        let next = next_run_at(&routine.schedule, routine.enabled, now_ms);
        if let Err(e) = store.arm(&routine.name, next).await {
            tracing::error!(routine = %routine.name, error = %e, "arming a routine failed");
            continue;
        }
        if let Err(e) = self.runner.run(&owner, &routine.name, now_ms).await {
            tracing::warn!(routine = %routine.name, error = %e, "routine run did not start");
        }
    }
}
```

`RoutineScheduler` gains a `db: Db` field for this. `RoutineRunner::run` gains the owner as its first parameter; until the runtime tier lands it can assert the owner matches the single account rather than resolving services per user.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p horsie-server --lib agents routines workflows environments`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add server/src/agents server/src/routines server/src/workflows server/src/environments server/src/bin
git commit -m "agents, routines, workflows, environments: bind the user scope"
```

---

### Task 7: Scope the MCP and GitHub stores

**Files:**
- Modify: `server/src/mcp/store.rs` (8 query sites)
- Modify: `server/src/github/store.rs` (5 query sites)
- Modify: `server/src/bin/horsie-server/main.rs` (construction sites)

**Interfaces:**
- Consumes: `UserId`; the constructor pattern from Task 4.
- Produces: `McpStore::new(db, user)`, `GithubStore::new(db, user)`.

- [ ] **Step 1: Write the failing test**

Add to the test module in `server/src/github/store.rs`:

```rust
/// The App registration is deployment config and stays shared; the credentials
/// an account holds against it do not.
#[tokio::test]
async fn credentials_are_scoped_but_the_app_is_not() {
    let db = crate::db::testing::db().await;
    let mine = GithubStore::new(db.clone(), UserId::new("1"));
    let theirs = GithubStore::new(db, UserId::new("k3m9x0abc7qr"));

    mine.save_app(&app_config()).await.unwrap();
    mine.save_credentials(&credentials("ghu_mine")).await.unwrap();

    // Same App, both accounts.
    assert!(theirs.load_app().await.unwrap().is_some());
    // Different credentials.
    assert!(theirs.load_credentials().await.unwrap().is_none());
}
```

Use the module's existing fixtures in place of `app_config()` and `credentials(...)`.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p horsie-server --lib github::store`
Expected: FAIL to compile — `GithubStore::new` takes one argument.

- [ ] **Step 3: Bind the scope**

Apply Task 4's three shapes. The one judgement call: in `github/store.rs`, statements against `github_app` take **no** predicate, and the ones against `github_credentials` take one. Write the reason above the `github_app` accessors:

```rust
/// Deliberately unscoped: a GitHub App is registered against the deployment —
/// one callback URL, one client id, one private key, all bound to this server's
/// public address. Accounts *install* that App, which is what
/// `github_credentials` holds.
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p horsie-server --lib mcp github`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add server/src/mcp server/src/github server/src/bin
git commit -m "mcp, github: bind the user scope at store construction"
```

---

### Task 8: Scope the journal

**Files:**
- Modify: `server/src/db/journal.rs` (16 query sites)
- Modify: `server/src/bin/horsie-server/main.rs` (construction site)

**Interfaces:**
- Consumes: `UserId`; the `journal_logs.user_id` column from Task 3.
- Produces: `SqlJournal::new(db: Db, user: UserId)`.

- [ ] **Step 1: Write the failing test**

Add to the test module in `server/src/db/journal.rs`:

```rust
/// Two accounts may run actors with identical persistence ids without seeing
/// each other's events. The `(kind, id)` lookup is what binds the scope: with
/// no `log_id` there is no way to reach the events at all.
#[tokio::test]
async fn identical_persistence_ids_do_not_collide_across_accounts() {
    let db = crate::db::testing::db().await;
    let mine = SqlJournal::new(db.clone(), UserId::new("1"));
    let theirs = SqlJournal::new(db, UserId::new("k3m9x0abc7qr"));
    let pid = PersistenceId::new("session", "same-id");

    mine.persist(&pid, &[b"mine".to_vec()]).await.unwrap();
    theirs.persist(&pid, &[b"theirs".to_vec()]).await.unwrap();

    let mut mine_events = Vec::new();
    mine.replay(&pid, 0, &mut |_, e| mine_events.push(e.to_vec()))
        .await
        .unwrap();
    assert_eq!(mine_events, vec![b"mine".to_vec()]);
}
```

Match the module's existing `replay` signature — if its callback shape differs, use whatever the neighbouring tests in that file use.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p horsie-server --lib db::journal`
Expected: FAIL to compile — `SqlJournal::new` takes one argument.

- [ ] **Step 3: Bind the scope**

Add the field and the constructor parameter, then scope **only** the statements against `journal_logs` — the `SELECT log_id FROM journal_logs WHERE kind = ? AND id = ?` lookup, its `INSERT`, and the `last_seq` update.

Leave the statements against `journal_events` and `journal_snapshots` alone, and say why:

```rust
// Deliberately unscoped: these are reached only by `log_id`, which comes from
// the scoped `journal_logs` lookup above. Adding `user_id` here would widen
// `PRIMARY KEY (log_id, seq)` — the WITHOUT ROWID key that makes
// `WHERE log_id = ? AND seq > ? ORDER BY seq` a contiguous range scan — to
// duplicate a fact the parent row already enforces.
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p horsie-server --lib db::journal`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add server/src/db server/src/bin
git commit -m "journal: scope the log registry by user"
```

---

### Task 9: The CI static check

**Files:**
- Create: `server/src/db/scope_audit.rs`
- Modify: `server/src/db/mod.rs`

**Interfaces:**
- Consumes: nothing at runtime — this is a test-only module.
- Produces: a test that fails when a SQL literal touches a scoped table without naming `user_id`.

- [ ] **Step 1: Write the failing test**

Create `server/src/db/scope_audit.rs`:

```rust
//! A test that reads this crate's own source and fails any SQL literal that
//! touches a scoped table without naming `user_id`.
//!
//! This is affordable only because two invariants already hold, both stated in
//! `db/mod.rs`: every statement is a literal written in this repo, and `Db::q`
//! is the single place they pass through. It is the mechanism that catches the
//! failure mode a code review does not — somebody adds a method six months from
//! now and forgets the predicate.
//!
//! PostgreSQL row-level security would be the usual answer and is unavailable:
//! SQLite has none, and it is the backend every self-hoster runs.

#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

/// Every table with a `user_id` column.
const SCOPED: &[&str] = &[
    "providers", "models", "settings", "mcp_servers", "plugins", "memory_spaces",
    "agents", "routines", "environments", "workflows", "provider_oauth",
    "marketplaces", "model_cards", "memories", "github_credentials", "journal_logs",
];

/// Statements that touch a scoped table and must NOT be scoped, each with the
/// reason it is on this list. Matched as a substring of the offending literal.
const ALLOWED: &[(&str, &str)] = &[
    (
        "all_referenced_hashes",
        "artifact GC needs the union across accounts; a scoped keep-set would \
         delete bundle bytes another account still references",
    ),
    (
        "due_across_all_users",
        "one timer serves the deployment; each routine then runs as its owner",
    ),
];

#[test]
fn every_statement_against_a_scoped_table_names_the_scope() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offences = Vec::new();
    for file in rust_files(&root) {
        let src = std::fs::read_to_string(&file).unwrap();
        for (line_no, literal) in sql_literals(&src) {
            if ALLOWED.iter().any(|(needle, _)| literal.contains(needle)) {
                continue;
            }
            let touches = SCOPED.iter().find(|t| mentions_table(&literal, t));
            if let Some(table) = touches
                && !literal.contains("user_id")
            {
                offences.push(format!(
                    "{}:{line_no}: statement touches `{table}` without `user_id`:\n    {literal}",
                    file.display()
                ));
            }
        }
    }
    assert!(
        offences.is_empty(),
        "unscoped SQL against scoped tables:\n{}\n\nIf one is deliberate, add it \
         to ALLOWED in db/scope_audit.rs with the reason.",
        offences.join("\n")
    );
}
```

- [ ] **Step 2: Write the three helpers**

Still in `scope_audit.rs`:

```rust
fn rust_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(rust_files(&path));
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
    out
}

/// Every double-quoted literal that looks like SQL, with its 1-based line.
///
/// Deliberately crude: it does not parse Rust. A literal split across a
/// `format!` boundary is seen as its pieces, which is why `mentions_table`
/// looks for the table name rather than requiring a whole statement.
fn sql_literals(src: &str) -> Vec<(usize, String)> {
    const KEYWORDS: [&str; 4] = ["SELECT ", "INSERT INTO ", "UPDATE ", "DELETE FROM "];
    let mut out = Vec::new();
    for (i, line) in src.lines().enumerate() {
        if KEYWORDS.iter().any(|k| line.contains(k)) {
            out.push((i + 1, line.trim().to_string()));
        }
    }
    out
}

/// Whether a literal names this table, as a whole word.
///
/// Substring matching would make `models` match inside `model_cards`.
fn mentions_table(literal: &str, table: &str) -> bool {
    literal.split(|c: char| !c.is_alphanumeric() && c != '_').any(|w| w == table)
}
```

A statement built across several lines shows up as separate lines, so a `WHERE user_id = ?` on the following line would not be seen. Handle it by joining a statement's continuation lines: if a line ends without a closing quote, append the next. Add that to `sql_literals` if the first run reports false positives; start simple and let the run tell you.

- [ ] **Step 3: Wire the module in**

In `server/src/db/mod.rs`, next to the existing `pub mod journal;`:

```rust
#[cfg(test)]
mod scope_audit;
```

- [ ] **Step 4: Run it**

Run: `cargo test -p horsie-server --lib db::scope_audit`
Expected: PASS. If it reports offences, each one is either a real miss from Tasks 4–8 — fix the statement — or a deliberate exception, which goes in `ALLOWED` **with its reason written out**.

- [ ] **Step 5: Prove the check actually catches something**

Temporarily change one scoped statement to drop its `AND user_id = ?`, run the test, confirm it fails and names that file and line, then put it back. A check that has never failed is a check nobody knows works.

- [ ] **Step 6: Commit**

```bash
git add server/src/db
git commit -m "db: fail CI on unscoped SQL against a scoped table"
```

---

### Task 10: The isolation harness

**Files:**
- Create: `tests/tests/user_isolation.rs`

**Interfaces:**
- Consumes: every scoped store constructor from Tasks 4–8; `AuthStore::create_user` from Task 2.
- Produces: nothing consumed by later tasks. This is the deliverable that proves the plan.

- [ ] **Step 1: Write the harness**

Create `tests/tests/user_isolation.rs`:

```rust
//! Two accounts, the same names, and no way for either to see the other.
//!
//! This is the load-bearing test of the whole scoping design. No HTTP route in
//! this repo creates a second account, so this file is the only thing that
//! exercises the isolation guarantees at all — if it rots, they rot silently
//! with it. Every store that gains a scope gains a case here.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use horsie_server::auth::UserId;
use horsie_server::db::testing;

/// Two ids that are not each other, and are not the bootstrap `'1'` by
/// accident.
fn two() -> (UserId, UserId) {
    (UserId::generate(), UserId::generate())
}

#[tokio::test]
async fn memory_spaces_are_invisible_across_accounts() {
    let db = testing::db().await;
    let (a, b) = two();
    let mine = horsie_server::memory::MemoryStore::new(db.clone(), a);
    let theirs = horsie_server::memory::MemoryStore::new(db, b);

    let row = horsie_server::memory::MemorySpaceRow {
        name: "notes".into(),
        description: "mine".into(),
        created_at: "2026-01-01 00:00:00".into(),
        updated_at: "2026-01-01 00:00:00".into(),
    };
    mine.create_space(&row).await.unwrap();

    // Read: not visible.
    assert!(theirs.get_space("notes").await.unwrap().is_none());
    assert!(theirs.list_spaces().await.unwrap().is_empty());
    // Write: does not clobber — the same name is free for the other account.
    theirs
        .create_space(&horsie_server::memory::MemorySpaceRow {
            description: "theirs".into(),
            ..row
        })
        .await
        .unwrap();
    assert_eq!(mine.get_space("notes").await.unwrap().unwrap().description, "mine");
    // Delete: does not reach across.
    theirs.delete_space("notes").await.unwrap();
    assert!(mine.get_space("notes").await.unwrap().is_some());
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p integration-tests --test user_isolation`
Expected: FAIL to compile if any of `MemoryStore`, `MemorySpaceRow` or `db::testing` is not `pub` from the crate root. Make them public rather than duplicating the types.

- [ ] **Step 3: Make it pass**

Export what the harness needs. `tests/Cargo.toml` already depends on `horsie-server` with `features = ["test-util"]`, which is what gates `db::testing`, so nothing needs adding there — only the `pub use` lines in the modules whose types this file names.

- [ ] **Step 4: Extend to every scoped store**

Repeat the read / write / delete triple for each, using that store's own natural key:

| Store | Key used twice | Read | Write | Delete |
| --- | --- | --- | --- | --- |
| `ModelCardStore` | `model_id` | `get`, `list` | `upsert` | `delete` |
| `PluginStore` | `name` | `get`, `list` | `install` | `delete` |
| `MarketplaceStore` | `name` | `get`, `list` | `add` | `remove` |
| `AgentStore` | `name` | `get`, `list` | `create` | `delete` |
| `RoutineStore` | `name` | `get`, `list` | `create` | `delete` |
| `WorkflowStore` | `name` | `get`, `list` | `create` | `delete` |
| `EnvironmentStore` | `name` | `get`, `list` | `create` | `delete` |
| `McpStore` | `name` | `get`, `list` | `create` | `delete` |
| `GithubStore` | — | `load_credentials` | `save_credentials` | `clear_credentials` |
| `DbConfigStore` | provider `name` | `settings_view` | `update` | — |
| `SqlJournal` | `PersistenceId` | `replay` | `persist` | `clear` |

Use each store's actual method names — read them off the file rather than trusting this table, and fix the table if it is wrong.

Add one case for the deliberate exceptions, asserting they *do* cross accounts:

```rust
#[tokio::test]
async fn the_two_deliberate_exceptions_still_cross_accounts() {
    let db = testing::db().await;
    let (a, b) = two();
    // Artifact GC must see both accounts' hashes, or it deletes bytes that are
    // still referenced.
    let mine = horsie_server::plugins::PluginStore::new(db.clone(), a);
    let theirs = horsie_server::plugins::PluginStore::new(db.clone(), b);
    mine.install(&plugin("one", "hash-a")).await.unwrap();
    theirs.install(&plugin("two", "hash-b")).await.unwrap();
    let keep = mine.all_referenced_hashes().await.unwrap();
    assert!(keep.contains("hash-a") && keep.contains("hash-b"));
}
```

- [ ] **Step 5: Run the whole file**

Run: `cargo test -p integration-tests --test user_isolation`
Expected: PASS, one test per store plus the exceptions case.

- [ ] **Step 6: Run everything, both backends**

Run: `make check`
Then: `HORSIE_TEST_POSTGRES_URL=postgres://localhost/postgres cargo test --workspace`
Expected: PASS both times.

- [ ] **Step 7: Commit and open the PR**

```bash
git add tests/
git commit -m "tests: prove two accounts cannot see each other"
git push -u origin per-user-scoping
```

Open the PR against `main`. Body: what the scope is, that the server still runs with one account and behaves identically, and that the harness is the only upstream exercise of the isolation guarantees. Do **not** enable auto-merge.

---

## Notes for the reviewer

Three things in this plan are decisions rather than mechanics, and are the ones worth arguing with:

- **No default on `user_id` after the backfill.** A default would make an `INSERT` that forgets the scope land silently in the bootstrap account's data. Without one it is a constraint violation — a test failure instead of a cross-account leak.
- **Two queries are deliberately unscoped**, and both would destroy data if "fixed": `all_referenced_hashes` (artifact GC) and `due_across_all_users` (the routine timer). They are on the static check's allowlist with their reasons.
- **`github_app` is not scoped** — a GitHub App is registered against the deployment, not an account. Only `github_credentials` is per-account.
