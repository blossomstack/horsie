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
        use std::str::FromStr;
        let tmp = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}/t.db", tmp.path().display());
        let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
            .unwrap()
            .create_if_missing(true)
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = sqlx::sqlite::SqlitePool::connect_with(opts).await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
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
}
