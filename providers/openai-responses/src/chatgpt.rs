//! ChatGPT-plan credentials: device-code login, storage handoff, and refresh.
//!
//! Codex's OAuth client is OpenAI's own and its redirect URIs are
//! `http://localhost:1455/auth/callback` and OpenAI's device callback — neither
//! of which a deployed horsie can receive. So login here is **device code**:
//! every call is outbound to `auth.openai.com`, and the browser is tied to the
//! server only by the user typing an 8-character code. No callback URL, no
//! inbound traffic, nothing to configure in a reverse proxy.
//!
//! horsie identifies itself as `horsie` in `originator`. It never sends
//! `x-oai-attestation`: that header's value is minted by a first-party OpenAI
//! client, and forging the envelope would make this a client evading an
//! integrity control rather than one being honest about who it is.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use horsie_agentcore::LlmError;
use std::sync::{Arc, RwLock};

/// OpenAI's own Codex OAuth client. Third parties cannot register one — there
/// is no allocation mechanism — so this constant is the only usable client id.
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const DEFAULT_ISSUER: &str = "https://auth.openai.com";
/// Where a ChatGPT-plan request goes. Not `api.openai.com`: a subscription is
/// only spendable through the Codex backend.
pub const CHATGPT_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
/// How we identify ourselves. opencode sends its own name and is not blocked;
/// impersonating `codex_cli_rs` would be a lie we do not need to tell.
pub const ORIGINATOR: &str = "horsie";
/// Refresh this long before the access token actually expires, so a turn that
/// starts just under the wire does not die mid-stream.
const REFRESH_SKEW_SECS: i64 = 60;

/// A ChatGPT credential as persisted by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredTokens {
    pub access: String,
    pub refresh: String,
    /// Unix epoch seconds.
    pub expires_at: i64,
    pub account_id: String,
}

/// Where refreshed tokens go. Implemented by the server's config store; the
/// provider itself has no idea what a database is.
///
/// Async because the only real implementation writes to a database, and a
/// fire-and-forget spawn could lose a rotated refresh token — which would cost
/// the operator a fresh sign-in for no visible reason.
#[async_trait::async_trait]
pub trait TokenStore: Send + Sync + std::fmt::Debug {
    async fn save(&self, tokens: &StoredTokens) -> Result<(), String>;
}

/// A live ChatGPT credential that refreshes itself.
///
/// This has to own its refresh because the provider registry is built
/// synchronously and swapped wholesale: there is no other moment at which an
/// hour-old access token could be renewed.
#[derive(Debug)]
pub struct ChatGptTokens {
    state: RwLock<StoredTokens>,
    store: Arc<dyn TokenStore>,
    issuer: String,
    http: reqwest::Client,
}

impl ChatGptTokens {
    #[must_use]
    pub fn new(
        tokens: StoredTokens,
        store: Arc<dyn TokenStore>,
        issuer: impl Into<String>,
    ) -> Self {
        Self {
            state: RwLock::new(tokens),
            store,
            issuer: issuer.into(),
            http: reqwest::Client::new(),
        }
    }

    fn read(&self) -> StoredTokens {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The account id, which rides on every request as `ChatGPT-Account-ID`.
    #[must_use]
    pub fn account_id(&self) -> String {
        self.read().account_id
    }

    /// A usable access token, refreshing first if the current one is spent.
    pub async fn access_token(&self, now: i64) -> Result<String, LlmError> {
        let current = self.read();
        if current.expires_at - REFRESH_SKEW_SECS > now {
            return Ok(current.access);
        }
        self.refresh(now).await
    }

    /// Force a refresh — used when a request comes back 401 despite an
    /// apparently valid expiry, which happens when a token is revoked early.
    pub async fn refresh(&self, now: i64) -> Result<String, LlmError> {
        let current = self.read();
        let refreshed = refresh_tokens(&self.http, &self.issuer, &current.refresh, now).await?;
        // The account id never comes back on a refresh; keep the one we have.
        let next = StoredTokens {
            account_id: current.account_id.clone(),
            ..refreshed
        };
        // A failed write is logged, not fatal: the token in hand is valid, so
        // the turn should proceed. The cost is one more refresh after a restart.
        if let Err(e) = self.store.save(&next).await {
            tracing::error!(error = %e, "could not persist the refreshed ChatGPT token");
        }
        let access = next.access.clone();
        *self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = next;
        Ok(access)
    }
}

/// Exchange a refresh token for a fresh pair.
///
/// **Only a 4xx invalidates a credential.** A 5xx or a network error means the
/// path to OpenAI is broken, not that the login is bad — #164 was exactly this:
/// a Caddy 502 made horsie delete a perfectly valid login. So the error kinds
/// are distinguished here, and callers key their "sign in again" behaviour off
/// `LlmError::ApiError` alone.
pub async fn refresh_tokens(
    http: &reqwest::Client,
    issuer: &str,
    refresh_token: &str,
    now: i64,
) -> Result<StoredTokens, LlmError> {
    let resp = http
        .post(format!("{issuer}/oauth/token"))
        .header("originator", ORIGINATOR)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", CLIENT_ID),
        ])
        .send()
        .await
        .map_err(|e| LlmError::Network(Box::new(e)))?;

    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();

    if (400..500).contains(&status) {
        return Err(LlmError::ApiError {
            status,
            message: format!("ChatGPT token refresh was rejected: {body}"),
        });
    }
    if status >= 500 {
        // Transient: the caller must keep the stored refresh token.
        return Err(LlmError::Overloaded);
    }

    tokens_from_token_response(&body, refresh_token, now)
}

/// Parse a token-endpoint response into a storable credential.
///
/// `previous_refresh` is used when the response omits `refresh_token`, which
/// OpenAI does when the existing one is still valid.
fn tokens_from_token_response(
    body: &str,
    previous_refresh: &str,
    now: i64,
) -> Result<StoredTokens, LlmError> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| LlmError::ApiError {
        status: 502,
        message: format!("token response was not JSON ({e})"),
    })?;

    let access = v["access_token"]
        .as_str()
        .ok_or_else(|| LlmError::ApiError {
            status: 502,
            message: "token response carried no access_token".to_string(),
        })?
        .to_string();

    let refresh = v["refresh_token"]
        .as_str()
        .unwrap_or(previous_refresh)
        .to_string();

    // A missing `expires_in` is treated as an hour — the documented default —
    // rather than as "never expires", which would strand the credential.
    let expires_in = v["expires_in"].as_i64().unwrap_or(3600);

    let account_id = v["id_token"]
        .as_str()
        .and_then(account_id_from_id_token)
        .unwrap_or_default();

    Ok(StoredTokens {
        access,
        refresh,
        expires_at: now + expires_in,
        account_id,
    })
}

/// Pull the ChatGPT account id out of an id_token's claims.
///
/// Three shapes are in the wild, in the order Codex and opencode both try them:
/// a top-level `chatgpt_account_id`, the same key namespaced under
/// `https://api.openai.com/auth`, and finally the first organization id.
#[must_use]
pub fn account_id_from_id_token(id_token: &str) -> Option<String> {
    let payload = id_token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;

    claims["chatgpt_account_id"]
        .as_str()
        .or_else(|| claims["https://api.openai.com/auth"]["chatgpt_account_id"].as_str())
        .or_else(|| claims["organizations"][0]["id"].as_str())
        .map(str::to_string)
}

/// A device-code login waiting for the person to approve it in a browser.
#[derive(Debug, Clone)]
pub struct DeviceLogin {
    pub device_auth_id: String,
    /// What the person types at [`DeviceLogin::verification_url`].
    pub user_code: String,
    pub interval_secs: u64,
}

impl DeviceLogin {
    /// Where the person approves this login. On OpenAI's own site — horsie
    /// never sees the ChatGPT password.
    #[must_use]
    pub fn verification_url(issuer: &str) -> String {
        format!("{issuer}/codex/device")
    }
}

/// Ask OpenAI for a user code. Outbound only; nothing calls horsie back.
pub async fn start_device_login(
    http: &reqwest::Client,
    issuer: &str,
) -> Result<DeviceLogin, LlmError> {
    let resp = http
        .post(format!("{issuer}/api/accounts/deviceauth/usercode"))
        .header("originator", ORIGINATOR)
        .json(&serde_json::json!({ "client_id": CLIENT_ID }))
        .send()
        .await
        .map_err(|e| LlmError::Network(Box::new(e)))?;

    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    if status >= 400 {
        return Err(LlmError::ApiError {
            status,
            message: format!("could not start a ChatGPT device login: {body}"),
        });
    }

    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| LlmError::ApiError {
        status: 502,
        message: format!("device-code response was not JSON ({e})"),
    })?;

    Ok(DeviceLogin {
        device_auth_id: v["device_auth_id"].as_str().unwrap_or_default().to_string(),
        user_code: v["user_code"].as_str().unwrap_or_default().to_string(),
        // `interval` arrives as a string. Under 1s would hammer the endpoint
        // into rate limiting the login it is trying to complete.
        interval_secs: v["interval"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .or_else(|| v["interval"].as_u64())
            .unwrap_or(5)
            .max(1),
    })
}

/// Poll once. `Ok(None)` means "not approved yet, ask again".
pub async fn poll_device_login(
    http: &reqwest::Client,
    issuer: &str,
    login: &DeviceLogin,
    now: i64,
) -> Result<Option<StoredTokens>, LlmError> {
    let resp = http
        .post(format!("{issuer}/api/accounts/deviceauth/token"))
        .header("originator", ORIGINATOR)
        .json(&serde_json::json!({
            "device_auth_id": login.device_auth_id,
            "user_code": login.user_code,
        }))
        .send()
        .await
        .map_err(|e| LlmError::Network(Box::new(e)))?;

    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();

    // Anything that is not a success is "still waiting" as far as the operator
    // is concerned: the endpoint answers 4xx while the code is unapproved.
    if status >= 400 {
        return Ok(None);
    }

    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| LlmError::ApiError {
        status: 502,
        message: format!("device-token response was not JSON ({e})"),
    })?;

    let (Some(code), Some(verifier)) = (
        v["authorization_code"].as_str(),
        v["code_verifier"].as_str(),
    ) else {
        return Ok(None);
    };

    exchange_device_code(http, issuer, code, verifier, now)
        .await
        .map(Some)
}

/// Trade an approved device authorization for tokens.
///
/// The `redirect_uri` here is OpenAI's own device callback: a protocol
/// formality that nothing ever navigates to. It is why this flow works for a
/// server that cannot receive a redirect at all.
async fn exchange_device_code(
    http: &reqwest::Client,
    issuer: &str,
    code: &str,
    code_verifier: &str,
    now: i64,
) -> Result<StoredTokens, LlmError> {
    let redirect_uri = format!("{issuer}/deviceauth/callback");
    let resp = http
        .post(format!("{issuer}/oauth/token"))
        .header("originator", ORIGINATOR)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri.as_str()),
            ("client_id", CLIENT_ID),
            ("code_verifier", code_verifier),
        ])
        .send()
        .await
        .map_err(|e| LlmError::Network(Box::new(e)))?;

    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    if status >= 400 {
        return Err(LlmError::ApiError {
            status,
            message: format!("ChatGPT token exchange failed: {body}"),
        });
    }

    tokens_from_token_response(&body, "", now)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Debug, Default)]
    pub(crate) struct RecordingStore(Mutex<Vec<StoredTokens>>);

    impl RecordingStore {
        pub(crate) fn saved(&self) -> Vec<StoredTokens> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    #[async_trait::async_trait]
    impl TokenStore for RecordingStore {
        async fn save(&self, tokens: &StoredTokens) -> Result<(), String> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(tokens.clone());
            Ok(())
        }
    }

    fn id_token_with(claims: serde_json::Value) -> String {
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
        format!("header.{payload}.signature")
    }

    /// A fake issuer. `token_status` decides what `/oauth/token` answers, so a
    /// test can pick the failure mode it cares about.
    pub(crate) async fn mock_issuer(token_status: u16, approved: bool) -> String {
        use axum::{Router, routing::post};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");

        let token = move || async move {
            let id_token = id_token_with(serde_json::json!({ "chatgpt_account_id": "acct_1" }));
            let body = serde_json::json!({
                "access_token": "access-2",
                "refresh_token": "refresh-2",
                "expires_in": 3600,
                "id_token": id_token,
            });
            (
                axum::http::StatusCode::from_u16(token_status).unwrap(),
                axum::Json(body),
            )
        };

        let usercode = || async {
            axum::Json(serde_json::json!({
                "device_auth_id": "dev-1",
                "user_code": "ABCD-EFGH",
                "interval": "5",
            }))
        };

        let device_token = move || async move {
            if approved {
                (
                    axum::http::StatusCode::OK,
                    axum::Json(serde_json::json!({
                        "authorization_code": "auth-code",
                        "code_verifier": "verifier",
                    })),
                )
            } else {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({ "error": "authorization_pending" })),
                )
            }
        };

        let app = Router::new()
            .route("/oauth/token", post(token))
            .route("/api/accounts/deviceauth/usercode", post(usercode))
            .route("/api/accounts/deviceauth/token", post(device_token));

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        base
    }

    fn tokens(expires_at: i64) -> StoredTokens {
        StoredTokens {
            access: "access-1".into(),
            refresh: "refresh-1".into(),
            expires_at,
            account_id: "acct_1".into(),
        }
    }

    #[tokio::test]
    async fn a_live_token_is_returned_without_contacting_the_issuer() {
        let store = Arc::new(RecordingStore::default());
        // An issuer that would 500 if it were called at all.
        let issuer = mock_issuer(500, false).await;
        let t = ChatGptTokens::new(tokens(10_000), store.clone(), issuer);

        assert_eq!(t.access_token(1_000).await.unwrap(), "access-1");
        assert!(store.saved().is_empty(), "no refresh, so nothing to save");
    }

    #[tokio::test]
    async fn an_expired_token_refreshes_and_persists() {
        let store = Arc::new(RecordingStore::default());
        let issuer = mock_issuer(200, false).await;
        let t = ChatGptTokens::new(tokens(1_000), store.clone(), issuer);

        assert_eq!(t.access_token(1_000).await.unwrap(), "access-2");

        let saved = store.saved();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].access, "access-2");
        assert_eq!(saved[0].refresh, "refresh-2");
        assert_eq!(saved[0].expires_at, 1_000 + 3600);
        // The refresh response carries no account id of its own; the stored one
        // must survive, or every refresh would silently drop the header.
        assert_eq!(saved[0].account_id, "acct_1");
    }

    /// #164: a 502 from a proxy once deleted a valid login. A 5xx must fail the
    /// turn and leave the credential alone.
    #[tokio::test]
    async fn a_5xx_refresh_fails_without_touching_the_stored_credential() {
        let store = Arc::new(RecordingStore::default());
        let issuer = mock_issuer(503, false).await;
        let t = ChatGptTokens::new(tokens(1_000), store.clone(), issuer);

        let err = t.access_token(1_000).await.expect_err("must fail");

        assert!(matches!(err, LlmError::Overloaded), "got {err:?}");
        assert!(
            store.saved().is_empty(),
            "a transient failure must not rewrite the credential"
        );
    }

    #[tokio::test]
    async fn a_4xx_refresh_is_terminal_and_says_so() {
        let store = Arc::new(RecordingStore::default());
        let issuer = mock_issuer(400, false).await;
        let t = ChatGptTokens::new(tokens(1_000), store.clone(), issuer);

        let err = t.access_token(1_000).await.expect_err("must fail");

        assert!(
            matches!(err, LlmError::ApiError { status: 400, .. }),
            "got {err:?}"
        );
        assert!(store.saved().is_empty());
    }

    #[tokio::test]
    async fn polling_reports_pending_until_the_person_approves() {
        let http = reqwest::Client::new();
        let issuer = mock_issuer(200, false).await;
        let login = start_device_login(&http, &issuer).await.unwrap();

        assert_eq!(login.user_code, "ABCD-EFGH");
        assert_eq!(login.interval_secs, 5);
        assert_eq!(
            poll_device_login(&http, &issuer, &login, 1_000)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn an_approved_login_exchanges_into_tokens() {
        let http = reqwest::Client::new();
        let issuer = mock_issuer(200, true).await;
        let login = start_device_login(&http, &issuer).await.unwrap();

        let got = poll_device_login(&http, &issuer, &login, 1_000)
            .await
            .unwrap()
            .expect("approved logins yield tokens");

        assert_eq!(got.access, "access-2");
        assert_eq!(got.account_id, "acct_1");
        assert_eq!(got.expires_at, 1_000 + 3600);
    }

    #[test]
    fn the_account_id_is_read_from_any_of_its_three_claim_shapes() {
        let direct = id_token_with(serde_json::json!({ "chatgpt_account_id": "a" }));
        assert_eq!(account_id_from_id_token(&direct).as_deref(), Some("a"));

        let namespaced = id_token_with(serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "b" }
        }));
        assert_eq!(account_id_from_id_token(&namespaced).as_deref(), Some("b"));

        let org = id_token_with(serde_json::json!({ "organizations": [{ "id": "c" }] }));
        assert_eq!(account_id_from_id_token(&org).as_deref(), Some("c"));

        assert_eq!(account_id_from_id_token("not-a-jwt"), None);
    }

    #[test]
    fn a_response_without_a_new_refresh_token_keeps_the_old_one() {
        let body = serde_json::json!({ "access_token": "a", "expires_in": 60 }).to_string();

        let t = tokens_from_token_response(&body, "keep-me", 100).unwrap();

        assert_eq!(t.refresh, "keep-me");
        assert_eq!(t.expires_at, 160);
    }
}
