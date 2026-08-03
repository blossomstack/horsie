//! SQLite storage for the admin account and every issued token, sharing the
//! config store's pool. Policy lives in `service.rs`; this layer only reads and
//! writes rows.

use crate::auth::{Principal, TokenKind};
use crate::db::Db;
use sqlx::Row;
use sqlx::any::AnyRow;

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

/// A token as listed in the UI: never the secret, which exists only at the
/// moment of minting.
#[derive(Clone, Debug, PartialEq)]
pub struct TokenSummary {
    pub id: String,
    pub label: Option<String>,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

pub struct AuthStore {
    db: Db,
}

impl AuthStore {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    // --- users ---

    pub async fn user_count(&self) -> Result<i64, String> {
        let row = sqlx::query(&self.db.q("SELECT COUNT(*) AS n FROM auth_users"))
            .fetch_one(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        row.try_get::<i64, _>("n").map_err(|e| e.to_string())
    }

    pub async fn get_user(&self, username: &str) -> Result<Option<UserRow>, String> {
        let row = sqlx::query(
            &self
                .db
                .q("SELECT id, username, password_hash, password_is_generated \
             FROM auth_users WHERE username = ?"),
        )
        .bind(username)
        .fetch_optional(self.db.pool())
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
        // `RETURNING id` rather than a follow-up `last_insert_id`: sqlx's Any
        // driver reports that as NULL on SQLite regardless of the backend.
        let row = sqlx::query(&self.db.q("INSERT INTO auth_users \
             (username, password_hash, password_is_generated, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?) RETURNING id"))
        .bind(username)
        .bind(password_hash)
        .bind(i64::from(generated))
        .bind(now)
        .bind(now)
        .fetch_one(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        row.try_get::<i64, _>("id").map_err(|e| e.to_string())
    }

    /// Replace the password. Always clears `password_is_generated`: the only
    /// way to reach here is a deliberate change.
    pub async fn set_password(
        &self,
        username: &str,
        password_hash: &str,
        now: i64,
    ) -> Result<(), String> {
        sqlx::query(&self.db.q(
            "UPDATE auth_users SET password_hash = ?, password_is_generated = 0, \
             updated_at = ? WHERE username = ?",
        ))
        .bind(password_hash)
        .bind(now)
        .bind(username)
        .execute(self.db.pool())
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
        sqlx::query(&self.db.q("INSERT INTO auth_tokens \
             (id, kind, principal, token_hash, label, chain_id, expires_at, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)"))
        .bind(id)
        .bind(kind.as_db())
        .bind(principal.to_db())
        .bind(hash)
        .bind(label)
        .bind(chain_id)
        .bind(expires_at)
        .bind(now)
        .execute(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// The live token with this hash, or `None` when absent, revoked, or
    /// expired. Expiry is compared in SQL, which is why these columns are
    /// INTEGER.
    pub async fn lookup_token(&self, hash: &[u8], now: i64) -> Result<Option<TokenRow>, String> {
        let row = sqlx::query(&self.db.q(
            "SELECT id, kind, principal, label, last_used_at FROM auth_tokens \
             WHERE token_hash = ? AND revoked_at IS NULL \
             AND (expires_at IS NULL OR expires_at > ?)",
        ))
        .bind(hash)
        .bind(now)
        .fetch_optional(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_token).transpose()
    }

    pub async fn revoke_token(&self, id: &str, now: i64) -> Result<(), String> {
        sqlx::query(
            &self
                .db
                .q("UPDATE auth_tokens SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL"),
        )
        .bind(now)
        .bind(id)
        .execute(self.db.pool())
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
        sqlx::query(&self.db.q("UPDATE auth_tokens SET revoked_at = ? \
             WHERE principal = ? AND kind = ? AND revoked_at IS NULL \
             AND (? IS NULL OR id <> ?)"))
        .bind(now)
        .bind(principal.to_db())
        .bind(kind.as_db())
        .bind(except_id)
        .bind(except_id)
        .execute(self.db.pool())
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
        sqlx::query(
            &self
                .db
                .q("UPDATE auth_tokens SET last_used_at = ? WHERE id = ?"),
        )
        .bind(now)
        .bind(id)
        .execute(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        Ok(true)
    }
    /// Live tokens of one kind, newest first. Used to list machine tokens; the
    /// hash is deliberately not selected.
    pub async fn list_tokens_of_kind(&self, kind: TokenKind) -> Result<Vec<TokenSummary>, String> {
        let rows = sqlx::query(&self.db.q(
            "SELECT id, label, created_at, last_used_at FROM auth_tokens \
             WHERE kind = ? AND revoked_at IS NULL ORDER BY created_at DESC, id DESC",
        ))
        .bind(kind.as_db())
        .fetch_all(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        rows.iter()
            .map(|row| {
                Ok(TokenSummary {
                    id: row.try_get("id").map_err(|e: sqlx::Error| e.to_string())?,
                    label: row
                        .try_get("label")
                        .map_err(|e: sqlx::Error| e.to_string())?,
                    created_at: row
                        .try_get("created_at")
                        .map_err(|e: sqlx::Error| e.to_string())?,
                    last_used_at: row
                        .try_get("last_used_at")
                        .map_err(|e: sqlx::Error| e.to_string())?,
                })
            })
            .collect()
    }

    // --- device codes ---

    pub async fn insert_device_code(
        &self,
        device_hash: &[u8],
        user_code: &str,
        now: i64,
        expires_at: i64,
    ) -> Result<(), String> {
        sqlx::query(&self.db.q("INSERT INTO auth_device_codes \
             (device_code_hash, user_code, created_at, expires_at) VALUES (?, ?, ?, ?)"))
        .bind(device_hash)
        .bind(user_code)
        .bind(now)
        .bind(expires_at)
        .execute(self.db.pool())
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
        let row = sqlx::query(&self.db.q(
            "SELECT user_code, principal, expires_at, approved_at, denied_at, \
             consumed_at, last_polled_at FROM auth_device_codes WHERE device_code_hash = ?",
        ))
        .bind(device_hash)
        .fetch_optional(self.db.pool())
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
        let res = sqlx::query(&self.db.q(
            "UPDATE auth_device_codes SET approved_at = ?, principal = ? \
             WHERE user_code = ? AND expires_at > ? \
             AND approved_at IS NULL AND denied_at IS NULL",
        ))
        .bind(now)
        .bind(principal.to_db())
        .bind(user_code)
        .bind(now)
        .execute(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn deny_device_code(&self, user_code: &str, now: i64) -> Result<bool, String> {
        let res = sqlx::query(&self.db.q("UPDATE auth_device_codes SET denied_at = ? \
             WHERE user_code = ? AND expires_at > ? \
             AND approved_at IS NULL AND denied_at IS NULL"))
        .bind(now)
        .bind(user_code)
        .bind(now)
        .execute(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn mark_device_polled(&self, device_hash: &[u8], now: i64) -> Result<(), String> {
        sqlx::query(
            &self
                .db
                .q("UPDATE auth_device_codes SET last_polled_at = ? WHERE device_code_hash = ?"),
        )
        .bind(now)
        .bind(device_hash)
        .execute(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn consume_device_code(&self, device_hash: &[u8], now: i64) -> Result<(), String> {
        sqlx::query(
            &self
                .db
                .q("UPDATE auth_device_codes SET consumed_at = ? WHERE device_code_hash = ?"),
        )
        .bind(now)
        .bind(device_hash)
        .execute(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Housekeeping, called whenever a code is issued. Device codes are
    /// short-lived and never read after expiry, so nothing is lost.
    pub async fn purge_expired_device_codes(&self, now: i64) -> Result<(), String> {
        sqlx::query(
            &self
                .db
                .q("DELETE FROM auth_device_codes WHERE expires_at <= ?"),
        )
        .bind(now)
        .execute(self.db.pool())
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
        let row = sqlx::query(&self.db.q(
            "SELECT id, kind, principal, chain_id, expires_at, revoked_at \
             FROM auth_tokens WHERE token_hash = ?",
        ))
        .bind(hash)
        .fetch_optional(self.db.pool())
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
            &self.db.q(
                "UPDATE auth_tokens SET revoked_at = ? WHERE chain_id = ? AND revoked_at IS NULL",
            ),
        )
        .bind(now)
        .bind(chain_id)
        .execute(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }
}

fn row_to_user(row: &AnyRow) -> Result<UserRow, String> {
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

fn row_to_token(row: &AnyRow) -> Result<TokenRow, String> {
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::auth::generate;

    /// A migrated, file-backed temp database. NOT `sqlite::memory:` — every
    /// pooled connection would get its own private database, so a row written
    /// through one connection is invisible to the next. Same shape as
    /// `memory::store`'s test helper.
    async fn store() -> (AuthStore, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let pool = crate::db::testing::db().await;
        (AuthStore::new(pool), tmp)
    }

    #[tokio::test]
    async fn creates_and_reads_back_the_admin_user() {
        let (s, _tmp) = store().await;
        assert_eq!(s.user_count().await.unwrap(), 0);
        assert!(s.get_user("admin").await.unwrap().is_none());

        let id = s
            .create_user("admin", "phc-hash", true, 1000)
            .await
            .unwrap();
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
            "id-live",
            TokenKind::Web,
            &Principal::User(1),
            &live.hash,
            None,
            None,
            Some(9_999_999_999),
            1000,
        )
        .await
        .unwrap();

        let found = s.lookup_token(&live.hash, 1001).await.unwrap().unwrap();
        assert_eq!(found.id, "id-live");
        assert_eq!(found.kind, TokenKind::Web);
        assert_eq!(found.principal, Principal::User(1));

        // Expired.
        let old = generate(TokenKind::Web);
        s.insert_token(
            "id-old",
            TokenKind::Web,
            &Principal::User(1),
            &old.hash,
            None,
            None,
            Some(500),
            100,
        )
        .await
        .unwrap();
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
            "id-agent",
            TokenKind::Agent,
            &Principal::User(1),
            &t.hash,
            Some("laptop"),
            None,
            None,
            1000,
        )
        .await
        .unwrap();
        let found = s
            .lookup_token(&t.hash, 9_999_999_999)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.label.as_deref(), Some("laptop"));
    }

    #[tokio::test]
    async fn revoking_a_kind_leaves_the_excepted_token_and_other_kinds_alone() {
        let (s, _tmp) = store().await;
        let (a, b, c) = (
            generate(TokenKind::Web),
            generate(TokenKind::Web),
            generate(TokenKind::Agent),
        );
        for (id, kind, tok) in [
            ("a", TokenKind::Web, &a),
            ("b", TokenKind::Web, &b),
            ("c", TokenKind::Agent, &c),
        ] {
            s.insert_token(
                id,
                kind,
                &Principal::User(1),
                &tok.hash,
                None,
                None,
                None,
                1000,
            )
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
        s.insert_token(
            "id",
            TokenKind::Web,
            &Principal::User(1),
            &t.hash,
            None,
            None,
            None,
            1000,
        )
        .await
        .unwrap();

        // First touch always writes.
        assert!(s.touch_token("id", None, 1000).await.unwrap());
        // Within the minute: skipped.
        assert!(!s.touch_token("id", Some(1000), 1030).await.unwrap());
        // Past the minute: written.
        assert!(s.touch_token("id", Some(1000), 1061).await.unwrap());
    }

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
            s.get_device_code(b"d2")
                .await
                .unwrap()
                .unwrap()
                .last_polled_at,
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

    #[tokio::test]
    async fn agent_tokens_list_newest_first_and_drop_out_when_revoked() {
        let (s, _tmp) = store().await;
        let (a, b, other) = (
            generate(TokenKind::Agent),
            generate(TokenKind::Agent),
            generate(TokenKind::Access),
        );
        s.insert_token(
            "t-a",
            TokenKind::Agent,
            &Principal::User(1),
            &a.hash,
            Some("laptop"),
            None,
            None,
            1000,
        )
        .await
        .unwrap();
        s.insert_token(
            "t-b",
            TokenKind::Agent,
            &Principal::User(1),
            &b.hash,
            Some("ci"),
            None,
            None,
            2000,
        )
        .await
        .unwrap();
        // A different kind must not show up in the machine-token list.
        s.insert_token(
            "t-c",
            TokenKind::Access,
            &Principal::User(1),
            &other.hash,
            None,
            None,
            None,
            3000,
        )
        .await
        .unwrap();

        let listed = s.list_tokens_of_kind(TokenKind::Agent).await.unwrap();
        assert_eq!(
            listed.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["t-b", "t-a"],
            "newest first"
        );
        assert_eq!(listed[0].label.as_deref(), Some("ci"));

        s.revoke_token("t-b", 4000).await.unwrap();
        let listed = s.list_tokens_of_kind(TokenKind::Agent).await.unwrap();
        assert_eq!(
            listed.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["t-a"]
        );
    }
}
