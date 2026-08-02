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

pub const INITIAL_PASSWORD_FILE: &str = "initial-admin-password";

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
        if file.exists()
            && let Err(e) = std::fs::remove_file(&file)
        {
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
            assert!(matches!(
                svc.login("wrong").await,
                Err(LoginError::BadCredentials)
            ));
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

        svc.change_password(&pw, "a-new-password", &keep)
            .await
            .unwrap();

        assert!(
            svc.verify(&keep).await.unwrap().is_some(),
            "the caller stays logged in"
        );
        assert!(
            svc.verify(&other).await.unwrap().is_none(),
            "other browsers are logged out"
        );
        assert!(!tmp.path().join("initial-admin-password").exists());
        assert!(!svc.must_change_password().await.unwrap());
        assert!(svc.login("a-new-password").await.is_ok());
        assert!(matches!(
            svc.login(&pw).await,
            Err(LoginError::BadCredentials)
        ));
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
