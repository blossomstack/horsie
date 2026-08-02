# Auth A: identity core + web login — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give `horsie-server` a single admin account, an opaque-token identity core shared by all three auth surfaces, and a web UI that logs in against it — on by default, disabled by explicit config.

**Architecture:** A new `server/src/auth/` module owns the schema, token format, password hashing, and policy, in the store/service split the `memory` and `plugins` modules already use. An axum middleware over the `/api` router resolves a cookie or bearer credential into a `Principal` extension and returns `401` otherwise. The browser authenticates by cookie because both SSE streams use the native `EventSource`, which cannot set headers.

**Tech Stack:** Rust 2024, axum 0.7, sqlx 0.8 (SQLite, runtime queries, embedded migrations), fluorite 0.6 wire schemas with TS codegen, React 19 + react-router 7 + TanStack Query, Playwright.

## Global Constraints

- Production code denies `clippy::unwrap_used`, `expect_used`, `panic`, and `wildcard_enum_match_arm` (workspace lints). Test modules opt out with the existing `#[cfg_attr(test, allow(...))]` / `#![allow(...)]` headers — copy the header from a neighbouring file.
- No SQL foreign keys anywhere in this schema. `PRAGMA foreign_keys` is never enabled in `open_pool`, so a declared constraint is silently ignored — worse than none. See `server/migrations/0009_memory.sql` for the precedent.
- Store fallible results as `Result<T, String>` in store/service layers, matching `memory` and `plugins`.
- Every wire type is fluorite-generated. Never hand-write a struct that crosses the HTTP boundary. JSON is camelCase.
- `make check` = `fmt-check` + `clippy --all-targets --all-features -D warnings` + `test --workspace`. Run `cargo fmt --all` **before** clippy; a formatting failure masks lint output.
- Auth defaults to **enabled**. Existing tests run with it disabled, which is a real supported configuration.
- Token secrets are `hsk_<tag>_<43 url-safe base64 chars>`; only `SHA-256(secret)` is ever stored.
- Timestamps in the new tables are `INTEGER` unix epoch seconds — unlike the `TEXT` epoch seconds used by `memory`/`plugins`, because expiry and cleanup are compared in SQL, where lexicographic string comparison is a trap.

---

### Task 1: Token format and principals

**Files:**
- Create: `server/src/auth/mod.rs`
- Create: `server/src/auth/token.rs`
- Modify: `server/src/lib.rs` (add `pub mod auth;`)
- Modify: `Cargo.toml` (workspace deps: add `rand`)
- Modify: `server/Cargo.toml` (add `rand`)

**Interfaces:**
- Consumes: nothing.
- Produces: `TokenKind::{Web, Access, Refresh, Agent}` with `tag(&self) -> &'static str` and `from_tag(&str) -> Option<TokenKind>`; `Principal::{Anonymous, User(i64)}` with `to_db(&self) -> String` and `from_db(&str) -> Result<Principal, String>`; `generate(kind: TokenKind) -> GeneratedToken { secret: String, hash: Vec<u8> }`; `hash_secret(secret: &str) -> Vec<u8>`; `parse(secret: &str) -> Option<TokenKind>`.

All four kinds are defined here even though Task 6 only mints `Web`. The tag alphabet is a wire format shared with sub-projects B and C; fixing it once avoids re-parsing later.

- [ ] **Step 1: Add the `rand` dependency**

In the root `Cargo.toml`, under `[workspace.dependencies]`, add after the `uuid` line:

```toml
rand              = "0.8"
```

In `server/Cargo.toml`, under `[dependencies]`, add after the `sha2` line:

```toml
rand              = { workspace = true }
```

- [ ] **Step 2: Write the failing test**

Create `server/src/auth/token.rs` containing only the test module:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn generated_secret_has_the_documented_shape() {
        let t = generate(TokenKind::Web);
        assert!(t.secret.starts_with("hsk_web_"), "{}", t.secret);
        // 32 random bytes, base64url no-pad
        assert_eq!(t.secret.len(), "hsk_web_".len() + 43);
        assert_eq!(t.hash.len(), 32);
        assert!(
            t.secret
                .trim_start_matches("hsk_web_")
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn two_generated_secrets_differ() {
        assert_ne!(generate(TokenKind::Web).secret, generate(TokenKind::Web).secret);
    }

    #[test]
    fn hash_is_deterministic_and_matches_generation() {
        let t = generate(TokenKind::Agent);
        assert_eq!(hash_secret(&t.secret), t.hash);
        assert_eq!(hash_secret("hsk_web_abc"), hash_secret("hsk_web_abc"));
        assert_ne!(hash_secret("hsk_web_abc"), hash_secret("hsk_web_abd"));
    }

    #[test]
    fn every_kind_round_trips_through_its_tag() {
        for kind in [TokenKind::Web, TokenKind::Access, TokenKind::Refresh, TokenKind::Agent] {
            assert_eq!(TokenKind::from_tag(kind.tag()), Some(kind));
            assert_eq!(parse(&generate(kind).secret), Some(kind));
        }
    }

    #[test]
    fn parse_rejects_junk() {
        assert_eq!(parse("hsk_nope_aaa"), None);
        assert_eq!(parse("bearer-token"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("hsk_web"), None);
    }

    #[test]
    fn principals_round_trip_through_the_database_encoding() {
        assert_eq!(Principal::User(7).to_db(), "user:7");
        assert_eq!(Principal::from_db("user:7"), Ok(Principal::User(7)));
        assert!(Principal::from_db("user:seven").is_err());
        assert!(Principal::from_db("wat").is_err());
    }
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p horsie-server auth::token`
Expected: FAIL — `server/src/auth/token.rs` is not part of any module tree yet, so this reports an unresolved module or no tests run. Both count as red.

- [ ] **Step 4: Write the implementation**

Prepend to `server/src/auth/token.rs`, above the test module:

```rust
//! Token format shared by every authenticated surface: `hsk_<tag>_<secret>`.
//!
//! The tag makes a wrong-kind credential rejectable before touching the
//! database, and the `hsk_` prefix makes tokens greppable and recognisable to
//! secret scanners. Only `SHA-256(secret)` is ever stored: a plain hash is
//! correct for a 256-bit random secret on the hot path of every request —
//! there is nothing to brute-force — whereas passwords, which have no such
//! entropy, use argon2id (see `password.rs`).

use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// What a token authorizes. `Web` is a browser cookie session, `Access` and
/// `Refresh` belong to the CLI, `Agent` to a headless vendor agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Web,
    Access,
    Refresh,
    Agent,
}

impl TokenKind {
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::Access => "usr",
            Self::Refresh => "ref",
            Self::Agent => "agt",
        }
    }

    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "web" => Some(Self::Web),
            "usr" => Some(Self::Access),
            "ref" => Some(Self::Refresh),
            "agt" => Some(Self::Agent),
            _ => None,
        }
    }

    /// The `auth_tokens.kind` column value.
    pub fn as_db(&self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::Access => "access",
            Self::Refresh => "refresh",
            Self::Agent => "agent",
        }
    }

    pub fn from_db(s: &str) -> Option<Self> {
        match s {
            "web" => Some(Self::Web),
            "access" => Some(Self::Access),
            "refresh" => Some(Self::Refresh),
            "agent" => Some(Self::Agent),
            _ => None,
        }
    }
}

/// Who a request is acting as. `Agent` arrives with sub-project C; until then
/// a verified credential is always a user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Principal {
    Anonymous,
    User(i64),
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
            Some(("user", id)) => id
                .parse::<i64>()
                .map(Self::User)
                .map_err(|_| format!("bad user id in principal {s:?}")),
            _ => Err(format!("unrecognized principal {s:?}")),
        }
    }
}

/// A freshly minted token: the secret to hand out once, and the hash to store.
pub struct GeneratedToken {
    pub secret: String,
    pub hash: Vec<u8>,
}

pub fn generate(kind: TokenKind) -> GeneratedToken {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let secret = format!("hsk_{}_{body}", kind.tag());
    let hash = hash_secret(&secret);
    GeneratedToken { secret, hash }
}

pub fn hash_secret(secret: &str) -> Vec<u8> {
    Sha256::digest(secret.as_bytes()).to_vec()
}

/// The kind a presented secret claims to be, or `None` if it is not one of
/// ours. Claiming a kind is not proof of anything — the caller still looks the
/// hash up — but it lets a wrong-kind credential be rejected for free.
pub fn parse(secret: &str) -> Option<TokenKind> {
    let rest = secret.strip_prefix("hsk_")?;
    let (tag, body) = rest.split_once('_')?;
    if body.is_empty() {
        return None;
    }
    TokenKind::from_tag(tag)
}
```

Create `server/src/auth/mod.rs`:

```rust
//! Authentication: the single admin account, opaque bearer/cookie tokens, and
//! the policy that turns a presented credential into a [`Principal`].
//!
//! Mirrors the `memory` and `plugins` modules' store/service split and shares
//! the config store's `SqlitePool`.

mod token;

pub use token::{GeneratedToken, Principal, TokenKind, generate, hash_secret, parse};
```

In `server/src/lib.rs`, add `pub mod auth;` alongside the other `pub mod` declarations, in alphabetical position (before `pub mod config;`).

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p horsie-server auth::token`
Expected: PASS, 6 tests.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock server/Cargo.toml server/src/lib.rs server/src/auth/
git commit -m "feat(auth): token format and principal encoding"
```

---

### Task 2: Schema and store

**Files:**
- Create: `server/migrations/0014_auth.sql`
- Create: `server/src/auth/store.rs`
- Modify: `server/src/auth/mod.rs`

**Interfaces:**
- Consumes: `TokenKind`, `Principal`, `hash_secret` from Task 1.
- Produces: `AuthStore::new(pool: SqlitePool)`; `UserRow { id: i64, username: String, password_hash: String, password_is_generated: bool }`; `TokenRow { id: String, kind: TokenKind, principal: Principal, label: Option<String>, expires_at: Option<i64> }`; and the methods `user_count`, `get_user`, `create_user`, `set_password`, `insert_token`, `lookup_token`, `revoke_token`, `revoke_kind_for_principal`, `touch_token`.

- [ ] **Step 1: Write the migration**

Create `server/migrations/0014_auth.sql`:

```sql
-- Authentication: one admin account plus the opaque tokens every
-- authenticated surface presents (browser cookie, CLI access/refresh, vendor
-- agent). Sub-project A mints only `web` tokens; the table is shared so the
-- CLI and vendor work does not reshape it.
--
-- No REFERENCES clauses: `PRAGMA foreign_keys` is never enabled in
-- `open_pool`, so a declared constraint would be silently ignored -- worse
-- than no constraint at all. See 0009_memory.sql.
--
-- Timestamps here are INTEGER epoch seconds, not the TEXT epoch seconds used
-- by memory/plugins: expiry and cleanup compare them in SQL, where
-- lexicographic comparison of a TEXT number is a trap waiting for a digit
-- change.

CREATE TABLE auth_users (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    username              TEXT NOT NULL UNIQUE,
    password_hash         TEXT NOT NULL,           -- argon2id PHC string
    -- 1 while the first-boot generated password is still in use
    password_is_generated INTEGER NOT NULL DEFAULT 0,
    created_at            INTEGER NOT NULL,
    updated_at            INTEGER NOT NULL
);

CREATE TABLE auth_tokens (
    id           TEXT PRIMARY KEY,      -- public uuid; safe to list and log
    kind         TEXT NOT NULL,         -- web | access | refresh | agent
    principal    TEXT NOT NULL,         -- user:<id> | agent:<token id>
    token_hash   BLOB NOT NULL UNIQUE,  -- SHA-256 of the presented secret
    label        TEXT,                  -- agent tokens: operator-chosen name
    chain_id     TEXT,                  -- access/refresh: rotation chain
    expires_at   INTEGER,               -- NULL = never (agent tokens)
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER,
    revoked_at   INTEGER
);

CREATE INDEX idx_auth_tokens_hash ON auth_tokens(token_hash);
CREATE INDEX idx_auth_tokens_chain ON auth_tokens(chain_id);
CREATE INDEX idx_auth_tokens_principal ON auth_tokens(principal, kind);

-- Unused until the CLI device flow (sub-project B) lands. Created here so the
-- auth schema arrives as one migration rather than reshaping a shipped table.
CREATE TABLE auth_device_codes (
    device_code_hash BLOB PRIMARY KEY,
    user_code        TEXT NOT NULL UNIQUE,
    principal        TEXT,              -- set on approval
    created_at       INTEGER NOT NULL,
    expires_at       INTEGER NOT NULL,
    approved_at      INTEGER,
    denied_at        INTEGER,
    consumed_at      INTEGER,
    last_polled_at   INTEGER            -- drives slow_down
);
```

- [ ] **Step 2: Write the failing test**

Create `server/src/auth/store.rs` containing only the test module:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::auth::{TokenKind, generate};

    /// A migrated, file-backed temp database. NOT `sqlite::memory:` — every
    /// pooled connection would get its own private database, so a row written
    /// through one connection is invisible to the next. Same shape as
    /// `memory::store`'s test helper.
    async fn store() -> (AuthStore, tempfile::TempDir) {
        use std::str::FromStr;
        let tmp = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}/t.db", tmp.path().display());
        let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
            .unwrap()
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePool::connect_with(opts).await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        (AuthStore::new(pool), tmp)
    }

    #[tokio::test]
    async fn creates_and_reads_back_the_admin_user() {
        let (s, _tmp) = store().await;
        assert_eq!(s.user_count().await.unwrap(), 0);
        assert!(s.get_user("admin").await.unwrap().is_none());

        let id = s.create_user("admin", "phc-hash", true, 1000).await.unwrap();
        assert_eq!(s.user_count().await.unwrap(), 1);

        let u = s.get_user("admin").await.unwrap().unwrap();
        assert_eq!(u.id, id);
        assert_eq!(u.password_hash, "phc-hash");
        assert!(u.password_is_generated);
    }

    #[tokio::test]
    async fn a_second_user_is_refused() {
        let (s, _tmp) = store().await;
        s.create_user("admin", "h", true, 1000).await.unwrap();
        assert!(s.create_user("other", "h", false, 1000).await.is_err());
    }

    #[tokio::test]
    async fn set_password_clears_the_generated_flag() {
        let (s, _tmp) = store().await;
        s.create_user("admin", "old", true, 1000).await.unwrap();
        s.set_password("admin", "new", 2000).await.unwrap();
        let u = s.get_user("admin").await.unwrap().unwrap();
        assert_eq!(u.password_hash, "new");
        assert!(!u.password_is_generated);
    }

    #[tokio::test]
    async fn a_live_token_looks_up_and_a_revoked_or_expired_one_does_not() {
        let (s, _tmp) = store().await;
        let live = generate(TokenKind::Web);
        s.insert_token(
            "id-live", TokenKind::Web, &Principal::User(1), &live.hash,
            None, None, Some(9_999_999_999), 1000,
        ).await.unwrap();

        let found = s.lookup_token(&live.hash, 1001).await.unwrap().unwrap();
        assert_eq!(found.id, "id-live");
        assert_eq!(found.kind, TokenKind::Web);
        assert_eq!(found.principal, Principal::User(1));

        // Expired.
        let old = generate(TokenKind::Web);
        s.insert_token(
            "id-old", TokenKind::Web, &Principal::User(1), &old.hash,
            None, None, Some(500), 100,
        ).await.unwrap();
        assert!(s.lookup_token(&old.hash, 1001).await.unwrap().is_none());

        // Revoked.
        s.revoke_token("id-live", 1002).await.unwrap();
        assert!(s.lookup_token(&live.hash, 1003).await.unwrap().is_none());

        // Never issued.
        assert!(s.lookup_token(b"nope", 1003).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_token_with_no_expiry_never_expires() {
        let (s, _tmp) = store().await;
        let t = generate(TokenKind::Agent);
        s.insert_token(
            "id-agent", TokenKind::Agent, &Principal::User(1), &t.hash,
            Some("laptop"), None, None, 1000,
        ).await.unwrap();
        let found = s.lookup_token(&t.hash, 9_999_999_999).await.unwrap().unwrap();
        assert_eq!(found.label.as_deref(), Some("laptop"));
    }

    #[tokio::test]
    async fn revoking_a_kind_leaves_the_excepted_token_and_other_kinds_alone() {
        let (s, _tmp) = store().await;
        let (a, b, c) = (generate(TokenKind::Web), generate(TokenKind::Web), generate(TokenKind::Agent));
        for (id, kind, tok) in [
            ("a", TokenKind::Web, &a), ("b", TokenKind::Web, &b), ("c", TokenKind::Agent, &c),
        ] {
            s.insert_token(id, kind, &Principal::User(1), &tok.hash, None, None, None, 1000)
                .await
                .unwrap();
        }
        s.revoke_kind_for_principal(&Principal::User(1), TokenKind::Web, Some("a"), 2000)
            .await
            .unwrap();
        assert!(s.lookup_token(&a.hash, 2001).await.unwrap().is_some());
        assert!(s.lookup_token(&b.hash, 2001).await.unwrap().is_none());
        assert!(s.lookup_token(&c.hash, 2001).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn touch_writes_last_used_at_most_once_a_minute() {
        let (s, _tmp) = store().await;
        let t = generate(TokenKind::Web);
        s.insert_token("id", TokenKind::Web, &Principal::User(1), &t.hash, None, None, None, 1000)
            .await
            .unwrap();

        // First touch always writes.
        assert!(s.touch_token("id", None, 1000).await.unwrap());
        // Within the minute: skipped.
        assert!(!s.touch_token("id", Some(1000), 1030).await.unwrap());
        // Past the minute: written.
        assert!(s.touch_token("id", Some(1000), 1061).await.unwrap());
    }
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p horsie-server auth::store`
Expected: FAIL — `AuthStore` is undefined.

- [ ] **Step 4: Write the implementation**

Prepend to `server/src/auth/store.rs`:

```rust
//! SQLite storage for the admin account and every issued token, sharing the
//! config store's pool. Policy lives in `service.rs`; this layer only reads and
//! writes rows.

use crate::auth::{Principal, TokenKind};
use sqlx::Row;
use sqlx::sqlite::{SqlitePool, SqliteRow};

/// One row of `auth_users`.
#[derive(Clone, Debug, PartialEq)]
pub struct UserRow {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
    pub password_is_generated: bool,
}

/// A live token: what `lookup_token` returns once expiry and revocation have
/// already been ruled out.
#[derive(Clone, Debug, PartialEq)]
pub struct TokenRow {
    pub id: String,
    pub kind: TokenKind,
    pub principal: Principal,
    pub label: Option<String>,
    pub last_used_at: Option<i64>,
}

pub struct AuthStore {
    pool: SqlitePool,
}

impl AuthStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // --- users ---

    pub async fn user_count(&self) -> Result<i64, String> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM auth_users")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        row.try_get::<i64, _>("n").map_err(|e| e.to_string())
    }

    pub async fn get_user(&self, username: &str) -> Result<Option<UserRow>, String> {
        let row = sqlx::query(
            "SELECT id, username, password_hash, password_is_generated \
             FROM auth_users WHERE username = ?",
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_user).transpose()
    }

    /// Insert the account. The UNIQUE index on `username` plus this crate's
    /// single-account rule mean a second call errs, which is the point.
    pub async fn create_user(
        &self,
        username: &str,
        password_hash: &str,
        generated: bool,
        now: i64,
    ) -> Result<i64, String> {
        if self.user_count().await? > 0 {
            return Err("an account already exists".to_string());
        }
        let res = sqlx::query(
            "INSERT INTO auth_users \
             (username, password_hash, password_is_generated, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(username)
        .bind(password_hash)
        .bind(i64::from(generated))
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(res.last_insert_rowid())
    }

    /// Replace the password. Always clears `password_is_generated`: the only
    /// way to reach here is a deliberate change.
    pub async fn set_password(
        &self,
        username: &str,
        password_hash: &str,
        now: i64,
    ) -> Result<(), String> {
        sqlx::query(
            "UPDATE auth_users SET password_hash = ?, password_is_generated = 0, \
             updated_at = ? WHERE username = ?",
        )
        .bind(password_hash)
        .bind(now)
        .bind(username)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    // --- tokens ---

    #[allow(clippy::too_many_arguments)]
    pub async fn insert_token(
        &self,
        id: &str,
        kind: TokenKind,
        principal: &Principal,
        hash: &[u8],
        label: Option<&str>,
        chain_id: Option<&str>,
        expires_at: Option<i64>,
        now: i64,
    ) -> Result<(), String> {
        sqlx::query(
            "INSERT INTO auth_tokens \
             (id, kind, principal, token_hash, label, chain_id, expires_at, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(kind.as_db())
        .bind(principal.to_db())
        .bind(hash)
        .bind(label)
        .bind(chain_id)
        .bind(expires_at)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// The live token with this hash, or `None` when absent, revoked, or
    /// expired. Expiry is compared in SQL, which is why these columns are
    /// INTEGER.
    pub async fn lookup_token(&self, hash: &[u8], now: i64) -> Result<Option<TokenRow>, String> {
        let row = sqlx::query(
            "SELECT id, kind, principal, label, last_used_at FROM auth_tokens \
             WHERE token_hash = ? AND revoked_at IS NULL \
             AND (expires_at IS NULL OR expires_at > ?)",
        )
        .bind(hash)
        .bind(now)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_token).transpose()
    }

    pub async fn revoke_token(&self, id: &str, now: i64) -> Result<(), String> {
        sqlx::query("UPDATE auth_tokens SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL")
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Revoke every live token of one kind for a principal, optionally sparing
    /// one id — how a password change logs out every browser but the one that
    /// asked for it.
    pub async fn revoke_kind_for_principal(
        &self,
        principal: &Principal,
        kind: TokenKind,
        except_id: Option<&str>,
        now: i64,
    ) -> Result<(), String> {
        sqlx::query(
            "UPDATE auth_tokens SET revoked_at = ? \
             WHERE principal = ? AND kind = ? AND revoked_at IS NULL \
             AND (? IS NULL OR id <> ?)",
        )
        .bind(now)
        .bind(principal.to_db())
        .bind(kind.as_db())
        .bind(except_id)
        .bind(except_id)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Record use, at most once a minute per token. Returns whether a write
    /// happened. A live SSE stream would otherwise turn every request into a
    /// database write for no information gain.
    pub async fn touch_token(
        &self,
        id: &str,
        last_used_at: Option<i64>,
        now: i64,
    ) -> Result<bool, String> {
        if last_used_at.is_some_and(|t| now - t < 60) {
            return Ok(false);
        }
        sqlx::query("UPDATE auth_tokens SET last_used_at = ? WHERE id = ?")
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(true)
    }
}

fn row_to_user(row: &SqliteRow) -> Result<UserRow, String> {
    Ok(UserRow {
        id: row.try_get("id").map_err(|e| e.to_string())?,
        username: row.try_get("username").map_err(|e| e.to_string())?,
        password_hash: row.try_get("password_hash").map_err(|e| e.to_string())?,
        password_is_generated: row
            .try_get::<i64, _>("password_is_generated")
            .map_err(|e| e.to_string())?
            != 0,
    })
}

fn row_to_token(row: &SqliteRow) -> Result<TokenRow, String> {
    let kind: String = row.try_get("kind").map_err(|e| e.to_string())?;
    let principal: String = row.try_get("principal").map_err(|e| e.to_string())?;
    Ok(TokenRow {
        id: row.try_get("id").map_err(|e| e.to_string())?,
        kind: TokenKind::from_db(&kind).ok_or_else(|| format!("unknown token kind {kind:?}"))?,
        principal: Principal::from_db(&principal)?,
        label: row.try_get("label").map_err(|e| e.to_string())?,
        last_used_at: row.try_get("last_used_at").map_err(|e| e.to_string())?,
    })
}
```

In `server/src/auth/mod.rs`, add below the existing `mod token;`:

```rust
mod store;

pub use store::{AuthStore, TokenRow, UserRow};
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p horsie-server auth::store`
Expected: PASS, 7 tests.

- [ ] **Step 6: Commit**

```bash
git add server/migrations/0014_auth.sql server/src/auth/
git commit -m "feat(auth): schema and store for users and tokens"
```

---

### Task 3: Password hashing and failure throttling

**Files:**
- Create: `server/src/auth/password.rs`
- Create: `server/src/auth/throttle.rs`
- Modify: `server/src/auth/mod.rs`
- Modify: `Cargo.toml`, `server/Cargo.toml` (add `argon2`)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `password::hash(plain: &str) -> Result<String, String>`; `password::verify(plain: &str, phc: &str) -> bool`; `password::generate_initial() -> String`; `Throttle::new()`, `Throttle::delay(&self) -> Duration`, `Throttle::record_failure(&self)`, `Throttle::record_success(&self)`.

The throttle deliberately delays failures rather than locking an account. Behind a reverse proxy every request shares one source address, so per-IP lockout would let an attacker deny the admin their own server by guessing wrong on purpose.

- [ ] **Step 1: Add the `argon2` dependency**

Root `Cargo.toml`, `[workspace.dependencies]`, after the `rand` line added in Task 1:

```toml
argon2           = "0.5"
```

`server/Cargo.toml`, `[dependencies]`, after `rand`:

```toml
argon2            = { workspace = true }
```

- [ ] **Step 2: Write the failing tests**

Create `server/src/auth/password.rs` with only:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_hashed_password_verifies_and_a_wrong_one_does_not() {
        let phc = hash("correct horse").unwrap();
        assert!(phc.starts_with("$argon2id$"), "{phc}");
        assert!(verify("correct horse", &phc));
        assert!(!verify("Correct Horse", &phc));
        assert!(!verify("", &phc));
    }

    #[test]
    fn the_same_password_hashes_differently_each_time() {
        assert_ne!(hash("x").unwrap(), hash("x").unwrap());
    }

    #[test]
    fn verify_rejects_a_malformed_hash_instead_of_panicking() {
        assert!(!verify("x", "not-a-phc-string"));
        assert!(!verify("x", ""));
    }

    #[test]
    fn the_generated_initial_password_is_long_and_unpredictable() {
        let a = generate_initial();
        assert_eq!(a.chars().count(), 24);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(a, generate_initial());
    }
}
```

Create `server/src/auth/throttle.rs` with only:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn the_first_three_failures_are_not_delayed() {
        let t = Throttle::new();
        for _ in 0..3 {
            assert_eq!(t.delay(), Duration::ZERO);
            t.record_failure();
        }
        assert_eq!(t.delay(), Duration::from_secs(2));
    }

    #[test]
    fn the_delay_doubles_and_stops_at_thirty_seconds() {
        let t = Throttle::new();
        for _ in 0..3 {
            t.record_failure();
        }
        let seen: Vec<u64> = (0..7)
            .map(|_| {
                let d = t.delay().as_secs();
                t.record_failure();
                d
            })
            .collect();
        assert_eq!(seen, vec![2, 4, 8, 16, 30, 30, 30]);
    }

    #[test]
    fn a_success_clears_the_delay() {
        let t = Throttle::new();
        for _ in 0..6 {
            t.record_failure();
        }
        assert!(t.delay() > Duration::ZERO);
        t.record_success();
        assert_eq!(t.delay(), Duration::ZERO);
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p horsie-server auth::password auth::throttle`
Expected: FAIL — neither module is declared, and `hash`/`Throttle` are undefined.

- [ ] **Step 4: Write the implementations**

Prepend to `server/src/auth/password.rs`:

```rust
//! Argon2id password hashing for the single admin account.
//!
//! Passwords, unlike the 256-bit token secrets in `token.rs`, are guessable,
//! so they get a deliberately slow KDF rather than a plain hash.

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand::Rng;

/// Alphanumeric only: the generated password is read off a terminal and typed
/// into a browser, so ambiguity and shell quoting are the real risks, not the
/// handful of bits a wider alphabet would add to an already-142-bit secret.
const INITIAL_PASSWORD_LEN: usize = 24;

pub fn hash(plain: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(plain.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

/// Constant-time verification. A stored hash we cannot parse is a `false`, not
/// an error: there is no caller who could do anything useful with the
/// distinction, and returning `false` fails closed.
pub fn verify(plain: &str, phc: &str) -> bool {
    match PasswordHash::new(phc) {
        Ok(parsed) => Argon2::default()
            .verify_password(plain.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

pub fn generate_initial() -> String {
    let mut rng = rand::thread_rng();
    (0..INITIAL_PASSWORD_LEN)
        .map(|_| {
            const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
            char::from(ALPHABET[rng.gen_range(0..ALPHABET.len())])
        })
        .collect()
}
```

Prepend to `server/src/auth/throttle.rs`:

```rust
//! Throttles password guessing by *delaying failures*, never by locking the
//! account.
//!
//! Per-IP lockout is the textbook answer and is wrong here: behind a reverse
//! proxy (Caddy, fly.io) every request arrives from the proxy's address, so one
//! bucket covers everybody and an attacker denies the admin their own server by
//! guessing wrong on purpose. Delaying failures throttles guessing at the same
//! rate without ever refusing the person who knows the password — a correct
//! password is answered immediately no matter how many failures preceded it.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

const FREE_ATTEMPTS: u32 = 3;
const MAX_DELAY_SECS: u64 = 30;

pub struct Throttle {
    consecutive_failures: AtomicU32,
}

impl Default for Throttle {
    fn default() -> Self {
        Self::new()
    }
}

impl Throttle {
    pub fn new() -> Self {
        Self {
            consecutive_failures: AtomicU32::new(0),
        }
    }

    /// How long to wait before answering the *next* failed attempt.
    pub fn delay(&self) -> Duration {
        let n = self.consecutive_failures.load(Ordering::Relaxed);
        if n < FREE_ATTEMPTS {
            return Duration::ZERO;
        }
        let steps = n - FREE_ATTEMPTS;
        let secs = 2u64
            .checked_pow(steps + 1)
            .unwrap_or(MAX_DELAY_SECS)
            .min(MAX_DELAY_SECS);
        Duration::from_secs(secs)
    }

    pub fn record_failure(&self) {
        // Saturating: an attacker running forever must not wrap the counter
        // back into the free-attempt range.
        let _ = self
            .consecutive_failures
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_add(1))
            });
    }

    pub fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
    }
}
```

In `server/src/auth/mod.rs`, add:

```rust
pub mod password;
mod throttle;

pub use throttle::Throttle;
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p horsie-server auth::password auth::throttle`
Expected: PASS, 7 tests.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock server/Cargo.toml server/src/auth/
git commit -m "feat(auth): argon2 passwords and failure-delay throttling"
```

---

### Task 4: AuthService — bootstrap, login, verification

**Files:**
- Create: `server/src/auth/service.rs`
- Modify: `server/src/auth/mod.rs`

**Interfaces:**
- Consumes: `AuthStore`, `TokenRow`, `password::*`, `Throttle`, `Principal`, `TokenKind`, `generate`, `hash_secret`, `parse` from Tasks 1–3.
- Produces: `AuthService::new(store: AuthStore, deps: AuthDeps) -> AuthService` where `AuthDeps { enabled: bool, state_dir: PathBuf }`; `bootstrap(&self) -> Result<Option<String>, String>`; `enabled(&self) -> bool`; `login(&self, password: &str) -> Result<String, LoginError>`; `logout(&self, secret: &str) -> Result<(), String>`; `change_password(&self, current: &str, new: &str, active_secret: &str) -> Result<(), LoginError>`; `verify(&self, secret: &str) -> Result<Option<VerifiedToken>, String>` where `VerifiedToken { principal: Principal, kind: TokenKind, token_id: String }`; `must_change_password(&self) -> Result<bool, String>`.

`ADMIN_USERNAME` is `"admin"`. The single-account rule means `login` takes no username.

- [ ] **Step 1: Write the failing test**

Create `server/src/auth/service.rs` with only:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// File-backed, not `sqlite::memory:` — see the note in `store.rs`'s tests.
    /// The temp dir doubles as the service's state dir, so the generated
    /// password file lands somewhere the test can read.
    async fn service(tmp: &tempfile::TempDir, enabled: bool) -> AuthService {
        use std::str::FromStr;
        let url = format!("sqlite://{}/t.db", tmp.path().display());
        let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
            .unwrap()
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePool::connect_with(opts).await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        AuthService::new(
            AuthStore::new(pool),
            AuthDeps {
                enabled,
                state_dir: tmp.path().to_path_buf(),
            },
        )
    }

    #[tokio::test]
    async fn bootstrap_generates_a_password_once_and_records_it() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = service(&tmp, true).await;

        let generated = svc.bootstrap().await.unwrap().expect("a password");
        assert_eq!(generated.chars().count(), 24);
        let file = tmp.path().join("initial-admin-password");
        assert_eq!(std::fs::read_to_string(&file).unwrap().trim(), generated);
        assert!(svc.must_change_password().await.unwrap());

        // Second boot: no new password, file untouched.
        assert!(svc.bootstrap().await.unwrap().is_none());
        assert_eq!(std::fs::read_to_string(&file).unwrap().trim(), generated);
    }

    #[tokio::test]
    async fn bootstrap_does_nothing_when_auth_is_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = service(&tmp, false).await;
        assert!(svc.bootstrap().await.unwrap().is_none());
        assert!(!tmp.path().join("initial-admin-password").exists());
    }

    #[tokio::test]
    async fn login_issues_a_web_token_that_verifies() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = service(&tmp, true).await;
        let pw = svc.bootstrap().await.unwrap().unwrap();

        let secret = svc.login(&pw).await.unwrap();
        assert!(secret.starts_with("hsk_web_"));

        let v = svc.verify(&secret).await.unwrap().expect("verifies");
        assert_eq!(v.kind, TokenKind::Web);
        assert_eq!(v.principal, Principal::User(1));

        // Logout revokes it.
        svc.logout(&secret).await.unwrap();
        assert!(svc.verify(&secret).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_wrong_password_is_rejected_and_a_correct_one_is_never_delayed() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = service(&tmp, true).await;
        let pw = svc.bootstrap().await.unwrap().unwrap();

        // Exactly three: the fourth failure would sleep, and a unit test that
        // sits for two seconds to prove the sleep exists is a bad trade —
        // `throttle.rs` already covers the arithmetic.
        for _ in 0..3 {
            assert!(matches!(svc.login("wrong").await, Err(LoginError::BadCredentials)));
        }
        // Failures are now delayed, but the correct password still answers at once.
        assert!(svc.delay_before_failure() > std::time::Duration::ZERO);
        let started = std::time::Instant::now();
        svc.login(&pw).await.unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        // ...and success cleared the delay.
        assert_eq!(svc.delay_before_failure(), std::time::Duration::ZERO);
    }

    #[tokio::test]
    async fn verify_returns_none_for_junk_and_for_a_disabled_service() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = service(&tmp, true).await;
        svc.bootstrap().await.unwrap();
        assert!(svc.verify("not-a-token").await.unwrap().is_none());
        assert!(svc.verify("hsk_web_deadbeef").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn changing_the_password_logs_out_other_browsers_and_deletes_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = service(&tmp, true).await;
        let pw = svc.bootstrap().await.unwrap().unwrap();

        let keep = svc.login(&pw).await.unwrap();
        let other = svc.login(&pw).await.unwrap();

        svc.change_password(&pw, "a-new-password", &keep).await.unwrap();

        assert!(svc.verify(&keep).await.unwrap().is_some(), "the caller stays logged in");
        assert!(svc.verify(&other).await.unwrap().is_none(), "other browsers are logged out");
        assert!(!tmp.path().join("initial-admin-password").exists());
        assert!(!svc.must_change_password().await.unwrap());
        assert!(svc.login("a-new-password").await.is_ok());
        assert!(matches!(svc.login(&pw).await, Err(LoginError::BadCredentials)));
    }

    #[tokio::test]
    async fn changing_the_password_requires_the_current_one() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = service(&tmp, true).await;
        let pw = svc.bootstrap().await.unwrap().unwrap();
        let session = svc.login(&pw).await.unwrap();
        assert!(matches!(
            svc.change_password("nope", "whatever12", &session).await,
            Err(LoginError::BadCredentials)
        ));
        assert!(matches!(
            svc.change_password(&pw, "short", &session).await,
            Err(LoginError::WeakPassword(_))
        ));
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p horsie-server auth::service`
Expected: FAIL — `AuthService` is undefined.

- [ ] **Step 3: Write the implementation**

Prepend to `server/src/auth/service.rs`:

```rust
//! Authentication policy: first-boot bootstrap, login, logout, password
//! change, and credential verification. `store.rs` holds the rows; everything
//! that decides *whether* something is allowed lives here.

use crate::auth::store::AuthStore;
use crate::auth::{Principal, Throttle, TokenKind, generate, hash_secret, parse, password};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The one account. Stored as a column rather than assumed, so adding users
/// later is a service change, not a schema migration.
pub const ADMIN_USERNAME: &str = "admin";

/// Browser sessions last 30 days.
const WEB_TOKEN_TTL_SECS: i64 = 30 * 24 * 60 * 60;

/// Short enough to type from a terminal, long enough that the throttle in
/// `throttle.rs` is the binding constraint on guessing.
const MIN_PASSWORD_LEN: usize = 8;

const INITIAL_PASSWORD_FILE: &str = "initial-admin-password";

#[derive(Debug)]
pub enum LoginError {
    BadCredentials,
    WeakPassword(String),
    Internal(String),
}

/// Deployment inputs the host supplies.
pub struct AuthDeps {
    pub enabled: bool,
    /// Where the first-boot password file is written.
    pub state_dir: PathBuf,
}

/// A credential that checked out.
#[derive(Clone, Debug, PartialEq)]
pub struct VerifiedToken {
    pub principal: Principal,
    pub kind: TokenKind,
    pub token_id: String,
}

pub struct AuthService {
    store: AuthStore,
    enabled: bool,
    state_dir: PathBuf,
    throttle: Throttle,
}

impl AuthService {
    pub fn new(store: AuthStore, deps: AuthDeps) -> Self {
        Self {
            store,
            enabled: deps.enabled,
            state_dir: deps.state_dir,
            throttle: Throttle::new(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Create the admin account if there is none, returning the generated
    /// password so the host can print it. Also writes it to
    /// `<state_dir>/initial-admin-password` (0600): an operator whose
    /// container logs have rotated would otherwise be locked out of their own
    /// deployment with no recovery short of editing SQLite.
    pub async fn bootstrap(&self) -> Result<Option<String>, String> {
        if !self.enabled || self.store.user_count().await? > 0 {
            return Ok(None);
        }
        let plain = password::generate_initial();
        let hash = password::hash(&plain)?;
        self.store
            .create_user(ADMIN_USERNAME, &hash, true, now_secs())
            .await?;
        write_secret_file(&self.state_dir.join(INITIAL_PASSWORD_FILE), &plain)?;
        Ok(Some(plain))
    }

    pub async fn must_change_password(&self) -> Result<bool, String> {
        Ok(self
            .store
            .get_user(ADMIN_USERNAME)
            .await?
            .is_some_and(|u| u.password_is_generated))
    }

    /// How long the next *failed* login will be held before answering. Exposed
    /// for tests and for the handler, which sleeps for it.
    pub fn delay_before_failure(&self) -> Duration {
        self.throttle.delay()
    }

    /// Verify the password and mint a browser session token, returning the
    /// secret to set as a cookie. A correct password is answered immediately
    /// however many failures preceded it; only failures are delayed.
    pub async fn login(&self, plain: &str) -> Result<String, LoginError> {
        let user = self
            .store
            .get_user(ADMIN_USERNAME)
            .await
            .map_err(LoginError::Internal)?
            .ok_or(LoginError::BadCredentials)?;

        if !password::verify(plain, &user.password_hash) {
            let delay = self.throttle.delay();
            self.throttle.record_failure();
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            return Err(LoginError::BadCredentials);
        }
        self.throttle.record_success();

        let now = now_secs();
        let token = generate(TokenKind::Web);
        let id = uuid::Uuid::new_v4().to_string();
        self.store
            .insert_token(
                &id,
                TokenKind::Web,
                &Principal::User(user.id),
                &token.hash,
                None,
                None,
                Some(now + WEB_TOKEN_TTL_SECS),
                now,
            )
            .await
            .map_err(LoginError::Internal)?;
        Ok(token.secret)
    }

    /// Revoke the presented session. Unknown or already-dead secrets are a
    /// no-op: logging out twice is not an error worth surfacing.
    pub async fn logout(&self, secret: &str) -> Result<(), String> {
        if let Some(v) = self.verify(secret).await? {
            self.store.revoke_token(&v.token_id, now_secs()).await?;
        }
        Ok(())
    }

    /// Replace the password, revoking every other browser session. The caller's
    /// own session survives — being logged out of the tab you just used to
    /// change your password is a bug, not security.
    pub async fn change_password(
        &self,
        current: &str,
        new: &str,
        active_secret: &str,
    ) -> Result<(), LoginError> {
        if new.chars().count() < MIN_PASSWORD_LEN {
            return Err(LoginError::WeakPassword(format!(
                "password must be at least {MIN_PASSWORD_LEN} characters"
            )));
        }
        let user = self
            .store
            .get_user(ADMIN_USERNAME)
            .await
            .map_err(LoginError::Internal)?
            .ok_or(LoginError::BadCredentials)?;
        if !password::verify(current, &user.password_hash) {
            return Err(LoginError::BadCredentials);
        }

        let hash = password::hash(new).map_err(LoginError::Internal)?;
        let now = now_secs();
        self.store
            .set_password(ADMIN_USERNAME, &hash, now)
            .await
            .map_err(LoginError::Internal)?;

        let keep = self
            .verify(active_secret)
            .await
            .map_err(LoginError::Internal)?
            .map(|v| v.token_id);
        self.store
            .revoke_kind_for_principal(
                &Principal::User(user.id),
                TokenKind::Web,
                keep.as_deref(),
                now,
            )
            .await
            .map_err(LoginError::Internal)?;

        // The generated password is no longer in play; remove the recovery
        // file so a stale secret does not sit on disk.
        let file = self.state_dir.join(INITIAL_PASSWORD_FILE);
        if file.exists() && let Err(e) = std::fs::remove_file(&file) {
            tracing::warn!(error = %e, path = %file.display(), "could not remove the initial password file");
        }
        Ok(())
    }

    /// Resolve a presented secret. `None` means "not a live credential" — junk,
    /// unknown, revoked, or expired are all the same answer to a caller.
    pub async fn verify(&self, secret: &str) -> Result<Option<VerifiedToken>, String> {
        // A secret that does not even claim one of our kinds never reaches the
        // database.
        if parse(secret).is_none() {
            return Ok(None);
        }
        let now = now_secs();
        let Some(row) = self.store.lookup_token(&hash_secret(secret), now).await? else {
            return Ok(None);
        };
        if let Err(e) = self.store.touch_token(&row.id, row.last_used_at, now).await {
            tracing::warn!(error = %e, "recording token use failed");
        }
        Ok(Some(VerifiedToken {
            principal: row.principal,
            kind: row.kind,
            token_id: row.id,
        }))
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Write owner-readable-only on unix; elsewhere fall back to a plain write.
fn write_secret_file(path: &std::path::Path, contents: &str) -> Result<(), String> {
    std::fs::write(path, format!("{contents}\n")).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}
```

In `server/src/auth/mod.rs`, add:

```rust
mod service;

pub use service::{ADMIN_USERNAME, AuthDeps, AuthService, LoginError, VerifiedToken};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p horsie-server auth::`
Expected: PASS — all auth module tests, 27 total.

- [ ] **Step 5: Commit**

```bash
git add server/src/auth/
git commit -m "feat(auth): bootstrap, login, logout, and password change"
```

---

### Task 5: Wire types

**Files:**
- Create: `models/fluorite/auth.fl`
- Modify: `models/src/lib.rs`
- Modify: `clients/web/package.json` (the `generate-types` script)
- Modify: `clients/ts/package.json` (the `generate-types` script)
- Test: `models/src/lib.rs` (a serde round-trip test module)

**Interfaces:**
- Consumes: nothing.
- Produces: `horsie_models::auth::{AuthStatus, LoginRequest, PasswordChangeRequest}`, serialized camelCase.

- [ ] **Step 1: Write the failing test**

Add to the bottom of `models/src/lib.rs`:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod auth_wire_tests {
    use crate::auth::{AuthStatus, LoginRequest, PasswordChangeRequest};

    #[test]
    fn auth_status_is_camel_case_on_the_wire() {
        let json = serde_json::to_string(&AuthStatus {
            enabled: true,
            authenticated: false,
            must_change_password: false,
        })
        .unwrap();
        assert!(json.contains("\"mustChangePassword\""), "{json}");
        assert!(!json.contains("must_change_password"), "{json}");
    }

    #[test]
    fn login_and_password_change_deserialize_from_camel_case() {
        let req: LoginRequest = serde_json::from_str(r#"{"password":"p"}"#).unwrap();
        assert_eq!(req.password, "p");
        let req: PasswordChangeRequest =
            serde_json::from_str(r#"{"currentPassword":"a","newPassword":"b"}"#).unwrap();
        assert_eq!(req.current_password, "a");
        assert_eq!(req.new_password, "b");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p horsie-models auth_wire`
Expected: FAIL — `crate::auth` does not exist.

- [ ] **Step 3: Write the schema and wire it up**

Create `models/fluorite/auth.fl`:

```
/// Wire contracts for server authentication. Sub-project A covers the browser
/// session only: the CLI device flow and vendor agent tokens add their own
/// types to this package later.
package auth;

/// What the UI needs to decide between rendering the app and rendering a login
/// page. `must_change_password` is only ever true for an authenticated caller
/// — telling an anonymous one that a deployment still has its first-boot
/// password just tells an attacker where to aim.
struct AuthStatus {
    /// False when the deployment runs with authentication turned off, in which
    /// case the UI shows no login surface at all.
    enabled: bool,
    authenticated: bool,
    must_change_password: bool,
}

/// There is exactly one account, so there is no username to send.
struct LoginRequest {
    password: String,
}

struct PasswordChangeRequest {
    current_password: String,
    new_password: String,
}
```

In `models/src/lib.rs`, add alongside the other module declarations, keeping alphabetical order (before the `capabilities` module):

```rust
#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod auth {
    include!(concat!(env!("OUT_DIR"), "/auth/mod.rs"));
}
```

In `clients/web/package.json`, append ` ../../models/fluorite/auth.fl` to the input list of the `generate-types` script, immediately after `memory.fl`. Do the same in `clients/ts/package.json`.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p horsie-models auth_wire`
Expected: PASS, 2 tests.

- [ ] **Step 5: Regenerate the TypeScript and confirm it type-checks**

Run: `cd clients/web && bun install && bun run generate-types && bun run typecheck`
Expected: `src/generated/auth/` now holds `authStatus.ts`, `loginRequest.ts`, `passwordChangeRequest.ts`, and typecheck passes.

- [ ] **Step 6: Commit**

```bash
git add models/fluorite/auth.fl models/src/lib.rs clients/web/package.json clients/ts/package.json clients/web/src/generated
git commit -m "feat(auth): fluorite wire types for login and status"
```

---

### Task 6: HTTP handlers and the auth middleware

**Files:**
- Create: `server/src/http/auth.rs`
- Modify: `server/src/http/mod.rs` (AppState field, routes, middleware layer, tests)

**Interfaces:**
- Consumes: `AuthService`, `LoginError`, `VerifiedToken`, `Principal`, `TokenKind` from Tasks 1–4; `horsie_models::auth::*` from Task 5.
- Produces: `AppState.auth: Arc<AuthService>`; handlers `status`, `login`, `logout`, `change_password`; `require_auth` middleware; `COOKIE_NAME`.

The middleware runs on the `/api` router only. The SPA shell and its assets are never guarded — the app has to load in order to render a login page, and the bundle holds no secrets.

- [ ] **Step 1: Write the failing tests**

In `server/src/http/mod.rs`, inside the existing `mod tests`, add a state builder that turns auth on. Place it directly after the existing `test_state` function:

```rust
    /// `test_state` with authentication enabled and the admin account
    /// bootstrapped. Returns the state and the generated password.
    ///
    /// Opens a second pool on the same file `test_state` already created and
    /// migrated, rather than reaching through the `Arc<dyn ConfigStore>` trait
    /// object for its pool — the auth tables live in that database, but auth
    /// has no business widening the config trait to get at them.
    async fn auth_state(tmp: &tempfile::TempDir) -> (AppState, String) {
        use std::str::FromStr;
        let mut state = test_state(tmp).await;
        let url = format!("sqlite://{}/config.db", tmp.path().display());
        let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
            .unwrap()
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePool::connect_with(opts).await.unwrap();
        let svc = Arc::new(crate::auth::AuthService::new(
            crate::auth::AuthStore::new(pool),
            crate::auth::AuthDeps {
                enabled: true,
                state_dir: tmp.path().to_path_buf(),
            },
        ));
        let password = svc.bootstrap().await.unwrap().expect("bootstrapped");
        state.auth = svc;
        (state, password)
    }

    /// The `Set-Cookie` session value from a login response.
    fn session_cookie(res: &axum::response::Response) -> String {
        let raw = res
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .expect("set-cookie")
            .to_str()
            .unwrap();
        raw.split(';')
            .next()
            .unwrap()
            .trim_start_matches("horsie_session=")
            .to_string()
    }

    fn get_with_cookie(uri: &str, cookie: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header("cookie", format!("horsie_session={cookie}"))
            .body(Body::empty())
            .unwrap()
    }
```

Then add these tests at the end of the module:

```rust
    #[tokio::test]
    async fn with_auth_disabled_everything_is_reachable() {
        let tmp = tempfile::tempdir().unwrap();
        let app = app(test_state(&tmp).await);
        let res = app.clone().oneshot(get("/api/sessions")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let res = app.oneshot(get("/api/auth/status")).await.unwrap();
        let status: horsie_models::auth::AuthStatus = read_json(res).await;
        assert!(!status.enabled);
        assert!(!status.authenticated);
    }

    #[tokio::test]
    async fn with_auth_enabled_the_api_is_closed_but_health_and_status_are_not() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, _pw) = auth_state(&tmp).await;
        let app = app(state);

        let res = app.clone().oneshot(get("/api/sessions")).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        let res = app.clone().oneshot(get("/api/health")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let res = app.oneshot(get("/api/auth/status")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let status: horsie_models::auth::AuthStatus = read_json(res).await;
        assert!(status.enabled);
        assert!(!status.authenticated);
        // Never leaked to an anonymous caller.
        assert!(!status.must_change_password);
    }

    #[tokio::test]
    async fn login_sets_a_cookie_that_opens_the_api_and_logout_closes_it_again() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, pw) = auth_state(&tmp).await;
        let app = app(state);

        // Wrong password.
        let res = app
            .clone()
            .oneshot(post_json("/api/auth/login", &serde_json::json!({"password": "nope"})))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // Right password.
        let res = app
            .clone()
            .oneshot(post_json("/api/auth/login", &serde_json::json!({"password": pw})))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let raw_cookie = res
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(raw_cookie.contains("HttpOnly"), "{raw_cookie}");
        assert!(raw_cookie.contains("SameSite=Lax"), "{raw_cookie}");
        assert!(raw_cookie.contains("Path=/"), "{raw_cookie}");
        let cookie = session_cookie(&res);

        // The cookie opens the API.
        let res = app
            .clone()
            .oneshot(get_with_cookie("/api/sessions", &cookie))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // ...and reports an authenticated status that admits the generated password.
        let res = app
            .clone()
            .oneshot(get_with_cookie("/api/auth/status", &cookie))
            .await
            .unwrap();
        let status: horsie_models::auth::AuthStatus = read_json(res).await;
        assert!(status.authenticated);
        assert!(status.must_change_password);

        // Logout revokes it.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/logout")
                    .header("cookie", format!("horsie_session={cookie}"))
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res = app
            .oneshot(get_with_cookie("/api/sessions", &cookie))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_bearer_token_is_accepted_and_a_bogus_one_is_not() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, pw) = auth_state(&tmp).await;
        let secret = state.auth.login(&pw).await.unwrap();
        let app = app(state);

        let req = Request::builder()
            .uri("/api/sessions")
            .header("authorization", format!("Bearer {secret}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::OK);

        let req = Request::builder()
            .uri("/api/sessions")
            .header("authorization", "Bearer hsk_web_notarealtoken")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(req).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn changing_the_password_requires_the_current_one_and_then_works() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, pw) = auth_state(&tmp).await;
        let app = app(state);

        let res = app
            .clone()
            .oneshot(post_json("/api/auth/login", &serde_json::json!({"password": pw})))
            .await
            .unwrap();
        let cookie = session_cookie(&res);

        let change = |body: serde_json::Value, cookie: String| {
            Request::builder()
                .method("POST")
                .uri("/api/auth/password")
                .header("content-type", "application/json")
                .header("cookie", format!("horsie_session={cookie}"))
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap()
        };

        let res = app
            .clone()
            .oneshot(change(
                serde_json::json!({"currentPassword": "wrong", "newPassword": "a-good-one"}),
                cookie.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        let res = app
            .clone()
            .oneshot(change(
                serde_json::json!({"currentPassword": pw, "newPassword": "short"}),
                cookie.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let res = app
            .clone()
            .oneshot(change(
                serde_json::json!({"currentPassword": pw, "newPassword": "a-good-one"}),
                cookie.clone(),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // The caller's own session survives, and the flag has cleared.
        let res = app
            .oneshot(get_with_cookie("/api/auth/status", &cookie))
            .await
            .unwrap();
        let status: horsie_models::auth::AuthStatus = read_json(res).await;
        assert!(status.authenticated);
        assert!(!status.must_change_password);
    }

    #[tokio::test]
    async fn the_spa_shell_is_reachable_without_a_credential() {
        let tmp = tempfile::tempdir().unwrap();
        let web = tmp.path().join("web");
        std::fs::create_dir_all(web.join("assets")).unwrap();
        std::fs::write(web.join("index.html"), "<html>app</html>").unwrap();
        std::fs::write(web.join("favicon.svg"), "<svg/>").unwrap();

        let (mut state, _pw) = auth_state(&tmp).await;
        state.web_dir = Some(web);
        let app = app(state);

        let res = app.oneshot(get("/settings")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p horsie-server http::tests`
Expected: FAIL — `AppState` has no `auth` field, `pool_for_tests` does not exist, and `/api/auth/*` routes are missing.

- [ ] **Step 3: Write the handlers**

Create `server/src/http/auth.rs`:

```rust
//! `/api/auth/*` — the browser's login surface, plus the middleware every other
//! `/api` route sits behind.
//!
//! The browser authenticates by cookie rather than a header because it has no
//! choice: both event streams use the native `EventSource`, which cannot set
//! headers. Non-browser callers (the CLI, vendor agents) send
//! `Authorization: Bearer` and are accepted by the same code path.

use crate::auth::{LoginError, Principal};
use crate::http::AppState;
use crate::http::error::Api;
use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use horsie_models::auth::{AuthStatus, LoginRequest, PasswordChangeRequest};

pub const COOKIE_NAME: &str = "horsie_session";

/// Paths reachable without a credential. `/api/auth/status` and `/api/auth/login`
/// are how a caller becomes authenticated in the first place; `/api/health` is a
/// liveness probe; plugin artifacts carry their own capability token and are
/// fetched by runtimes that have no session cookie.
fn is_public(path: &str) -> bool {
    path == "/api/health"
        || path == "/api/auth/status"
        || path == "/api/auth/login"
        || path.starts_with("/api/plugin-artifacts/")
}

/// Resolve a credential into a [`Principal`] and put it in the request
/// extensions, or answer `401`. With auth disabled every request is
/// `Principal::Anonymous`, which is today's behaviour exactly.
pub async fn require_auth(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    if !state.auth.enabled() {
        req.extensions_mut().insert(Principal::Anonymous);
        return next.run(req).await;
    }
    if is_public(req.uri().path()) {
        return next.run(req).await;
    }
    let Some(secret) = credential(req.headers()) else {
        return unauthorized();
    };
    match state.auth.verify(&secret).await {
        Ok(Some(v)) => {
            req.extensions_mut().insert(v.principal);
            next.run(req).await
        }
        Ok(None) => unauthorized(),
        Err(e) => {
            tracing::error!(error = %e, "verifying a credential failed");
            Api::internal("could not verify the credential").into_response()
        }
    }
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(horsie_models::session_api::ApiError {
            code: "unauthorized".to_string(),
            message: "authentication required".to_string(),
        }),
    )
        .into_response()
}

/// The bearer header if present, else the session cookie.
fn credential(headers: &HeaderMap) -> Option<String> {
    if let Some(bearer) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        return Some(bearer.to_string());
    }
    cookie_value(headers, COOKIE_NAME)
}

/// Pull one cookie out of the `Cookie` header. Hand-rolled rather than pulling
/// in a cookie crate: one name, no attributes to parse on the request side.
fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

/// `GET /api/auth/status` — reachable unauthenticated, since it is what tells
/// the UI to render a login page.
pub async fn status(State(state): State<AppState>, headers: HeaderMap) -> Json<AuthStatus> {
    if !state.auth.enabled() {
        return Json(AuthStatus {
            enabled: false,
            authenticated: false,
            must_change_password: false,
        });
    }
    let authenticated = match credential(&headers) {
        Some(secret) => matches!(state.auth.verify(&secret).await, Ok(Some(_))),
        None => false,
    };
    // Only ever disclosed to someone already inside.
    let must_change_password =
        authenticated && state.auth.must_change_password().await.unwrap_or(false);
    Json(AuthStatus {
        enabled: true,
        authenticated,
        must_change_password,
    })
}

/// `POST /api/auth/login`
///
/// `Json` stays last: axum requires the body-consuming extractor in final
/// position.
pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> Result<Response, Api> {
    let secret = state.auth.login(&body.password).await.map_err(to_api)?;
    let must_change_password = state.auth.must_change_password().await.unwrap_or(false);
    let mut res = Json(AuthStatus {
        enabled: true,
        authenticated: true,
        must_change_password,
    })
    .into_response();
    // `Secure` only when the request actually arrived over TLS. Setting it
    // unconditionally would make the cookie unusable on a plain-HTTP localhost
    // deployment, which is the default self-host shape.
    let secure = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("https"));
    let cookie = format!(
        "{COOKIE_NAME}={secret}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}{}",
        30 * 24 * 60 * 60,
        if secure { "; Secure" } else { "" }
    );
    match axum::http::HeaderValue::from_str(&cookie) {
        Ok(v) => {
            res.headers_mut().insert(header::SET_COOKIE, v);
        }
        Err(e) => tracing::error!(error = %e, "building the session cookie failed"),
    }
    Ok(res)
}

/// `POST /api/auth/logout`
pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, Api> {
    if let Some(secret) = credential(&headers) {
        state.auth.logout(&secret).await.map_err(Api::internal)?;
    }
    let mut res = Json(AuthStatus {
        enabled: state.auth.enabled(),
        authenticated: false,
        must_change_password: false,
    })
    .into_response();
    if let Ok(v) = axum::http::HeaderValue::from_str(&format!(
        "{COOKIE_NAME}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0"
    )) {
        res.headers_mut().insert(header::SET_COOKIE, v);
    }
    Ok(res)
}

/// `POST /api/auth/password`
pub async fn change_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PasswordChangeRequest>,
) -> Result<Json<AuthStatus>, Api> {
    let active = credential(&headers).unwrap_or_default();
    state
        .auth
        .change_password(&body.current_password, &body.new_password, &active)
        .await
        .map_err(to_api)?;
    Ok(Json(AuthStatus {
        enabled: true,
        authenticated: true,
        must_change_password: false,
    }))
}

fn to_api(e: LoginError) -> Api {
    match e {
        LoginError::BadCredentials => Api(
            StatusCode::UNAUTHORIZED,
            horsie_models::session_api::ApiError {
                code: "unauthorized".to_string(),
                message: "incorrect password".to_string(),
            },
        ),
        LoginError::WeakPassword(m) => Api::unprocessable(m),
        LoginError::Internal(m) => Api::internal(m),
    }
}
```

- [ ] **Step 4: Wire the routes, state, and layer**

In `server/src/http/mod.rs`:

Add `mod auth;` to the module list, and `use crate::auth::AuthService;` with the other imports.

Add to `AppState`:

```rust
    /// The single admin account, the tokens it issues, and the policy the
    /// `/api` middleware applies. Disabled deployments get a service whose
    /// `enabled()` is false and which passes every request through.
    pub auth: Arc<AuthService>,
```

Add the routes to the `api` router, before `.with_state(state)`:

```rust
        .route("/api/auth/status", get(auth::status))
        .route("/api/auth/login", post(auth::login))
        .route("/api/auth/logout", post(auth::logout))
        .route("/api/auth/password", post(auth::change_password))
```

Then apply the layer to the API router — after `.with_state(state)`, which is what makes the state available to `from_fn_with_state`:

```rust
    let api = Router::new()
        // ... every existing route, plus the four above ...
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ))
        .with_state(state);
```

Note the ordering: `state.clone()` goes to the middleware and the original to `with_state`.

In the existing `test_state` function, add the disabled service to the returned `AppState`:

```rust
        let auth = Arc::new(crate::auth::AuthService::new(
            crate::auth::AuthStore::new(opened.pool.clone()),
            crate::auth::AuthDeps {
                enabled: false,
                state_dir: tmp.path().to_path_buf(),
            },
        ));
```

and add `auth,` to the `AppState { … }` literal.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p horsie-server`
Expected: PASS — every pre-existing HTTP test still passes (they run auth-disabled) plus the six new ones.

- [ ] **Step 6: Commit**

```bash
git add server/src/http/
git commit -m "feat(auth): /api/auth endpoints and the request middleware"
```

---

### Task 7: Boot configuration and the first-boot banner

**Files:**
- Modify: `server/src/bin/horsie-server/config.rs`
- Modify: `server/src/bin/horsie-server/main.rs`

**Interfaces:**
- Consumes: `AuthService`, `AuthDeps`, `AuthStore` from Tasks 2–4; `AppState.auth` from Task 6.
- Produces: `BootConfig.auth: AuthConfig` with `AuthConfig { enabled: bool }` defaulting to `true`, and `auth_enabled(&BootConfig) -> bool` applying the env override.

- [ ] **Step 1: Write the failing test**

In `server/src/bin/horsie-server/config.rs`, add to the existing test module:

```rust
    #[test]
    fn auth_is_enabled_unless_the_config_turns_it_off() {
        let cfg = BootConfig::default();
        assert!(cfg.auth.enabled, "default is on");

        let cfg: BootConfig = serde_json::from_str(r#"{ "auth": { "enabled": false } }"#).unwrap();
        assert!(!cfg.auth.enabled);

        // An unrelated config still gets the default.
        let cfg: BootConfig =
            serde_json::from_str(r#"{ "database": { "url": "sqlite://x.db" } }"#).unwrap();
        assert!(cfg.auth.enabled);
    }

    #[test]
    fn the_env_override_beats_the_file_in_both_directions() {
        let on = BootConfig::default();
        let off: BootConfig = serde_json::from_str(r#"{ "auth": { "enabled": false } }"#).unwrap();
        // An explicit env value wins over whatever the file said.
        assert!(auth_enabled_from(&off, Some("true".into())));
        assert!(auth_enabled_from(&off, Some("1".into())));
        assert!(!auth_enabled_from(&on, Some("false".into())));
        assert!(!auth_enabled_from(&on, Some("0".into())));
        // Unset, or a value we do not recognise, falls through to the file —
        // a typo must not silently disable authentication.
        assert!(auth_enabled_from(&on, None));
        assert!(auth_enabled_from(&on, Some("maybe".into())));
        assert!(!auth_enabled_from(&off, None));
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p horsie-server --bin horsie-server`
Expected: FAIL — `BootConfig` has no `auth` field and `auth_enabled_from` is undefined.

- [ ] **Step 3: Write the implementation**

In `server/src/bin/horsie-server/config.rs`, add the field to `BootConfig`:

```rust
    /// Authentication. Enabled unless explicitly turned off — a deployment
    /// reachable from anywhere but localhost should not be open by accident.
    #[serde(default)]
    pub auth: AuthConfig,
```

and the type plus the override helper:

```rust
#[derive(Debug, Deserialize)]
pub struct AuthConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

fn default_true() -> bool {
    true
}

/// `$HORSIE_AUTH_ENABLED` if set to a recognised value, else the config file.
/// An unrecognised value falls through to the file rather than silently
/// disabling authentication.
pub fn auth_enabled(cfg: &BootConfig) -> bool {
    auth_enabled_from(cfg, std::env::var("HORSIE_AUTH_ENABLED").ok())
}

fn auth_enabled_from(cfg: &BootConfig, env: Option<String>) -> bool {
    match env.as_deref().map(str::trim) {
        Some("1" | "true" | "TRUE" | "yes") => true,
        Some("0" | "false" | "FALSE" | "no") => false,
        _ => cfg.auth.enabled,
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p horsie-server --bin horsie-server`
Expected: PASS.

- [ ] **Step 5: Wire the service into `main.rs`**

In `server/src/bin/horsie-server/main.rs`, after the `memory` service is built and before `RuntimeManager::new`:

```rust
    let auth = Arc::new(horsie_server::auth::AuthService::new(
        horsie_server::auth::AuthStore::new(opened.pool.clone()),
        horsie_server::auth::AuthDeps {
            enabled: config::auth_enabled(&cfg),
            state_dir: state_dir.clone(),
        },
    ));
    match auth.bootstrap().await {
        Ok(Some(password)) => {
            let file = state_dir.join("initial-admin-password").display().to_string();
            println!(
                "\n\
                 ┌──────────────────────────────────────────────────────────────┐\n\
                 │  horsie created its admin account                            │\n\
                 └──────────────────────────────────────────────────────────────┘\n\
                 \n  username: admin\n  password: {password}\n\n\
                 Also written to {file} (delete it after you change the password).\n\
                 Change it from Settings → Account.\n"
            );
        }
        Ok(None) => {}
        Err(e) => return Err(BootError::Config(format!("bootstrapping the admin account: {e}"))),
    }
    if !auth.enabled() {
        println!(
            "warning: authentication is disabled — every caller that can reach \
             this port has full access"
        );
    }
```

Add `auth,` to the `AppState { … }` literal.

- [ ] **Step 6: Verify the whole workspace builds and passes**

Run: `cargo fmt --all && make check`
Expected: PASS.

- [ ] **Step 7: Verify the banner by hand**

Run:

```bash
rm -rf /tmp/horsie-auth-smoke && mkdir -p /tmp/horsie-auth-smoke
XDG_STATE_HOME=/tmp/horsie-auth-smoke/state XDG_DATA_HOME=/tmp/horsie-auth-smoke/data \
  cargo run -p horsie-server --bin horsie-server -- --addr 127.0.0.1:3799
```

Expected: the boxed banner prints a 24-character password on first run and nothing on a second run. In another shell, `curl -s -o /dev/null -w '%{http_code}\n' localhost:3799/api/sessions` returns `401`, and `curl -s localhost:3799/api/auth/status` reports `"enabled":true,"authenticated":false`. Stop the server.

- [ ] **Step 8: Commit**

```bash
git add server/src/bin/horsie-server/
git commit -m "feat(auth): auth.enabled boot config and first-boot admin account"
```

---

### Task 8: Web UI — login gate

**Files:**
- Create: `clients/web/src/hooks/useAuth.ts`
- Create: `clients/web/src/pages/LoginPage.tsx`
- Modify: `clients/web/src/api/client.ts`
- Modify: `clients/web/src/App.tsx`

**Interfaces:**
- Consumes: `AuthStatus`, `LoginRequest`, `PasswordChangeRequest` from `src/generated` (Task 5); the `/api/auth/*` endpoints from Task 6.
- Produces: `api.auth.{status,login,logout,changePassword}`; `useAuthStatus()` returning TanStack Query's result for `AuthStatus`; `<AuthGate>` wrapping the router's content.

- [ ] **Step 1: Add the API methods**

In `clients/web/src/api/client.ts`, add `AuthStatus`, `LoginRequest`, and `PasswordChangeRequest` to the type import block, then add to the `api` object after `health`:

```ts
  auth: {
    status: (): Promise<AuthStatus> => request("/auth/status"),

    login: (password: string): Promise<AuthStatus> =>
      request("/auth/login", {
        method: "POST",
        body: JSON.stringify({ password } satisfies LoginRequest),
      }),

    logout: (): Promise<AuthStatus> =>
      request("/auth/logout", { method: "POST", body: "{}" }),

    changePassword: (body: PasswordChangeRequest): Promise<AuthStatus> =>
      request("/auth/password", { method: "POST", body: JSON.stringify(body) }),
  },
```

In the same file, inside `request`, announce a `401` so the gate can react without every call site knowing about auth. Add immediately before `throw new ApiRequestError(res.status, code, message);`:

```ts
    // A session that expired mid-use should land on the login page, not on a
    // wall of failed queries. The gate listens for this.
    if (res.status === 401) {
      window.dispatchEvent(new Event("horsie:unauthorized"));
    }
```

- [ ] **Step 2: Write the hook**

Create `clients/web/src/hooks/useAuth.ts`:

```ts
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect } from "react";
import { api } from "../api/client";
import type { AuthStatus } from "../api/types";

export const AUTH_STATUS_KEY = ["auth", "status"] as const;

/**
 * The server's view of this browser. Never cached across a `401`: the
 * `horsie:unauthorized` event the API client emits refetches it, which is what
 * drops an expired session back to the login page.
 */
export function useAuthStatus() {
  const qc = useQueryClient();
  const query = useQuery<AuthStatus>({
    queryKey: AUTH_STATUS_KEY,
    queryFn: () => api.auth.status(),
    staleTime: 30_000,
    retry: false,
  });

  useEffect(() => {
    const onUnauthorized = () => {
      void qc.invalidateQueries({ queryKey: AUTH_STATUS_KEY });
    };
    window.addEventListener("horsie:unauthorized", onUnauthorized);
    return () =>
      window.removeEventListener("horsie:unauthorized", onUnauthorized);
  }, [qc]);

  return query;
}
```

If `AuthStatus` is not re-exported from `src/api/types.ts`, add it there alongside the other generated re-exports.

- [ ] **Step 3: Write the login page**

Create `clients/web/src/pages/LoginPage.tsx`:

```tsx
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { api, ApiRequestError } from "../api/client";
import { AUTH_STATUS_KEY } from "../hooks/useAuth";

/** Shown instead of the app whenever the server says auth is on and this
 *  browser is not authenticated. */
export function LoginPage() {
  const qc = useQueryClient();
  const [password, setPassword] = useState("");
  const login = useMutation({
    mutationFn: () => api.auth.login(password),
    onSuccess: (status) => {
      qc.setQueryData(AUTH_STATUS_KEY, status);
      void qc.invalidateQueries();
    },
  });

  const message =
    login.error instanceof ApiRequestError
      ? login.error.message
      : login.error
        ? "Could not sign in."
        : null;

  return (
    <div className="flex h-full items-center justify-center p-6">
      <form
        data-testid="login-form"
        className="w-full max-w-sm space-y-4 rounded-[var(--radius)] border p-6"
        style={{ background: "var(--surface)" }}
        onSubmit={(e) => {
          e.preventDefault();
          login.mutate();
        }}
      >
        <div>
          <h1 className="text-[15px] font-semibold text-text">Sign in</h1>
          <p className="text-xs text-faint">
            This horsie server requires a password.
          </p>
        </div>
        <input
          type="password"
          autoFocus
          autoComplete="current-password"
          data-testid="login-password"
          className="w-full rounded-[var(--radius)] border bg-surface-2 px-2.5 py-2 text-sm text-text"
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          placeholder="Password"
        />
        {message && (
          <p data-testid="login-error" className="text-xs text-danger">
            {message}
          </p>
        )}
        <button
          type="submit"
          data-testid="login-submit"
          disabled={login.isPending || password.length === 0}
          className="w-full rounded-[var(--radius)] bg-accent px-2.5 py-2 text-sm text-on-accent disabled:opacity-50"
        >
          {login.isPending ? "Signing in…" : "Sign in"}
        </button>
      </form>
    </div>
  );
}
```

Check the class names against a neighbouring component (`clients/web/src/pages/settings/fields.tsx`) and use whatever tokens that file uses for inputs, buttons, and danger text — do not invent new ones.

- [ ] **Step 4: Gate the app**

In `clients/web/src/App.tsx`, add the gate component and wrap the router's routes:

```tsx
function AuthGate({ children }: { children: React.ReactNode }) {
  const { data, isPending } = useAuthStatus();
  // Render nothing until the first status lands: flashing a login form at
  // someone who is already signed in is worse than a blank frame.
  if (isPending) return null;
  if (data?.enabled && !data.authenticated) return <LoginPage />;
  return <>{children}</>;
}
```

with imports `import { useAuthStatus } from "./hooks/useAuth";` and `import { LoginPage } from "./pages/LoginPage";`, then wrap the `<Routes>` element:

```tsx
      <BrowserRouter>
        <AuthGate>
          <Routes>
            {/* unchanged */}
          </Routes>
        </AuthGate>
      </BrowserRouter>
```

- [ ] **Step 5: Verify it type-checks and builds**

Run: `cd clients/web && bun run typecheck && bun run build`
Expected: PASS.

- [ ] **Step 6: Verify by hand against a real server**

Run the server from Task 7 Step 7 again (its state directory already has an account, so re-use the password it printed), then `cd clients/web && bun run dev` and open the printed URL. Expected: the login form appears; a wrong password shows an error; the right one reveals the session list.

- [ ] **Step 7: Commit**

```bash
git add clients/web/src
git commit -m "feat(auth): web login page and route gate"
```

---

### Task 9: Web UI — account settings

**Files:**
- Create: `clients/web/src/pages/settings/AccountSettings.tsx`
- Modify: `clients/web/src/pages/settings/SettingsLayout.tsx`
- Modify: `clients/web/src/App.tsx`

**Interfaces:**
- Consumes: `api.auth.{logout,changePassword}` and `useAuthStatus` from Task 8; `SettingsHeader` from `./SettingsHeader`; `SettingsNav`'s `NavItem` from `../../components/SettingsNav`.
- Produces: the `/settings/account` route.

- [ ] **Step 1: Write the page**

Create `clients/web/src/pages/settings/AccountSettings.tsx`:

```tsx
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { ApiRequestError, api } from "../../api/client";
import { AUTH_STATUS_KEY, useAuthStatus } from "../../hooks/useAuth";
import { SettingsHeader } from "./SettingsHeader";

export function AccountSettings() {
  const qc = useQueryClient();
  const { data: status } = useAuthStatus();
  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");

  const change = useMutation({
    mutationFn: () =>
      api.auth.changePassword({ currentPassword: current, newPassword: next }),
    onSuccess: (s) => {
      setCurrent("");
      setNext("");
      qc.setQueryData(AUTH_STATUS_KEY, s);
    },
  });

  const logout = useMutation({
    mutationFn: () => api.auth.logout(),
    onSuccess: (s) => {
      qc.setQueryData(AUTH_STATUS_KEY, s);
      void qc.invalidateQueries();
    },
  });

  if (!status?.enabled) {
    return (
      <div className="flex h-full flex-col">
        <SettingsHeader
          title="Account"
          desc="Sign-in for this server."
        />
        <div className="p-6 text-sm text-muted" data-testid="account-disabled">
          Authentication is disabled on this deployment, so there is no account
          to manage. Anyone who can reach this server has full access.
        </div>
      </div>
    );
  }

  const error =
    change.error instanceof ApiRequestError ? change.error.message : null;

  return (
    <div className="flex h-full flex-col">
      <SettingsHeader title="Account" desc="Sign-in for this server." />
      <div className="space-y-6 p-6">
        {status.mustChangePassword && (
          <p
            data-testid="account-must-change"
            className="rounded-[var(--radius)] border p-3 text-sm text-text"
          >
            This server is still using the password it generated on first boot.
            Change it below, then delete{" "}
            <code>initial-admin-password</code> from the state directory.
          </p>
        )}
        <form
          data-testid="password-form"
          className="max-w-sm space-y-3"
          onSubmit={(e) => {
            e.preventDefault();
            change.mutate();
          }}
        >
          <input
            type="password"
            autoComplete="current-password"
            data-testid="current-password"
            placeholder="Current password"
            className="w-full rounded-[var(--radius)] border bg-surface-2 px-2.5 py-2 text-sm text-text"
            value={current}
            onChange={(e) => setCurrent(e.target.value)}
          />
          <input
            type="password"
            autoComplete="new-password"
            data-testid="new-password"
            placeholder="New password (8 characters or more)"
            className="w-full rounded-[var(--radius)] border bg-surface-2 px-2.5 py-2 text-sm text-text"
            value={next}
            onChange={(e) => setNext(e.target.value)}
          />
          {error && (
            <p data-testid="password-error" className="text-xs text-danger">
              {error}
            </p>
          )}
          {change.isSuccess && (
            <p data-testid="password-saved" className="text-xs text-success">
              Password changed. Other browsers have been signed out.
            </p>
          )}
          <button
            type="submit"
            data-testid="password-submit"
            disabled={change.isPending || !current || !next}
            className="rounded-[var(--radius)] bg-accent px-2.5 py-2 text-sm text-on-accent disabled:opacity-50"
          >
            Change password
          </button>
        </form>
        <button
          type="button"
          data-testid="logout"
          className="rounded-[var(--radius)] border px-2.5 py-2 text-sm text-text"
          onClick={() => logout.mutate()}
        >
          Sign out
        </button>
      </div>
    </div>
  );
}
```

As in Task 8, match the input/button classes to `clients/web/src/pages/settings/fields.tsx` rather than inventing tokens.

- [ ] **Step 2: Add the nav entry and route**

In `clients/web/src/pages/settings/SettingsLayout.tsx`, import `UserCog` from `lucide-react` and append to `ITEMS`:

```tsx
  { to: "account", label: "Account", icon: UserCog },
```

In `clients/web/src/App.tsx`, add inside the `settings` route:

```tsx
              <Route path="account" element={<AccountSettings />} />
```

with `import { AccountSettings } from "./pages/settings/AccountSettings";`.

- [ ] **Step 3: Verify it type-checks and builds**

Run: `cd clients/web && bun run typecheck && bun run build`
Expected: PASS.

- [ ] **Step 4: Verify by hand**

With the dev server and the real server from Task 8 still running: open Settings → Account, change the password, confirm the banner disappears and `initial-admin-password` is gone from the state directory, then sign out and confirm the login form returns.

- [ ] **Step 5: Commit**

```bash
git add clients/web/src
git commit -m "feat(auth): account settings page with password change and sign out"
```

---

### Task 10: End-to-end login test

**Files:**
- Create: `clients/web/e2e/n-auth-login.spec.ts`
- Modify: `clients/web/e2e/global-setup.ts`

**Interfaces:**
- Consumes: `freePort`, `waitFor`, `REPO_ROOT` from `clients/web/e2e/harness.ts`; the built `horsie-server` binary and `dist/` that `global-setup` already produces.
- Produces: nothing other tasks depend on.

The suite's shared server stays auth-disabled — the other fifteen specs assume open access — so this spec brings up its own server on a fresh state directory with auth on.

- [ ] **Step 1: Pin the shared server to auth-disabled**

In `clients/web/e2e/global-setup.ts`, find where the `horsie-server` child process is spawned and add `HORSIE_AUTH_ENABLED: "false"` to its `env` object, with the comment:

```ts
  // The suite drives the API directly and through the UI without signing in.
  // Authentication has its own spec, which brings up its own server.
  HORSIE_AUTH_ENABLED: "false",
```

- [ ] **Step 2: Write the spec**

Create `clients/web/e2e/n-auth-login.spec.ts`:

```ts
import { expect, test } from "@playwright/test";
import { spawn, type ChildProcess } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { REPO_ROOT, freePort, waitFor } from "./harness";

// A second server, auth ON, on its own state/data dirs: the shared one in
// global-setup runs auth-disabled for every other spec.
let proc: ChildProcess | undefined;
let baseURL = "";
let password = "";
let root = "";

test.beforeAll(async () => {
  root = fs.mkdtempSync(path.join(os.tmpdir(), "horsie-auth-e2e-"));
  const port = await freePort();
  baseURL = `http://127.0.0.1:${port}`;
  const bin = path.join(REPO_ROOT, "target/debug/horsie-server");
  const dist = path.join(REPO_ROOT, "clients/web/dist");

  proc = spawn(bin, ["--addr", `127.0.0.1:${port}`, "--web", dist], {
    env: {
      ...process.env,
      HORSIE_AUTH_ENABLED: "true",
      XDG_STATE_HOME: path.join(root, "state"),
      XDG_DATA_HOME: path.join(root, "data"),
      XDG_CONFIG_HOME: path.join(root, "config"),
    },
    stdio: ["ignore", "pipe", "pipe"],
  });

  let out = "";
  proc.stdout?.on("data", (b: Buffer) => {
    out += b.toString();
  });

  await waitFor(async () => {
    const res = await fetch(`${baseURL}/api/health`).catch(() => null);
    return res?.ok === true;
  }, 30_000);

  await waitFor(async () => /password: (\S+)/.test(out), 10_000);
  password = out.match(/password: (\S+)/)?.[1] ?? "";
  expect(password).toHaveLength(24);
});

test.afterAll(() => {
  proc?.kill("SIGKILL");
  fs.rmSync(root, { recursive: true, force: true });
});

test("an unauthenticated browser gets the login form, and the right password opens the app", async ({
  page,
}) => {
  await page.goto(baseURL);
  await expect(page.getByTestId("login-form")).toBeVisible();

  await page.getByTestId("login-password").fill("definitely-wrong");
  await page.getByTestId("login-submit").click();
  await expect(page.getByTestId("login-error")).toBeVisible();

  await page.getByTestId("login-password").fill(password);
  await page.getByTestId("login-submit").click();
  await expect(page.getByTestId("login-form")).toHaveCount(0);
  await expect(page.getByTestId("settings-nav")).toHaveCount(0); // on the sessions view, not settings
});

test("signing out returns the login form", async ({ page }) => {
  await page.goto(baseURL);
  await page.getByTestId("login-password").fill(password);
  await page.getByTestId("login-submit").click();
  await expect(page.getByTestId("login-form")).toHaveCount(0);

  await page.goto(`${baseURL}/settings/account`);
  await expect(page.getByTestId("account-must-change")).toBeVisible();
  await page.getByTestId("logout").click();
  await expect(page.getByTestId("login-form")).toBeVisible();
});
```

- [ ] **Step 3: Run the spec**

Run: `cd clients/web && bun run test:e2e -- n-auth-login`
Expected: PASS, 2 tests.

- [ ] **Step 4: Run the whole e2e suite**

Run: `cd clients/web && bun run test:e2e`
Expected: PASS — the other specs are unaffected because their server runs auth-disabled.

- [ ] **Step 5: Commit**

```bash
git add clients/web/e2e
git commit -m "test(auth): end-to-end login and sign-out"
```

---

### Task 11: Documentation

**Files:**
- Modify: `README.md`
- Modify: `docs/guide/README.md`
- Modify: `docs/guide/self-hosting.md`
- Modify: `docs/guide/settings-reference.md`

**Interfaces:**
- Consumes: the behaviour shipped in Tasks 7–9.
- Produces: nothing other tasks depend on.

- [ ] **Step 1: Replace the "no authentication" warnings**

In `README.md`, replace the block that begins `> **No built-in authentication.**` with:

```markdown
> **Authentication is on by default.** On first boot the server creates an
> `admin` account, prints a generated password, and writes it to
> `initial-admin-password` in its state directory. Change it from
> **Settings → Account**. To run without a password on a trusted network, set
> `HORSIE_AUTH_ENABLED=false` or `{"auth": {"enabled": false}}` in `config.json`.
```

Apply the same replacement to the equivalent block in `docs/guide/README.md`.

- [ ] **Step 2: Rewrite the self-hosting security section**

In `docs/guide/self-hosting.md`, replace the `**Security:** there is no authentication…` paragraph with:

```markdown
**Signing in.** The first time the server starts it creates an `admin` account
and prints a generated password:

    docker compose -f docker/docker-compose.yml logs horsie | grep -A4 'admin account'

The same password is written to `initial-admin-password` in the server's state
directory, so a rotated log is not a lockout. Change it from
**Settings → Account**, which deletes that file.

**Turning it off.** On a trusted network — or behind an auth proxy that already
identifies callers — set `HORSIE_AUTH_ENABLED=false`, or `"auth": {"enabled":
false}` in `config.json`. The server then behaves exactly as it did before
authentication existed: anything that can reach the port has full access.
```

Also update the sentence in the "Manual / advanced setup" preamble that reads `running behind your own reverse proxy / auth layer` — it is still true and needs no change; leave it.

- [ ] **Step 3: Document the config key**

In `docs/guide/settings-reference.md`, in the table of `config.json` deployment settings, add a row:

```markdown
| `auth.enabled` | Require a password for the web UI and API. Default `true`. Override with `HORSIE_AUTH_ENABLED=false`. |
```

Match the existing table's column count and style — read the surrounding rows before writing this one.

- [ ] **Step 4: Verify the docs describe what shipped**

Re-read each edited paragraph against the behaviour in Tasks 7–9: the banner wording, the file name, the env var spelling, and the settings path (`Settings → Account`). Fix any drift.

- [ ] **Step 5: Commit**

```bash
git add README.md docs/guide/
git commit -m "docs: authentication is on by default"
```

---

### Task 12: Full verification and pull request

**Files:** none.

- [ ] **Step 1: Run every check**

Run: `cargo fmt --all && make check`
Expected: PASS.

- [ ] **Step 2: Build the web UI**

Run: `cd clients/web && bun run generate-types && bun run typecheck && bun run build`
Expected: PASS, and `git status` shows no unstaged changes under `src/generated` (the CI drift job checks this).

- [ ] **Step 3: Run the e2e suite**

Run: `cd clients/web && bun run test:e2e`
Expected: PASS.

- [ ] **Step 4: Confirm the upgrade path by hand**

Point the server at a state directory that already has a settings database from before this change (copy one, or run `main` first), start it, and confirm: the migration applies, the banner prints once, and the UI asks for the password. This is the homelab's upgrade experience — see it work before shipping it.

- [ ] **Step 5: Open the pull request**

```bash
git push -u origin feat/server-auth
gh pr create --title "Auth A: identity core and web UI login" --body "$(cat <<'EOF'
Closes #107. Part of #106. Design: `docs/superpowers/specs/2026-08-02-server-auth-design.md`.

Adds a single admin account, an opaque-token identity core shared by all three auth surfaces, and a web UI that logs in against it. Authentication is on by default; `HORSIE_AUTH_ENABLED=false` or `{"auth":{"enabled":false}}` restores the previous open behaviour.

First boot generates the admin password, prints it, and writes it to `initial-admin-password` in the state directory — an operator whose container logs have rotated would otherwise be locked out with no recovery short of editing SQLite.

Failed logins are throttled by delaying the failure rather than by locking the account: behind a reverse proxy every request shares one source address, so per-IP lockout would let an attacker deny the admin their own server by guessing wrong on purpose.

Existing HTTP and e2e tests run with auth disabled — a real supported configuration, not a test-only escape — with authenticated coverage added alongside.

**This is breaking for existing deployments.** After upgrading, the homelab GitOps deploy needs the generated password from the container log or state directory.
EOF
)"
```

- [ ] **Step 6: Confirm CI is green**

Run: `gh pr checks --watch`
Expected: all checks pass.
