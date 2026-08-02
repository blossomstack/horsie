# Auth B: CLI login via device flow — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let `horsie auth login --server <url>` obtain tokens from a horsie server through a device-code flow approved in the browser, store them, and use them for `horsie session tail`.

**Architecture:** The device grant's *shape* over fluorite JSON — not RFC 8628 on the wire. Sub-project A's `auth_tokens` table and single verification path already carry `access`/`refresh` kinds and a `chain_id` column, and `auth_device_codes` already exists unused; this change fills them in. Access tokens live an hour, refresh tokens ninety days and rotate on every use, and presenting an already-rotated refresh revokes its whole chain.

**Tech Stack:** Rust 2024, axum 0.7, sqlx 0.8 (SQLite), fluorite 0.6 with TS codegen, clap 4, reqwest, React 19 + TanStack Query, Playwright.

## Global Constraints

- Production code denies `clippy::unwrap_used`, `expect_used`, `panic`, `wildcard_enum_match_arm`. Test modules use the existing `#[cfg_attr(test, allow(...))]` / `#![allow(...)]` headers.
- No SQL foreign keys. Auth timestamps are `INTEGER` epoch seconds.
- Store/service layers return `Result<T, String>`. Every wire type is fluorite-generated; JSON is camelCase.
- Test SQLite pools are **file-backed with `busy_timeout`**, never `sqlite::memory:` — each pooled connection would otherwise get its own private database.
- `make check` = `fmt-check` + `clippy --all-targets --all-features -D warnings` + `test --workspace`. Run `cargo fmt --all` **before** clippy.
- **Out of scope, belongs to #109:** `horsie connect` presenting a credential, and the server requiring one on `/api/vendor/connect`. This change makes tokens *available*; C makes them *required*. Do not touch `runtime-vendor` or `cli/src/connect.rs`.

---

### Task 1: Device-code and token-chain storage

**Files:**
- Modify: `server/src/auth/store.rs`
- Modify: `server/src/auth/mod.rs`

**Interfaces:**
- Consumes: `Principal`, `TokenKind` from A.
- Produces on `AuthStore`: `insert_device_code`, `get_device_code`, `approve_device_code`, `deny_device_code`, `mark_device_polled`, `consume_device_code`, `purge_expired_device_codes`, `lookup_token_including_revoked`, `revoke_chain`; and `DeviceCodeRow { user_code, principal, expires_at, approved_at, denied_at, consumed_at, last_polled_at }`.

`lookup_token_including_revoked` is what makes refresh-reuse detection possible: the ordinary `lookup_token` returns `None` for a revoked row, which is indistinguishable from a token that never existed, and reuse detection needs exactly that distinction.

- [ ] **Step 1: Write the failing tests**

Append to the `mod tests` block in `server/src/auth/store.rs`:

```rust
    #[tokio::test]
    async fn a_device_code_is_created_then_approved_then_consumed() {
        let (s, _tmp) = store().await;
        s.insert_device_code(b"dhash", "BCDF-GHJK", 1000, 1600)
            .await
            .unwrap();

        let row = s.get_device_code(b"dhash").await.unwrap().unwrap();
        assert_eq!(row.user_code, "BCDF-GHJK");
        assert_eq!(row.expires_at, 1600);
        assert!(row.principal.is_none());
        assert!(row.approved_at.is_none());

        // Approval by user code, which is what the browser sends.
        assert!(
            s.approve_device_code("BCDF-GHJK", &Principal::User(1), 1100)
                .await
                .unwrap()
        );
        let row = s.get_device_code(b"dhash").await.unwrap().unwrap();
        assert_eq!(row.principal, Some(Principal::User(1)));
        assert_eq!(row.approved_at, Some(1100));

        // Consuming marks it used, so a second poll cannot mint a second pair.
        s.consume_device_code(b"dhash", 1200).await.unwrap();
        let row = s.get_device_code(b"dhash").await.unwrap().unwrap();
        assert_eq!(row.consumed_at, Some(1200));
    }

    #[tokio::test]
    async fn approving_or_denying_an_unknown_user_code_reports_it() {
        let (s, _tmp) = store().await;
        assert!(
            !s.approve_device_code("NOPE-NOPE", &Principal::User(1), 1000)
                .await
                .unwrap()
        );
        assert!(!s.deny_device_code("NOPE-NOPE", 1000).await.unwrap());
    }

    #[tokio::test]
    async fn denial_and_poll_marking_are_recorded() {
        let (s, _tmp) = store().await;
        s.insert_device_code(b"d2", "AAAA-BBBB", 1000, 1600)
            .await
            .unwrap();
        s.mark_device_polled(b"d2", 1050).await.unwrap();
        assert_eq!(
            s.get_device_code(b"d2").await.unwrap().unwrap().last_polled_at,
            Some(1050)
        );
        assert!(s.deny_device_code("AAAA-BBBB", 1060).await.unwrap());
        assert_eq!(
            s.get_device_code(b"d2").await.unwrap().unwrap().denied_at,
            Some(1060)
        );
    }

    #[tokio::test]
    async fn expired_device_codes_are_purged() {
        let (s, _tmp) = store().await;
        s.insert_device_code(b"old", "OLDC-ODEE", 100, 200)
            .await
            .unwrap();
        s.insert_device_code(b"new", "NEWC-ODEE", 1000, 1600)
            .await
            .unwrap();
        s.purge_expired_device_codes(1000).await.unwrap();
        assert!(s.get_device_code(b"old").await.unwrap().is_none());
        assert!(s.get_device_code(b"new").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_revoked_token_is_still_findable_for_reuse_detection() {
        let (s, _tmp) = store().await;
        let t = generate(TokenKind::Refresh);
        s.insert_token(
            "r1",
            TokenKind::Refresh,
            &Principal::User(1),
            &t.hash,
            None,
            Some("chain-a"),
            None,
            1000,
        )
        .await
        .unwrap();
        s.revoke_token("r1", 1100).await.unwrap();

        // The ordinary lookup hides it...
        assert!(s.lookup_token(&t.hash, 1200).await.unwrap().is_none());
        // ...but reuse detection needs to tell "revoked" from "never existed".
        let found = s
            .lookup_token_including_revoked(&t.hash)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, "r1");
        assert_eq!(found.chain_id.as_deref(), Some("chain-a"));
        assert!(found.revoked_at.is_some());
        assert!(
            s.lookup_token_including_revoked(b"never")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn revoking_a_chain_takes_every_token_in_it() {
        let (s, _tmp) = store().await;
        let (a, b, other) = (
            generate(TokenKind::Access),
            generate(TokenKind::Refresh),
            generate(TokenKind::Access),
        );
        for (id, kind, tok, chain) in [
            ("a", TokenKind::Access, &a, "chain-a"),
            ("b", TokenKind::Refresh, &b, "chain-a"),
            ("c", TokenKind::Access, &other, "chain-b"),
        ] {
            s.insert_token(
                id,
                kind,
                &Principal::User(1),
                &tok.hash,
                None,
                Some(chain),
                None,
                1000,
            )
            .await
            .unwrap();
        }
        s.revoke_chain("chain-a", 2000).await.unwrap();
        assert!(s.lookup_token(&a.hash, 2001).await.unwrap().is_none());
        assert!(s.lookup_token(&b.hash, 2001).await.unwrap().is_none());
        assert!(s.lookup_token(&other.hash, 2001).await.unwrap().is_some());
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p horsie-server --lib auth::store`
Expected: FAIL — the new methods do not exist.

- [ ] **Step 3: Implement**

In `server/src/auth/store.rs`, add above the tests:

```rust
/// A row of `auth_device_codes`, addressed by the hash of the device code the
/// CLI holds. `user_code` is the short string the human types.
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceCodeRow {
    pub user_code: String,
    pub principal: Option<Principal>,
    pub expires_at: i64,
    pub approved_at: Option<i64>,
    pub denied_at: Option<i64>,
    pub consumed_at: Option<i64>,
    pub last_polled_at: Option<i64>,
}

/// A token row as seen by reuse detection: revoked rows included, so a
/// presented-but-dead credential can be told apart from one that never existed.
#[derive(Clone, Debug, PartialEq)]
pub struct RawTokenRow {
    pub id: String,
    pub kind: TokenKind,
    pub principal: Principal,
    pub chain_id: Option<String>,
    pub expires_at: Option<i64>,
    pub revoked_at: Option<i64>,
}
```

and, inside `impl AuthStore`:

```rust
    // --- device codes ---

    pub async fn insert_device_code(
        &self,
        device_hash: &[u8],
        user_code: &str,
        now: i64,
        expires_at: i64,
    ) -> Result<(), String> {
        sqlx::query(
            "INSERT INTO auth_device_codes \
             (device_code_hash, user_code, created_at, expires_at) VALUES (?, ?, ?, ?)",
        )
        .bind(device_hash)
        .bind(user_code)
        .bind(now)
        .bind(expires_at)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// The row as stored. Expiry is *not* filtered here: the poll endpoint has
    /// to tell an expired code (`expired_token`) from an unknown one, and both
    /// answers come from this one read.
    pub async fn get_device_code(
        &self,
        device_hash: &[u8],
    ) -> Result<Option<DeviceCodeRow>, String> {
        let row = sqlx::query(
            "SELECT user_code, principal, expires_at, approved_at, denied_at, \
             consumed_at, last_polled_at FROM auth_device_codes WHERE device_code_hash = ?",
        )
        .bind(device_hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        let Some(row) = row else { return Ok(None) };
        let principal: Option<String> = row.try_get("principal").map_err(|e| e.to_string())?;
        Ok(Some(DeviceCodeRow {
            user_code: row.try_get("user_code").map_err(|e| e.to_string())?,
            principal: principal.as_deref().map(Principal::from_db).transpose()?,
            expires_at: row.try_get("expires_at").map_err(|e| e.to_string())?,
            approved_at: row.try_get("approved_at").map_err(|e| e.to_string())?,
            denied_at: row.try_get("denied_at").map_err(|e| e.to_string())?,
            consumed_at: row.try_get("consumed_at").map_err(|e| e.to_string())?,
            last_polled_at: row.try_get("last_polled_at").map_err(|e| e.to_string())?,
        }))
    }

    /// Returns whether a live, unanswered code was actually approved — `false`
    /// for an unknown, expired, or already-answered user code, which is what
    /// the browser needs in order to say "that code is no longer valid".
    pub async fn approve_device_code(
        &self,
        user_code: &str,
        principal: &Principal,
        now: i64,
    ) -> Result<bool, String> {
        let res = sqlx::query(
            "UPDATE auth_device_codes SET approved_at = ?, principal = ? \
             WHERE user_code = ? AND expires_at > ? \
             AND approved_at IS NULL AND denied_at IS NULL",
        )
        .bind(now)
        .bind(principal.to_db())
        .bind(user_code)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn deny_device_code(&self, user_code: &str, now: i64) -> Result<bool, String> {
        let res = sqlx::query(
            "UPDATE auth_device_codes SET denied_at = ? \
             WHERE user_code = ? AND expires_at > ? \
             AND approved_at IS NULL AND denied_at IS NULL",
        )
        .bind(now)
        .bind(user_code)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn mark_device_polled(&self, device_hash: &[u8], now: i64) -> Result<(), String> {
        sqlx::query("UPDATE auth_device_codes SET last_polled_at = ? WHERE device_code_hash = ?")
            .bind(now)
            .bind(device_hash)
            .execute(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn consume_device_code(&self, device_hash: &[u8], now: i64) -> Result<(), String> {
        sqlx::query("UPDATE auth_device_codes SET consumed_at = ? WHERE device_code_hash = ?")
            .bind(now)
            .bind(device_hash)
            .execute(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Housekeeping, called whenever a code is issued. Device codes are
    /// short-lived and never read after expiry, so nothing is lost.
    pub async fn purge_expired_device_codes(&self, now: i64) -> Result<(), String> {
        sqlx::query("DELETE FROM auth_device_codes WHERE expires_at <= ?")
            .bind(now)
            .execute(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    // --- token chains ---

    /// Look a token up regardless of revocation or expiry. Only refresh-reuse
    /// detection should call this; everything else wants `lookup_token`.
    pub async fn lookup_token_including_revoked(
        &self,
        hash: &[u8],
    ) -> Result<Option<RawTokenRow>, String> {
        let row = sqlx::query(
            "SELECT id, kind, principal, chain_id, expires_at, revoked_at \
             FROM auth_tokens WHERE token_hash = ?",
        )
        .bind(hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        let Some(row) = row else { return Ok(None) };
        let kind: String = row.try_get("kind").map_err(|e| e.to_string())?;
        let principal: String = row.try_get("principal").map_err(|e| e.to_string())?;
        Ok(Some(RawTokenRow {
            id: row.try_get("id").map_err(|e| e.to_string())?,
            kind: TokenKind::from_db(&kind)
                .ok_or_else(|| format!("unknown token kind {kind:?}"))?,
            principal: Principal::from_db(&principal)?,
            chain_id: row.try_get("chain_id").map_err(|e| e.to_string())?,
            expires_at: row.try_get("expires_at").map_err(|e| e.to_string())?,
            revoked_at: row.try_get("revoked_at").map_err(|e| e.to_string())?,
        }))
    }

    /// Revoke every live token sharing a rotation chain — the response to a
    /// replayed refresh token.
    pub async fn revoke_chain(&self, chain_id: &str, now: i64) -> Result<(), String> {
        sqlx::query(
            "UPDATE auth_tokens SET revoked_at = ? WHERE chain_id = ? AND revoked_at IS NULL",
        )
        .bind(now)
        .bind(chain_id)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }
```

Export the new rows from `server/src/auth/mod.rs`:

```rust
pub use store::{AuthStore, DeviceCodeRow, RawTokenRow, TokenRow, UserRow};
```

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p horsie-server --lib auth::store`
Expected: PASS — 7 existing plus 6 new.

- [ ] **Step 5: Commit**

```bash
git add server/src/auth/
git commit -m "feat(auth): device-code and token-chain storage"
```

---

### Task 2: Device authorization and refresh in AuthService

**Files:**
- Modify: `server/src/auth/service.rs`
- Modify: `server/src/auth/mod.rs`

**Interfaces:**
- Consumes: Task 1's store methods.
- Produces on `AuthService`: `start_device_authorization() -> Result<DeviceAuthorization, String>`; `poll_device_token(&str) -> Result<IssuedTokens, DeviceError>`; `approve_device(&str, &Principal) -> Result<bool, String>`; `deny_device(&str) -> Result<bool, String>`; `refresh(&str) -> Result<IssuedTokens, DeviceError>`. Plus `DeviceAuthorization { device_code, user_code, expires_in, interval }`, `IssuedTokens { access_token, refresh_token, expires_in }`, and `DeviceError::{AuthorizationPending, SlowDown, ExpiredToken, AccessDenied, Internal(String)}`.

- [ ] **Step 1: Write the failing tests**

Append to `mod tests` in `server/src/auth/service.rs`:

```rust
    #[tokio::test]
    async fn a_user_code_is_readable_and_unambiguous() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = service(&tmp, true).await;
        svc.bootstrap().await.unwrap();
        let d = svc.start_device_authorization().await.unwrap();

        assert_eq!(d.user_code.len(), 9, "XXXX-XXXX");
        assert_eq!(d.user_code.chars().nth(4), Some('-'));
        assert!(
            d.user_code
                .chars()
                .filter(|c| *c != '-')
                .all(|c| USER_CODE_ALPHABET.contains(c)),
            "{}",
            d.user_code
        );
        // Nothing a human can misread between O/0, I/1, or misspell as a word.
        for bad in ['O', '0', 'I', '1', 'A', 'E', 'U'] {
            assert!(!d.user_code.contains(bad), "{} in {}", bad, d.user_code);
        }
        assert!(d.device_code.starts_with("hsk_"));
        assert_eq!(d.interval, DEVICE_POLL_INTERVAL_SECS);
        assert_eq!(d.expires_in, DEVICE_CODE_TTL_SECS);
    }

    #[tokio::test]
    async fn polling_walks_pending_then_approved_then_refuses_a_second_redemption() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = service(&tmp, true).await;
        svc.bootstrap().await.unwrap();
        let d = svc.start_device_authorization().await.unwrap();

        assert!(matches!(
            svc.poll_device_token(&d.device_code).await,
            Err(DeviceError::AuthorizationPending)
        ));

        assert!(svc.approve_device(&d.user_code, &Principal::User(1)).await.unwrap());

        let issued = svc.poll_device_token(&d.device_code).await.unwrap();
        assert!(issued.access_token.starts_with("hsk_usr_"));
        assert!(issued.refresh_token.starts_with("hsk_ref_"));
        assert_eq!(issued.expires_in, ACCESS_TOKEN_TTL_SECS);

        // The access token authenticates as the approver.
        let v = svc.verify(&issued.access_token).await.unwrap().unwrap();
        assert_eq!(v.principal, Principal::User(1));
        assert_eq!(v.kind, TokenKind::Access);

        // A consumed code cannot mint a second pair.
        assert!(matches!(
            svc.poll_device_token(&d.device_code).await,
            Err(DeviceError::ExpiredToken)
        ));
    }

    #[tokio::test]
    async fn a_denied_code_reports_access_denied_and_an_unknown_one_expires() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = service(&tmp, true).await;
        svc.bootstrap().await.unwrap();
        let d = svc.start_device_authorization().await.unwrap();

        assert!(svc.deny_device(&d.user_code).await.unwrap());
        assert!(matches!(
            svc.poll_device_token(&d.device_code).await,
            Err(DeviceError::AccessDenied)
        ));
        // Answering twice is refused.
        assert!(!svc.deny_device(&d.user_code).await.unwrap());
        assert!(!svc.approve_device(&d.user_code, &Principal::User(1)).await.unwrap());

        // An unknown device code is indistinguishable from an expired one.
        assert!(matches!(
            svc.poll_device_token("hsk_ref_nosuchcode").await,
            Err(DeviceError::ExpiredToken)
        ));
    }

    #[tokio::test]
    async fn polling_faster_than_the_interval_is_told_to_slow_down() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = service(&tmp, true).await;
        svc.bootstrap().await.unwrap();
        let d = svc.start_device_authorization().await.unwrap();

        assert!(matches!(
            svc.poll_device_token(&d.device_code).await,
            Err(DeviceError::AuthorizationPending)
        ));
        // Immediately again: too fast.
        assert!(matches!(
            svc.poll_device_token(&d.device_code).await,
            Err(DeviceError::SlowDown)
        ));
    }

    #[tokio::test]
    async fn refresh_rotates_and_replaying_the_old_token_kills_the_chain() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = service(&tmp, true).await;
        svc.bootstrap().await.unwrap();
        let d = svc.start_device_authorization().await.unwrap();
        svc.approve_device(&d.user_code, &Principal::User(1)).await.unwrap();
        let first = svc.poll_device_token(&d.device_code).await.unwrap();

        let second = svc.refresh(&first.refresh_token).await.unwrap();
        assert_ne!(first.refresh_token, second.refresh_token);
        assert_ne!(first.access_token, second.access_token);
        // The rotated-away refresh token is dead.
        assert!(svc.verify(&first.refresh_token).await.unwrap().is_none());
        // The new pair works.
        assert!(svc.verify(&second.access_token).await.unwrap().is_some());

        // Replaying the old refresh token is the only signal available that a
        // credential file was copied: kill everything it could have produced.
        assert!(matches!(
            svc.refresh(&first.refresh_token).await,
            Err(DeviceError::AccessDenied)
        ));
        assert!(svc.verify(&second.access_token).await.unwrap().is_none());
        assert!(svc.verify(&second.refresh_token).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn refresh_refuses_anything_that_is_not_a_live_refresh_token() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = service(&tmp, true).await;
        let pw = svc.bootstrap().await.unwrap().unwrap();
        let web = svc.login(&pw).await.unwrap();

        assert!(matches!(
            svc.refresh(&web).await,
            Err(DeviceError::AccessDenied)
        ));
        assert!(matches!(
            svc.refresh("not-a-token").await,
            Err(DeviceError::AccessDenied)
        ));
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p horsie-server --lib auth::service`
Expected: FAIL — `start_device_authorization` and friends do not exist.

- [ ] **Step 3: Implement**

In `server/src/auth/service.rs` add near the other constants:

```rust
/// CLI access tokens live an hour; the refresh token carries the session.
pub const ACCESS_TOKEN_TTL_SECS: i64 = 60 * 60;
const REFRESH_TOKEN_TTL_SECS: i64 = 90 * 24 * 60 * 60;
/// Long enough to walk to a browser, short enough that a guessed user code is
/// worthless.
pub const DEVICE_CODE_TTL_SECS: u32 = 600;
pub const DEVICE_POLL_INTERVAL_SECS: u32 = 5;

/// No vowels (so no code spells a word), and no `0`/`O`/`1`/`I` (so nothing is
/// misread off a terminal). 28 symbols over 8 places is ~38 bits, which a
/// ten-minute expiry and a poll floor make untargetable.
pub const USER_CODE_ALPHABET: &str = "BCDFGHJKLMNPQRSTVWXZ23456789";
const USER_CODE_LEN: usize = 8;
```

and the types:

```rust
/// What `POST /api/auth/device/code` hands the CLI.
pub struct DeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    pub expires_in: u32,
    pub interval: u32,
}

/// An access/refresh pair. Both are opaque secrets; only their hashes persist.
pub struct IssuedTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
}

/// Poll and refresh failures. The names are RFC 8628's because they are
/// already the right words, even though the wire format here is our own.
#[derive(Debug)]
pub enum DeviceError {
    AuthorizationPending,
    SlowDown,
    ExpiredToken,
    AccessDenied,
    Internal(String),
}
```

and, inside `impl AuthService`:

```rust
    /// Issue a device code + user code. Expired codes are purged on the way in,
    /// which is the only cleanup this table needs.
    pub async fn start_device_authorization(&self) -> Result<DeviceAuthorization, String> {
        let now = now_secs();
        self.store.purge_expired_device_codes(now).await?;

        let device = generate(TokenKind::Refresh);
        let user_code = generate_user_code();
        self.store
            .insert_device_code(
                &device.hash,
                &user_code,
                now,
                now + i64::from(DEVICE_CODE_TTL_SECS),
            )
            .await?;
        Ok(DeviceAuthorization {
            device_code: device.secret,
            user_code,
            expires_in: DEVICE_CODE_TTL_SECS,
            interval: DEVICE_POLL_INTERVAL_SECS,
        })
    }

    /// Approve a pending code on behalf of the logged-in browser. `false` means
    /// the code is unknown, expired, or already answered.
    pub async fn approve_device(
        &self,
        user_code: &str,
        principal: &Principal,
    ) -> Result<bool, String> {
        self.store
            .approve_device_code(&normalize_user_code(user_code), principal, now_secs())
            .await
    }

    pub async fn deny_device(&self, user_code: &str) -> Result<bool, String> {
        self.store
            .deny_device_code(&normalize_user_code(user_code), now_secs())
            .await
    }

    /// One poll. Unknown and expired are the same answer deliberately: a caller
    /// holding a code we have no record of learns nothing by being told which.
    pub async fn poll_device_token(&self, device_code: &str) -> Result<IssuedTokens, DeviceError> {
        let now = now_secs();
        let hash = hash_secret(device_code);
        let row = self
            .store
            .get_device_code(&hash)
            .await
            .map_err(DeviceError::Internal)?
            .ok_or(DeviceError::ExpiredToken)?;

        if row.consumed_at.is_some() || row.expires_at <= now {
            return Err(DeviceError::ExpiredToken);
        }
        if row.denied_at.is_some() {
            return Err(DeviceError::AccessDenied);
        }

        // An approved code is served immediately, without the poll floor. Rate
        // limiting exists to stop a tight loop hammering *while pending*; a
        // code can only be redeemed once, so delaying the one poll that
        // succeeds would buy nothing and cost the user a needless wait.
        if let Some(principal) = row.principal {
            self.store
                .consume_device_code(&hash, now)
                .await
                .map_err(DeviceError::Internal)?;
            return self
                .issue_pair(&principal, &uuid::Uuid::new_v4().to_string(), now)
                .await
                .map_err(DeviceError::Internal);
        }

        if row
            .last_polled_at
            .is_some_and(|t| now - t < i64::from(DEVICE_POLL_INTERVAL_SECS))
        {
            return Err(DeviceError::SlowDown);
        }
        self.store
            .mark_device_polled(&hash, now)
            .await
            .map_err(DeviceError::Internal)?;
        Err(DeviceError::AuthorizationPending)
    }

    /// Rotate a refresh token. Presenting one that was already rotated away
    /// revokes its whole chain — the only signal available that a credential
    /// file was copied.
    pub async fn refresh(&self, refresh_token: &str) -> Result<IssuedTokens, DeviceError> {
        if parse(refresh_token) != Some(TokenKind::Refresh) {
            return Err(DeviceError::AccessDenied);
        }
        let now = now_secs();
        let hash = hash_secret(refresh_token);
        let row = self
            .store
            .lookup_token_including_revoked(&hash)
            .await
            .map_err(DeviceError::Internal)?
            .ok_or(DeviceError::AccessDenied)?;

        if row.kind != TokenKind::Refresh {
            return Err(DeviceError::AccessDenied);
        }
        if row.revoked_at.is_some() {
            if let Some(chain) = row.chain_id.as_deref() {
                self.store
                    .revoke_chain(chain, now)
                    .await
                    .map_err(DeviceError::Internal)?;
                tracing::warn!(chain, "a rotated refresh token was replayed; chain revoked");
            }
            return Err(DeviceError::AccessDenied);
        }
        if row.expires_at.is_some_and(|e| e <= now) {
            return Err(DeviceError::AccessDenied);
        }

        let chain = row
            .chain_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        // Retire the presented token first: a failure after this point costs
        // the caller a re-login, whereas leaving it live would let the same
        // secret mint pairs forever.
        self.store
            .revoke_token(&row.id, now)
            .await
            .map_err(DeviceError::Internal)?;
        self.issue_pair(&row.principal, &chain, now)
            .await
            .map_err(DeviceError::Internal)
    }

    /// Mint an access/refresh pair on one rotation chain.
    async fn issue_pair(
        &self,
        principal: &Principal,
        chain_id: &str,
        now: i64,
    ) -> Result<IssuedTokens, String> {
        let access = generate(TokenKind::Access);
        let refresh = generate(TokenKind::Refresh);
        self.store
            .insert_token(
                &uuid::Uuid::new_v4().to_string(),
                TokenKind::Access,
                principal,
                &access.hash,
                None,
                Some(chain_id),
                Some(now + ACCESS_TOKEN_TTL_SECS),
                now,
            )
            .await?;
        self.store
            .insert_token(
                &uuid::Uuid::new_v4().to_string(),
                TokenKind::Refresh,
                principal,
                &refresh.hash,
                None,
                Some(chain_id),
                Some(now + REFRESH_TOKEN_TTL_SECS),
                now,
            )
            .await?;
        Ok(IssuedTokens {
            access_token: access.secret,
            refresh_token: refresh.secret,
            expires_in: ACCESS_TOKEN_TTL_SECS,
        })
    }
```

and the free functions near `now_secs`:

```rust
fn generate_user_code() -> String {
    use rand::Rng;
    let alphabet: Vec<char> = USER_CODE_ALPHABET.chars().collect();
    let mut rng = rand::thread_rng();
    let mut out = String::with_capacity(USER_CODE_LEN + 1);
    for i in 0..USER_CODE_LEN {
        if i == USER_CODE_LEN / 2 {
            out.push('-');
        }
        out.push(alphabet[rng.gen_range(0..alphabet.len())]);
    }
    out
}

/// Accept what a human actually types: lowercase, and with or without the dash.
fn normalize_user_code(input: &str) -> String {
    let bare: String = input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if bare.len() == USER_CODE_LEN {
        format!("{}-{}", &bare[..USER_CODE_LEN / 2], &bare[USER_CODE_LEN / 2..])
    } else {
        bare
    }
}
```

Export from `server/src/auth/mod.rs`:

```rust
pub use service::{
    ACCESS_TOKEN_TTL_SECS, ADMIN_USERNAME, AuthDeps, AuthService, DEVICE_CODE_TTL_SECS,
    DEVICE_POLL_INTERVAL_SECS, DeviceAuthorization, DeviceError, INITIAL_PASSWORD_FILE,
    IssuedTokens, LoginError, USER_CODE_ALPHABET, VerifiedToken,
};
```

- [ ] **Step 4: Add a normalization test**

```rust
    #[test]
    fn user_codes_are_normalized_the_way_people_type_them() {
        assert_eq!(normalize_user_code("bcdf-ghjk"), "BCDF-GHJK");
        assert_eq!(normalize_user_code("bcdfghjk"), "BCDF-GHJK");
        assert_eq!(normalize_user_code(" BCDF GHJK "), "BCDF-GHJK");
        // Anything not eight characters is passed through for the store to miss.
        assert_eq!(normalize_user_code("short"), "SHORT");
    }
```

- [ ] **Step 5: Run to verify they pass**

Run: `cargo test -p horsie-server --lib auth::`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add server/src/auth/
git commit -m "feat(auth): device authorization and refresh-token rotation"
```

---

### Task 3: Wire types

**Files:**
- Modify: `models/fluorite/auth.fl`
- Modify: `models/src/lib.rs` (extend the `auth_wire_tests` module)

**Interfaces:**
- Produces: `DeviceCodeResponse`, `DeviceTokenRequest`, `TokenPair`, `DeviceApprovalRequest`, `RefreshRequest` in `horsie_models::auth`, plus their TypeScript.

- [ ] **Step 1: Write the failing test**

Add to `auth_wire_tests` in `models/src/lib.rs`:

```rust
    #[test]
    fn device_flow_types_are_camel_case_on_the_wire() {
        use crate::auth::{DeviceCodeResponse, DeviceTokenRequest, TokenPair};
        let json = serde_json::to_string(&DeviceCodeResponse {
            device_code: "d".into(),
            user_code: "BCDF-GHJK".into(),
            verification_uri: "http://x/auth/device".into(),
            verification_uri_complete: "http://x/auth/device?code=BCDF-GHJK".into(),
            expires_in: 600,
            interval: 5,
        })
        .unwrap();
        assert!(json.contains("\"deviceCode\""), "{json}");
        assert!(json.contains("\"verificationUriComplete\""), "{json}");

        let req: DeviceTokenRequest =
            serde_json::from_str(r#"{"deviceCode":"d"}"#).unwrap();
        assert_eq!(req.device_code, "d");

        let pair = serde_json::to_string(&TokenPair {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_in: 3600,
        })
        .unwrap();
        assert!(pair.contains("\"accessToken\""), "{pair}");
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p horsie-models auth_wire`
Expected: FAIL — the types do not exist.

- [ ] **Step 3: Extend the schema**

Append to `models/fluorite/auth.fl`:

```
/// What the CLI gets when it starts a device authorization. The device code is
/// the secret it polls with; the user code is what a human reads out and types.
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    /// The same page with the code pre-filled, for when the CLI can print a
    /// link the user can click.
    verification_uri_complete: String,
    /// Seconds until the code expires.
    expires_in: u32,
    /// Seconds the CLI must wait between polls. Polling faster is answered
    /// with `slow_down` and does not reset the timer.
    interval: u32,
}

struct DeviceTokenRequest {
    device_code: String,
}

/// An access token and the refresh token that replaces it. The refresh token
/// rotates on every use.
struct TokenPair {
    access_token: String,
    refresh_token: String,
    /// Seconds until the access token expires.
    expires_in: u32,
}

/// Approve or deny a pending device authorization from the browser.
struct DeviceApprovalRequest {
    user_code: String,
}

struct RefreshRequest {
    refresh_token: String,
}
```

- [ ] **Step 4: Run and regenerate TypeScript**

Run: `cargo test -p horsie-models auth_wire`
Expected: PASS.

Run: `cd clients/web && bun run generate-types && bun run typecheck`
Expected: PASS, with new files under `src/generated/auth/`.

- [ ] **Step 5: Commit**

```bash
git add models/ clients/web/src/generated
git commit -m "feat(auth): device-flow wire types"
```

---

### Task 4: HTTP endpoints

**Files:**
- Modify: `server/src/http/auth.rs`
- Modify: `server/src/http/mod.rs` (routes + tests)

**Interfaces:**
- Produces: `POST /api/auth/device/code`, `/device/token`, `/device/approve`, `/device/deny`, `/api/auth/refresh`.

`/device/code`, `/device/token`, and `/refresh` join the unauthenticated allowlist — they are how a caller *becomes* authenticated. `/device/approve` and `/device/deny` require the browser cookie, which is the entire security of the flow: only someone already logged in can approve a code.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `server/src/http/mod.rs`:

```rust
    #[tokio::test]
    async fn the_device_flow_issues_tokens_that_open_the_api() {
        use horsie_models::auth::{DeviceCodeResponse, TokenPair};
        let tmp = tempfile::tempdir().unwrap();
        let (state, pw) = auth_state(&tmp).await;
        let app = app(state);

        // The CLI starts a device authorization without any credential.
        let res = app
            .clone()
            .oneshot(post_json("/api/auth/device/code", &serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let device: DeviceCodeResponse = read_json(res).await;
        assert!(device.verification_uri.ends_with("/auth/device"));
        assert!(device.verification_uri_complete.contains(&device.user_code));

        // Polling before approval is pending, not an error.
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/auth/device/token",
                &serde_json::json!({"deviceCode": device.device_code}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let err: horsie_models::session_api::ApiError = read_json(res).await;
        assert_eq!(err.code, "authorization_pending");

        // Approving needs a logged-in browser.
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/auth/device/approve",
                &serde_json::json!({"userCode": device.user_code}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        let res = app
            .clone()
            .oneshot(post_json(
                "/api/auth/login",
                &serde_json::json!({"password": pw}),
            ))
            .await
            .unwrap();
        let cookie = session_cookie(&res);

        let approve = Request::builder()
            .method("POST")
            .uri("/api/auth/device/approve")
            .header("content-type", "application/json")
            .header("cookie", format!("horsie_session={cookie}"))
            .body(Body::from(
                serde_json::to_vec(&serde_json::json!({"userCode": device.user_code})).unwrap(),
            ))
            .unwrap();
        assert_eq!(app.clone().oneshot(approve).await.unwrap().status(), StatusCode::OK);

        // The next poll mints the pair immediately: an approved code skips the
        // poll floor, so this test needs no sleep.
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/auth/device/token",
                &serde_json::json!({"deviceCode": device.device_code}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let pair: TokenPair = read_json(res).await;

        // The access token opens the API as a bearer.
        let req = Request::builder()
            .uri("/api/sessions")
            .header("authorization", format!("Bearer {}", pair.access_token))
            .body(Body::empty())
            .unwrap();
        assert_eq!(app.clone().oneshot(req).await.unwrap().status(), StatusCode::OK);

        // Refresh rotates, unauthenticated (the refresh token is the credential).
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/auth/refresh",
                &serde_json::json!({"refreshToken": pair.refresh_token}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let rotated: TokenPair = read_json(res).await;
        assert_ne!(rotated.refresh_token, pair.refresh_token);

        // Replaying the old refresh token is refused.
        let res = app
            .oneshot(post_json(
                "/api/auth/refresh",
                &serde_json::json!({"refreshToken": pair.refresh_token}),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn denying_a_device_code_reports_access_denied_to_the_poller() {
        use horsie_models::auth::DeviceCodeResponse;
        let tmp = tempfile::tempdir().unwrap();
        let (state, pw) = auth_state(&tmp).await;
        let app = app(state);

        let res = app
            .clone()
            .oneshot(post_json("/api/auth/device/code", &serde_json::json!({})))
            .await
            .unwrap();
        let device: DeviceCodeResponse = read_json(res).await;

        let res = app
            .clone()
            .oneshot(post_json(
                "/api/auth/login",
                &serde_json::json!({"password": pw}),
            ))
            .await
            .unwrap();
        let cookie = session_cookie(&res);

        let deny = Request::builder()
            .method("POST")
            .uri("/api/auth/device/deny")
            .header("content-type", "application/json")
            .header("cookie", format!("horsie_session={cookie}"))
            .body(Body::from(
                serde_json::to_vec(&serde_json::json!({"userCode": device.user_code})).unwrap(),
            ))
            .unwrap();
        assert_eq!(app.clone().oneshot(deny).await.unwrap().status(), StatusCode::OK);

        let res = app
            .oneshot(post_json(
                "/api/auth/device/token",
                &serde_json::json!({"deviceCode": device.device_code}),
            ))
            .await
            .unwrap();
        let err: horsie_models::session_api::ApiError = read_json(res).await;
        assert_eq!(err.code, "access_denied");
    }

    #[tokio::test]
    async fn approving_an_unknown_user_code_is_a_404() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, pw) = auth_state(&tmp).await;
        let app = app(state);
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/auth/login",
                &serde_json::json!({"password": pw}),
            ))
            .await
            .unwrap();
        let cookie = session_cookie(&res);
        let req = Request::builder()
            .method("POST")
            .uri("/api/auth/device/approve")
            .header("content-type", "application/json")
            .header("cookie", format!("horsie_session={cookie}"))
            .body(Body::from(
                serde_json::to_vec(&serde_json::json!({"userCode": "ZZZZ-ZZZZ"})).unwrap(),
            ))
            .unwrap();
        assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::NOT_FOUND);
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p horsie-server --lib http::tests`
Expected: FAIL — routes are missing (404).

- [ ] **Step 3: Implement the handlers**

In `server/src/http/auth.rs`, extend `is_public`:

```rust
fn is_public(path: &str) -> bool {
    path == "/api/health"
        || path == "/api/auth/status"
        || path == "/api/auth/login"
        // How a CLI becomes authenticated in the first place. Approval, which
        // is the actual authorization step, requires the browser cookie.
        || path == "/api/auth/device/code"
        || path == "/api/auth/device/token"
        || path == "/api/auth/refresh"
        || path.starts_with("/api/plugin-artifacts/")
}
```

and add the handlers:

```rust
/// `POST /api/auth/device/code` — start a device authorization.
pub async fn device_code(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<DeviceCodeResponse>, Api> {
    let d = state
        .auth
        .start_device_authorization()
        .await
        .map_err(Api::internal)?;
    // Same-origin: the browser page that approves this code is served by this
    // very server, so the request's own host is the right verification URI.
    let base = crate::http::request_base(&headers);
    Ok(Json(DeviceCodeResponse {
        verification_uri: format!("{base}/auth/device"),
        verification_uri_complete: format!("{base}/auth/device?code={}", d.user_code),
        device_code: d.device_code,
        user_code: d.user_code,
        expires_in: d.expires_in,
        interval: d.interval,
    }))
}

/// `POST /api/auth/device/token` — one poll.
pub async fn device_token(
    State(state): State<AppState>,
    Json(body): Json<DeviceTokenRequest>,
) -> Result<Json<TokenPair>, Api> {
    match state.auth.poll_device_token(&body.device_code).await {
        Ok(t) => Ok(Json(TokenPair {
            access_token: t.access_token,
            refresh_token: t.refresh_token,
            expires_in: u32::try_from(t.expires_in).unwrap_or(u32::MAX),
        })),
        Err(e) => Err(device_error(e)),
    }
}

/// `POST /api/auth/refresh` — rotate a refresh token.
pub async fn refresh(
    State(state): State<AppState>,
    Json(body): Json<RefreshRequest>,
) -> Result<Json<TokenPair>, Api> {
    match state.auth.refresh(&body.refresh_token).await {
        Ok(t) => Ok(Json(TokenPair {
            access_token: t.access_token,
            refresh_token: t.refresh_token,
            expires_in: u32::try_from(t.expires_in).unwrap_or(u32::MAX),
        })),
        Err(e) => Err(device_error(e)),
    }
}

/// `POST /api/auth/device/approve` — cookie-authenticated.
pub async fn device_approve(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<Principal>,
    Json(body): Json<DeviceApprovalRequest>,
) -> Result<StatusCode, Api> {
    let approved = state
        .auth
        .approve_device(&body.user_code, &principal)
        .await
        .map_err(Api::internal)?;
    answered(approved)
}

/// `POST /api/auth/device/deny` — cookie-authenticated.
pub async fn device_deny(
    State(state): State<AppState>,
    Json(body): Json<DeviceApprovalRequest>,
) -> Result<StatusCode, Api> {
    let denied = state
        .auth
        .deny_device(&body.user_code)
        .await
        .map_err(Api::internal)?;
    answered(denied)
}

/// Unknown, expired, and already-answered codes are one 404: the person at the
/// browser can do the same thing about all three — start over.
fn answered(ok: bool) -> Result<StatusCode, Api> {
    if ok {
        Ok(StatusCode::OK)
    } else {
        Err(Api::not_found(
            "that code is not waiting for an answer — it may have expired or already been used",
        ))
    }
}

/// Poll/refresh failures answer `400` with the RFC's error name as the code, so
/// a client can branch on `authorization_pending` vs `slow_down` without
/// parsing prose.
fn device_error(e: DeviceError) -> Api {
    let (code, message) = match e {
        DeviceError::AuthorizationPending => (
            "authorization_pending",
            "waiting for the code to be approved in a browser",
        ),
        DeviceError::SlowDown => ("slow_down", "polling too fast"),
        DeviceError::ExpiredToken => ("expired_token", "that code has expired or was already used"),
        DeviceError::AccessDenied => ("access_denied", "that request was denied"),
        DeviceError::Internal(m) => return Api::internal(m),
    };
    Api(
        StatusCode::BAD_REQUEST,
        horsie_models::session_api::ApiError {
            code: code.to_string(),
            message: message.to_string(),
        },
    )
}
```

Extend the imports at the top of the file:

```rust
use crate::auth::{DeviceError, LoginError, Principal};
use horsie_models::auth::{
    AuthStatus, DeviceApprovalRequest, DeviceCodeResponse, DeviceTokenRequest, LoginRequest,
    PasswordChangeRequest, RefreshRequest, TokenPair,
};
```

In `server/src/http/mod.rs`, add the routes next to the existing `/api/auth/*` ones:

```rust
        .route("/api/auth/device/code", post(auth::device_code))
        .route("/api/auth/device/token", post(auth::device_token))
        .route("/api/auth/device/approve", post(auth::device_approve))
        .route("/api/auth/device/deny", post(auth::device_deny))
        .route("/api/auth/refresh", post(auth::refresh))
```

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p horsie-server --lib http::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add server/src/http/
git commit -m "feat(auth): device-flow HTTP endpoints"
```

---

### Task 5: Browser approval page

**Files:**
- Create: `clients/web/src/pages/DeviceApprovalPage.tsx`
- Modify: `clients/web/src/api/client.ts`, `clients/web/src/App.tsx`

**Interfaces:**
- Produces: `api.auth.{approveDevice,denyDevice}` and the `/auth/device` route.

The page sits inside `AuthGate`, so an unauthenticated visitor is shown the login form first and lands here afterwards. That is the flow's whole security model, and it needs no extra code.

- [ ] **Step 1: Add the API methods**

In `clients/web/src/api/client.ts`, inside `api.auth`:

```ts
    approveDevice: (userCode: string): Promise<void> =>
      request("/auth/device/approve", {
        method: "POST",
        body: JSON.stringify({ userCode } satisfies DeviceApprovalRequest),
      }),

    denyDevice: (userCode: string): Promise<void> =>
      request("/auth/device/deny", {
        method: "POST",
        body: JSON.stringify({ userCode } satisfies DeviceApprovalRequest),
      }),
```

and add `DeviceApprovalRequest` to the type import block.

- [ ] **Step 2: Write the page**

Create `clients/web/src/pages/DeviceApprovalPage.tsx`:

```tsx
import { useMutation } from "@tanstack/react-query";
import { useState } from "react";
import { useSearchParams } from "react-router-dom";
import { ApiRequestError, api } from "../api/client";

/** Approve or deny a `horsie auth login` waiting on a device code. */
export function DeviceApprovalPage() {
  const [params] = useSearchParams();
  const [code, setCode] = useState(params.get("code") ?? "");

  const approve = useMutation({ mutationFn: () => api.auth.approveDevice(code) });
  const deny = useMutation({ mutationFn: () => api.auth.denyDevice(code) });

  const error = [approve.error, deny.error].find(
    (e): e is ApiRequestError => e instanceof ApiRequestError,
  );

  return (
    <div className="flex h-full items-center justify-center p-6">
      <div className="card w-full max-w-md space-y-4 p-6" data-testid="device-page">
        <div>
          <h1 className="text-[15px] font-semibold text-text">
            Authorize a command-line login
          </h1>
          <p className="mt-0.5 text-xs text-faint">
            Check that this code matches the one your terminal printed. Approving
            grants that machine access to this server as you.
          </p>
        </div>

        {approve.isSuccess ? (
          <p data-testid="device-approved" className="text-sm text-success">
            Approved. Your terminal should continue in a few seconds — you can
            close this page.
          </p>
        ) : deny.isSuccess ? (
          <p data-testid="device-denied" className="text-sm text-text">
            Denied. That login attempt was refused.
          </p>
        ) : (
          <>
            <input
              className="input text-center font-mono text-lg tracking-[0.3em]"
              data-testid="device-code"
              value={code}
              onChange={(e) => setCode(e.target.value)}
              placeholder="XXXX-XXXX"
              autoFocus
            />
            {error && (
              <p data-testid="device-error" className="text-xs text-error">
                {error.message}
              </p>
            )}
            <div className="flex gap-2">
              <button
                className="btn-primary flex-1 justify-center"
                data-testid="device-approve"
                disabled={!code || approve.isPending}
                onClick={() => approve.mutate()}
              >
                Approve
              </button>
              <button
                className="btn-outline flex-1 justify-center"
                data-testid="device-deny"
                disabled={!code || deny.isPending}
                onClick={() => deny.mutate()}
              >
                Deny
              </button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}
```

- [ ] **Step 3: Route it**

In `clients/web/src/App.tsx`, add the import and a route inside the `/` layout route:

```tsx
            <Route path="auth/device" element={<DeviceApprovalPage />} />
```

- [ ] **Step 4: Typecheck and build**

Run: `cd clients/web && bun run typecheck && bun run build`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add clients/web/src
git commit -m "feat(auth): browser page for approving a CLI login"
```

---

### Task 6: CLI credential store

**Files:**
- Create: `cli/src/auth.rs`
- Modify: `cli/src/lib.rs`, `cli/Cargo.toml`

**Interfaces:**
- Produces: `Credentials` (load/save/get/set/remove), `normalize_server`, `ServerCredentials { access_token, refresh_token, expires_at }`, `credentials_path()`, and `resolve_token(server) -> Result<Option<String>, CliError>` which prefers `HORSIE_TOKEN` and refreshes an expired access token.

- [ ] **Step 1: Write the failing tests**

Create `cli/src/auth.rs` with only a test module:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn server_urls_normalize_so_one_server_is_one_entry() {
        assert_eq!(normalize_server("http://Localhost:3789/"), "http://localhost:3789");
        assert_eq!(normalize_server("http://localhost:3789"), "http://localhost:3789");
        assert_eq!(
            normalize_server("HTTPS://Horsie.Example.COM/"),
            "https://horsie.example.com"
        );
        // A path is kept: someone may host horsie under a prefix.
        assert_eq!(normalize_server("https://x.com/horsie/"), "https://x.com/horsie");
    }

    #[test]
    fn credentials_round_trip_through_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("credentials.json");

        let mut creds = Credentials::default();
        assert!(creds.get("http://localhost:3789").is_none());

        creds.set(
            "http://localhost:3789/",
            ServerCredentials {
                access_token: "hsk_usr_a".into(),
                refresh_token: "hsk_ref_r".into(),
                expires_at: 1000,
            },
        );
        creds.save(&path).unwrap();

        let back = Credentials::load(&path).unwrap();
        let c = back.get("http://localhost:3789").expect("normalized lookup");
        assert_eq!(c.access_token, "hsk_usr_a");
        assert_eq!(c.expires_at, 1000);

        let mut back = back;
        assert!(back.remove("http://nope").is_none());
        assert!(back.remove("http://localhost:3789").is_some());
        assert!(back.get("http://localhost:3789").is_none());
    }

    #[test]
    fn a_missing_file_is_an_empty_set_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let creds = Credentials::load(&tmp.path().join("absent.json")).unwrap();
        assert!(creds.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn the_credential_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("credentials.json");
        let mut creds = Credentials::default();
        creds.set(
            "http://x",
            ServerCredentials {
                access_token: "a".into(),
                refresh_token: "r".into(),
                expires_at: 0,
            },
        );
        creds.save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    }

    #[test]
    fn an_access_token_near_expiry_counts_as_expired() {
        let c = ServerCredentials {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: 1000,
        };
        // Fresh with room to spare.
        assert!(!c.is_expired(900));
        // Inside the safety margin: treat as expired rather than send a token
        // that dies in flight.
        assert!(c.is_expired(1000 - EXPIRY_MARGIN_SECS + 1));
        assert!(c.is_expired(2000));
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p horsie auth::`
Expected: FAIL — the module is not declared.

- [ ] **Step 3: Implement**

Prepend to `cli/src/auth.rs`:

```rust
//! Credentials for talking to a session server: where they live, how they are
//! read and written, and how an expired access token is refreshed.
//!
//! One file holds every server the user has logged into, keyed by normalised
//! URL, so `--server` picks the right credential without further configuration.

use crate::error::CliError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Refresh this long before the access token actually expires, so a token does
/// not die between the check and the request it was attached to.
pub const EXPIRY_MARGIN_SECS: i64 = 60;

/// Environment override, for scripts and CI: skips the credential file
/// entirely.
pub const TOKEN_ENV: &str = "HORSIE_TOKEN";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerCredentials {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix epoch seconds at which `access_token` stops working.
    pub expires_at: i64,
}

impl ServerCredentials {
    pub fn is_expired(&self, now: i64) -> bool {
        now >= self.expires_at - EXPIRY_MARGIN_SECS
    }
}

/// Every server this machine has credentials for. `BTreeMap` so the file has a
/// stable order and diffs cleanly if a human ever looks at it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(default)]
    pub servers: BTreeMap<String, ServerCredentials>,
}

impl Credentials {
    /// A missing file is an empty set: not being logged in anywhere is a normal
    /// state, not an error.
    pub fn load(path: &Path) -> Result<Self, CliError> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| CliError::Config(format!("parse {}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(CliError::Io(format!("read {}: {e}", path.display()))),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), CliError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| CliError::Io(format!("create {}: {e}", parent.display())))?;
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| CliError::Config(format!("serialize credentials: {e}")))?;
        std::fs::write(path, format!("{text}\n"))
            .map_err(|e| CliError::Io(format!("write {}: {e}", path.display())))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| CliError::Io(format!("chmod {}: {e}", path.display())))?;
        }
        Ok(())
    }

    pub fn get(&self, server: &str) -> Option<&ServerCredentials> {
        self.servers.get(&normalize_server(server))
    }

    pub fn set(&mut self, server: &str, creds: ServerCredentials) {
        self.servers.insert(normalize_server(server), creds);
    }

    pub fn remove(&mut self, server: &str) -> Option<ServerCredentials> {
        self.servers.remove(&normalize_server(server))
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }
}

/// `<config-dir>/horsie/credentials.json`, beside the CLI's other state.
pub fn credentials_path() -> PathBuf {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(x) if !x.is_empty() => PathBuf::from(x),
        _ => match std::env::var_os("HOME") {
            Some(h) if !h.is_empty() => PathBuf::from(h).join(".config"),
            _ => PathBuf::from(".horsie"),
        },
    };
    base.join("horsie").join("credentials.json")
}

/// Scheme and host lowercased, trailing slash dropped. Without this,
/// `http://Localhost:3789/` and `http://localhost:3789` would be two entries
/// and logging in through one would not satisfy the other.
pub fn normalize_server(server: &str) -> String {
    let trimmed = server.trim().trim_end_matches('/');
    match trimmed.split_once("://") {
        Some((scheme, rest)) => {
            let (host, path) = match rest.split_once('/') {
                Some((h, p)) => (h, Some(p)),
                None => (rest, None),
            };
            let base = format!("{}://{}", scheme.to_ascii_lowercase(), host.to_ascii_lowercase());
            match path {
                Some(p) => format!("{base}/{p}"),
                None => base,
            }
        }
        None => trimmed.to_ascii_lowercase(),
    }
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
```

Add `pub mod auth;` to `cli/src/lib.rs`, and `tempfile = { workspace = true }` to `cli/Cargo.toml`'s `[dev-dependencies]` if absent.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p horsie auth::`
Expected: PASS, 5 tests.

- [ ] **Step 5: Commit**

```bash
git add cli/
git commit -m "feat(cli): credential store for session servers"
```

---

### Task 7: `horsie auth` commands and an authenticated `session tail`

**Files:**
- Modify: `cli/src/auth.rs` (the flow client), `cli/src/main.rs`, `cli/src/session.rs`

**Interfaces:**
- Consumes: Task 6's store, Task 4's endpoints.
- Produces: `auth::login(server, token: Option<&str>)`, `auth::logout(server: Option<&str>)`, `auth::status()`, and `auth::resolve_token(server)`; `horsie auth login|logout|status`; `session::tail` sending a bearer.

- [ ] **Step 1: Write the failing test**

Add to `cli/src/auth.rs`'s test module:

```rust
    #[tokio::test]
    async fn resolve_token_prefers_the_environment_override() {
        // The env override exists precisely so scripts need no credential file.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("credentials.json");
        let token = resolve_token_with(
            "http://localhost:3789",
            &path,
            Some("hsk_usr_from_env".to_string()),
        )
        .await
        .unwrap();
        assert_eq!(token.as_deref(), Some("hsk_usr_from_env"));
    }

    #[tokio::test]
    async fn resolve_token_returns_a_live_stored_token_without_refreshing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("credentials.json");
        let mut creds = Credentials::default();
        creds.set(
            "http://localhost:3789",
            ServerCredentials {
                access_token: "hsk_usr_live".into(),
                refresh_token: "hsk_ref_r".into(),
                // Comfortably in the future, so no network call is attempted —
                // which is what makes this test hermetic.
                expires_at: now_secs() + 3600,
            },
        );
        creds.save(&path).unwrap();

        let token = resolve_token_with("http://localhost:3789", &path, None)
            .await
            .unwrap();
        assert_eq!(token.as_deref(), Some("hsk_usr_live"));
    }

    #[tokio::test]
    async fn resolve_token_is_none_when_the_server_is_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("credentials.json");
        assert!(
            resolve_token_with("http://elsewhere", &path, None)
                .await
                .unwrap()
                .is_none()
        );
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p horsie auth::`
Expected: FAIL — `resolve_token_with` does not exist.

- [ ] **Step 3: Implement the flow client**

Append to `cli/src/auth.rs`:

```rust
/// The subset of the server's device-flow responses the CLI reads. Declared
/// here rather than taken from `horsie_models` so the CLI does not depend on
/// the server's whole wire surface for four fields.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: String,
    expires_in: u32,
    interval: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenPair {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
}

#[derive(Deserialize)]
struct ApiErrorBody {
    code: String,
    #[serde(default)]
    message: String,
}

fn api_url(server: &str, path: &str) -> String {
    format!("{}{path}", normalize_server(server))
}

/// POST a JSON body and read the server's error envelope on failure.
async fn post_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
) -> Result<Result<T, ApiErrorBody>, CliError> {
    let res = client
        .post(url)
        .json(body)
        .send()
        .await
        .map_err(|e| CliError::Server(format!("{url}: {e}")))?;
    if res.status().is_success() {
        let parsed = res
            .json::<T>()
            .await
            .map_err(|e| CliError::Server(format!("{url}: unexpected response: {e}")))?;
        return Ok(Ok(parsed));
    }
    let status = res.status();
    let err = res.json::<ApiErrorBody>().await.unwrap_or(ApiErrorBody {
        code: format!("http_{}", status.as_u16()),
        message: status.to_string(),
    });
    Ok(Err(err))
}

/// `horsie auth login`. With `token`, validate and store a pasted credential;
/// otherwise run the device flow to completion.
pub async fn login(server: &str, token: Option<&str>) -> Result<(), CliError> {
    let path = credentials_path();
    let client = reqwest::Client::new();

    if let Some(token) = token {
        validate_token(&client, server, token).await?;
        let mut creds = Credentials::load(&path)?;
        creds.set(
            server,
            ServerCredentials {
                access_token: token.to_string(),
                // A pasted token has no refresh half; when it dies the user
                // pastes another. Recorded as already-expired-proof by giving
                // it no expiry we could act on.
                refresh_token: String::new(),
                expires_at: i64::MAX,
            },
        );
        creds.save(&path)?;
        println!("stored a token for {}", normalize_server(server));
        return Ok(());
    }

    let start: DeviceCode = post_json(
        &client,
        &api_url(server, "/api/auth/device/code"),
        &serde_json::json!({}),
    )
    .await?
    .map_err(|e| CliError::Server(format!("starting the login: {}", e.message)))?;

    println!("To authorize this machine, open:\n");
    println!("    {}\n", start.verification_uri_complete);
    println!("and confirm the code:  {}\n", start.user_code);
    println!(
        "(If the link does not open, go to {} and type the code.)",
        start.verification_uri
    );

    let deadline = now_secs() + i64::from(start.expires_in);
    // The server's `interval` is a floor, and it answers `slow_down` if we
    // ignore it — so back off on that rather than hammering.
    let mut interval = u64::from(start.interval);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        if now_secs() >= deadline {
            return Err(CliError::Server(
                "the code expired before it was approved; run `horsie auth login` again".into(),
            ));
        }
        let polled: Result<TokenPair, ApiErrorBody> = post_json(
            &client,
            &api_url(server, "/api/auth/device/token"),
            &serde_json::json!({ "deviceCode": start.device_code }),
        )
        .await?;
        match polled {
            Ok(pair) => {
                let mut creds = Credentials::load(&path)?;
                creds.set(
                    server,
                    ServerCredentials {
                        access_token: pair.access_token,
                        refresh_token: pair.refresh_token,
                        expires_at: now_secs() + pair.expires_in,
                    },
                );
                creds.save(&path)?;
                println!("\nLogged in to {}.", normalize_server(server));
                return Ok(());
            }
            Err(e) => match e.code.as_str() {
                "authorization_pending" => {}
                "slow_down" => interval = interval.saturating_add(5),
                "access_denied" => {
                    return Err(CliError::Server("that login was denied".into()));
                }
                _ => {
                    return Err(CliError::Server(format!(
                        "login failed: {} ({})",
                        e.message, e.code
                    )));
                }
            },
        }
    }
}

/// Confirm a pasted token actually authenticates before storing it — otherwise
/// the first failure would surface much later, somewhere unrelated.
async fn validate_token(
    client: &reqwest::Client,
    server: &str,
    token: &str,
) -> Result<(), CliError> {
    let url = api_url(server, "/api/auth/status");
    let res = client
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| CliError::Server(format!("{url}: {e}")))?;
    let status: serde_json::Value = res
        .json()
        .await
        .map_err(|e| CliError::Server(format!("{url}: unexpected response: {e}")))?;
    if status.get("authenticated").and_then(|v| v.as_bool()) == Some(true) {
        Ok(())
    } else if status.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
        Err(CliError::Server(
            "that server has authentication disabled, so it needs no token".into(),
        ))
    } else {
        Err(CliError::Server("that token was not accepted".into()))
    }
}

/// `horsie auth logout`. Revocation is best-effort: forgetting the credential
/// locally is the part the user asked for, and a server that cannot be reached
/// must not leave a dead entry in the file.
pub async fn logout(server: Option<&str>) -> Result<(), CliError> {
    let path = credentials_path();
    let mut creds = Credentials::load(&path)?;
    let targets: Vec<String> = match server {
        Some(s) => vec![normalize_server(s)],
        None => creds.servers.keys().cloned().collect(),
    };
    if targets.is_empty() {
        println!("not logged in to anything");
        return Ok(());
    }
    let client = reqwest::Client::new();
    for target in targets {
        if let Some(c) = creds.remove(&target) {
            let url = api_url(&target, "/api/auth/logout");
            match client.post(&url).bearer_auth(&c.access_token).send().await {
                Ok(_) => println!("logged out of {target}"),
                Err(e) => println!("forgot {target} locally (could not reach it: {e})"),
            }
        }
    }
    creds.save(&path)?;
    Ok(())
}

/// `horsie auth status`.
pub fn status() -> Result<(), CliError> {
    let path = credentials_path();
    let creds = Credentials::load(&path)?;
    if creds.is_empty() {
        println!("not logged in to any server");
        println!("run `horsie auth login --server <url>` to log in");
        return Ok(());
    }
    let now = now_secs();
    println!("credentials in {}\n", path.display());
    for (server, c) in &creds.servers {
        let state = if c.refresh_token.is_empty() {
            "pasted token".to_string()
        } else if c.is_expired(now) {
            "access token expired (will refresh on next use)".to_string()
        } else {
            format!("valid for {}m", (c.expires_at - now) / 60)
        };
        println!("  {server}  —  {state}");
    }
    Ok(())
}

/// The bearer to send to `server`, refreshing a stale access token first.
/// `None` means "no credential configured", which callers report as a prompt to
/// log in rather than as a failure.
pub async fn resolve_token(server: &str) -> Result<Option<String>, CliError> {
    resolve_token_with(server, &credentials_path(), std::env::var(TOKEN_ENV).ok()).await
}

async fn resolve_token_with(
    server: &str,
    path: &Path,
    env_token: Option<String>,
) -> Result<Option<String>, CliError> {
    if let Some(t) = env_token.filter(|t| !t.is_empty()) {
        return Ok(Some(t));
    }
    let mut creds = Credentials::load(path)?;
    let Some(current) = creds.get(server).cloned() else {
        return Ok(None);
    };
    if !current.is_expired(now_secs()) || current.refresh_token.is_empty() {
        return Ok(Some(current.access_token));
    }

    let client = reqwest::Client::new();
    let refreshed: Result<TokenPair, ApiErrorBody> = post_json(
        &client,
        &api_url(server, "/api/auth/refresh"),
        &serde_json::json!({ "refreshToken": current.refresh_token }),
    )
    .await?;
    match refreshed {
        Ok(pair) => {
            let updated = ServerCredentials {
                access_token: pair.access_token,
                refresh_token: pair.refresh_token,
                expires_at: now_secs() + pair.expires_in,
            };
            creds.set(server, updated.clone());
            creds.save(path)?;
            Ok(Some(updated.access_token))
        }
        Err(_) => {
            // The refresh token is dead (rotated away, revoked, or expired).
            // Drop it so the next run says "log in" instead of retrying a
            // credential that can never work again.
            creds.remove(server);
            creds.save(path)?;
            Err(CliError::Server(format!(
                "the stored login for {} is no longer valid — run `horsie auth login --server {}`",
                normalize_server(server),
                normalize_server(server)
            )))
        }
    }
}
```

- [ ] **Step 4: Wire the commands**

In `cli/src/main.rs`, add to `enum Command`:

```rust
    /// Log in to a session server so other commands can reach it.
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
```

and the subcommand enum:

```rust
#[derive(Subcommand)]
enum AuthAction {
    /// Authorize this machine against a session server, approving in a browser.
    Login {
        /// `http(s)://host:port` of the session server.
        #[arg(long)]
        server: String,
        /// Store this token instead of running the browser flow. For scripts.
        #[arg(long)]
        token: Option<String>,
    },
    /// Forget stored credentials, revoking them server-side when reachable.
    Logout {
        /// Omit to log out of every server.
        #[arg(long)]
        server: Option<String>,
    },
    /// Show which servers this machine has credentials for.
    Status,
}
```

and the dispatch arm, matching the shape of the existing arms:

```rust
        Command::Auth { action } => match action {
            AuthAction::Login { server, token } => {
                horsie::auth::login(&server, token.as_deref()).await?;
                Ok(0)
            }
            AuthAction::Logout { server } => {
                horsie::auth::logout(server.as_deref()).await?;
                Ok(0)
            }
            AuthAction::Status => {
                horsie::auth::status()?;
                Ok(0)
            }
        },
```

- [ ] **Step 5: Send the bearer from `session tail`**

In `cli/src/session.rs`, inside `tail`, after the client is built:

```rust
    let token = crate::auth::resolve_token(server).await?;
```

and where the request is built each reconnect:

```rust
        let mut req = client.get(&url);
        if let Some(t) = &token {
            req = req.bearer_auth(t);
        }
```

Then handle a `401` in the stream match, next to the existing `NOT_FOUND` arm:

```rust
                    Some(Err(reqwest_eventsource::Error::InvalidStatusCode(status, _)))
                        if status == reqwest::StatusCode::UNAUTHORIZED =>
                    {
                        es.close();
                        return Err(CliError::Server(format!(
                            "not authorized for {server} — run `horsie auth login --server {server}`"
                        )));
                    }
```

- [ ] **Step 6: Verify**

Run: `cargo test -p horsie` then `cargo fmt --all && cargo clippy --all-targets --all-features -- -D warnings`
Expected: PASS.

Run: `cargo run -p horsie -- auth status`
Expected: "not logged in to any server".

- [ ] **Step 7: Commit**

```bash
git add cli/
git commit -m "feat(cli): horsie auth login/logout/status and an authenticated session tail"
```

---

### Task 8: End-to-end verification, docs, and the pull request

**Files:**
- Modify: `docs/guide/getting-started.md`, `docs/guide/settings-reference.md`
- Create: `clients/web/e2e/o-device-approval.spec.ts`

- [ ] **Step 1: Add an e2e spec for the approval page**

Create `clients/web/e2e/o-device-approval.spec.ts` modelled on `n-auth-login.spec.ts` (same auth-enabled second server), which: starts a device authorization over HTTP, logs in through the UI, visits `/auth/device?code=<userCode>`, approves, and asserts the poll endpoint then returns a token pair.

```ts
import { expect, test } from "@playwright/test";
import { spawn, type ChildProcess } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { REPO_ROOT, WEB_DIR, freePort, waitFor } from "./harness";

let proc: ChildProcess | undefined;
let baseURL = "";
let password = "";
let root = "";

test.beforeAll(async () => {
  root = fs.mkdtempSync(path.join(os.tmpdir(), "horsie-device-e2e-"));
  const port = await freePort();
  baseURL = `http://127.0.0.1:${port}`;
  const configPath = path.join(root, "config.json");
  fs.writeFileSync(
    configPath,
    JSON.stringify({
      storage: {
        state_dir: path.join(root, "state"),
        data_dir: path.join(root, "data"),
      },
      auth: { enabled: true },
    }),
  );
  const out = fs.openSync(path.join(root, "server.log"), "a");
  proc = spawn(
    path.join(REPO_ROOT, "target", "debug", "horsie-server"),
    ["--config", configPath, "--addr", `127.0.0.1:${port}`, "--web", path.join(WEB_DIR, "dist")],
    { stdio: ["ignore", out, out] },
  );
  await waitFor(async () => (await fetch(`${baseURL}/api/health`)).ok, {
    timeoutMs: 30_000,
    label: "device server /api/health",
  });
  const pwFile = path.join(root, "state", "server", "initial-admin-password");
  await waitFor(async () => fs.existsSync(pwFile), {
    timeoutMs: 10_000,
    label: "initial-admin-password file",
  });
  password = fs.readFileSync(pwFile, "utf8").trim();
});

test.afterAll(() => {
  proc?.kill("SIGKILL");
  fs.rmSync(root, { recursive: true, force: true });
});

test("approving a device code in the browser lets the waiting CLI collect tokens", async ({
  page,
}) => {
  // What `horsie auth login` does first.
  const started = await (
    await fetch(`${baseURL}/api/auth/device/code`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: "{}",
    })
  ).json();
  expect(started.userCode).toMatch(/^[A-Z0-9]{4}-[A-Z0-9]{4}$/);

  // The human logs in and approves.
  await page.goto(`${baseURL}/auth/device?code=${started.userCode}`);
  await page.getByTestId("login-password").fill(password);
  await page.getByTestId("login-submit").click();
  await expect(page.getByTestId("device-page")).toBeVisible();
  await expect(page.getByTestId("device-code")).toHaveValue(started.userCode);
  await page.getByTestId("device-approve").click();
  await expect(page.getByTestId("device-approved")).toBeVisible();

  // The CLI's next poll gets its tokens. An approved code skips the poll
  // floor, so there is nothing to wait for.
  const res = await fetch(`${baseURL}/api/auth/device/token`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ deviceCode: started.deviceCode }),
  });
  expect(res.status).toBe(200);
  const pair = await res.json();
  expect(pair.accessToken).toMatch(/^hsk_usr_/);

  // And that token opens the API.
  const sessions = await fetch(`${baseURL}/api/sessions`, {
    headers: { authorization: `Bearer ${pair.accessToken}` },
  });
  expect(sessions.status).toBe(200);
});
```

- [ ] **Step 2: Run the e2e suite**

Run: `cd clients/web && bun run build && HORSIE_E2E_SKIP_BUILD=1 bun run test:e2e`
Expected: PASS, including the new spec.

- [ ] **Step 3: Document the CLI login**

In `docs/guide/getting-started.md`, add a step before the first `horsie connect`/`horsie session` usage:

```markdown
## Log in

If the server has authentication on (the default), authorize this machine once:

    horsie auth login --server http://localhost:3789

It prints a URL and a short code. Open the URL, check the code matches, and
approve. Credentials are stored in `~/.config/horsie/credentials.json`
(owner-readable only) and refreshed automatically.

`horsie auth status` shows which servers you are logged in to, and
`horsie auth logout --server <url>` forgets one. For scripts, set
`HORSIE_TOKEN` instead of logging in.
```

Match the surrounding heading level and prose style — read the file first.

- [ ] **Step 4: Document the environment variable**

In `docs/guide/settings-reference.md`'s environment-variable table, add:

```markdown
| `HORSIE_TOKEN` | Bearer token the CLI sends instead of reading `~/.config/horsie/credentials.json`. For scripts and CI. |
```

- [ ] **Step 5: Full verification**

Run: `cargo fmt --all && make check`
Expected: PASS.

Run: `cd clients/web && bun run generate-types && bun run typecheck && bun run build && git status --short`
Expected: PASS with no codegen drift.

- [ ] **Step 6: Verify the real flow by hand**

Start a server with auth on, run `cargo run -p horsie -- auth login --server http://127.0.0.1:<port>`, approve in a browser, then confirm `horsie auth status` shows the server and that `horsie session tail` no longer 401s.

- [ ] **Step 7: Open the pull request**

```bash
git push -u origin feat/cli-auth
gh pr create --title "Auth B: CLI login via device flow" --body "..."
```

Body must state: closes #108, part of #106; the device grant's shape rather than RFC 8628 on the wire and why; access/refresh TTLs and rotation with chain revocation on replay; that `horsie connect` credentials are deliberately left to #109.

- [ ] **Step 8: Confirm CI is green**

Run: `gh pr checks --watch`
