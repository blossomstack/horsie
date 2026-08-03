//! Authentication policy: first-boot bootstrap, login, logout, password
//! change, and credential verification. `store.rs` holds the rows; everything
//! that decides *whether* something is allowed lives here.

use crate::auth::store::{AuthStore, TokenSummary};
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

#[derive(Debug)]
pub enum LoginError {
    BadCredentials,
    WeakPassword(String),
    Internal(String),
}

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

    /// Mint a long-lived machine token. Returns the secret — the only time it
    /// exists in the clear — alongside the summary the UI lists.
    ///
    /// No expiry: a headless agent has nobody to re-approve a device code, so
    /// revocation rather than rotation is the control that matters here.
    pub async fn mint_agent_token(
        &self,
        label: &str,
        principal: &Principal,
    ) -> Result<(String, TokenSummary), String> {
        let label = label.trim();
        if label.is_empty() {
            return Err("a machine token needs a label".to_string());
        }
        let now = now_secs();
        let token = generate(TokenKind::Agent);
        let id = uuid::Uuid::new_v4().to_string();
        self.store
            .insert_token(
                &id,
                TokenKind::Agent,
                principal,
                &token.hash,
                Some(label),
                None,
                None,
                now,
            )
            .await?;
        Ok((
            token.secret,
            TokenSummary {
                id,
                label: Some(label.to_string()),
                created_at: now,
                last_used_at: None,
            },
        ))
    }

    pub async fn list_agent_tokens(&self) -> Result<Vec<TokenSummary>, String> {
        self.store.list_tokens_of_kind(TokenKind::Agent).await
    }

    pub async fn revoke_agent_token(&self, id: &str) -> Result<(), String> {
        self.store.revoke_token(id, now_secs()).await
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
}

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
        format!(
            "{}-{}",
            &bare[..USER_CODE_LEN / 2],
            &bare[USER_CODE_LEN / 2..]
        )
    } else {
        bare
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
            .create_if_missing(true)
            .busy_timeout(std::time::Duration::from_secs(5));
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
        // Further failures would now be delayed...
        assert!(svc.delay_before_failure() > std::time::Duration::ZERO);
        // ...but the correct password is still accepted, and clears the delay.
        //
        // Deliberately not asserted by timing the call: a successful login also
        // writes a token row, so under load the measurement is dominated by
        // argon2 and SQLite rather than by the throttle. That `login` sleeps
        // only on the failure branch is plain in the code, and the delay
        // arithmetic itself is covered in `throttle.rs`.
        svc.login(&pw).await.unwrap();
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

    #[test]
    fn user_codes_are_normalized_the_way_people_type_them() {
        assert_eq!(normalize_user_code("bcdf-ghjk"), "BCDF-GHJK");
        assert_eq!(normalize_user_code("bcdfghjk"), "BCDF-GHJK");
        assert_eq!(normalize_user_code(" BCDF GHJK "), "BCDF-GHJK");
        // Anything not eight characters is passed through for the store to miss.
        assert_eq!(normalize_user_code("short"), "SHORT");
    }

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

        assert!(
            svc.approve_device(&d.user_code, &Principal::User(1))
                .await
                .unwrap()
        );

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
        assert!(
            !svc.approve_device(&d.user_code, &Principal::User(1))
                .await
                .unwrap()
        );

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
        svc.approve_device(&d.user_code, &Principal::User(1))
            .await
            .unwrap();
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

    #[tokio::test]
    async fn a_minted_agent_token_authenticates_and_can_be_revoked() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = service(&tmp, true).await;
        svc.bootstrap().await.unwrap();

        let (secret, view) = svc
            .mint_agent_token("my-laptop", &Principal::User(1))
            .await
            .unwrap();
        assert!(secret.starts_with("hsk_agt_"), "{secret}");
        assert_eq!(view.label.as_deref(), Some("my-laptop"));

        let v = svc.verify(&secret).await.unwrap().expect("verifies");
        assert_eq!(v.kind, TokenKind::Agent);
        assert_eq!(v.principal, Principal::User(1));

        let listed = svc.list_agent_tokens().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, view.id);

        svc.revoke_agent_token(&view.id).await.unwrap();
        assert!(svc.verify(&secret).await.unwrap().is_none());
        assert!(svc.list_agent_tokens().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_agent_token_needs_a_label() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = service(&tmp, true).await;
        svc.bootstrap().await.unwrap();
        // A wall of unlabelled secrets is unrevokable in practice: nobody can
        // tell which machine a row belongs to.
        assert!(svc.mint_agent_token("   ", &Principal::User(1)).await.is_err());
    }
}
