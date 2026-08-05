//! ChatGPT device-code sign-in for `kind = "chatgpt"` providers.
//!
//! The flow exists because a deployed horsie cannot receive an OAuth redirect:
//! the Codex OAuth client belongs to OpenAI and its registered redirects are
//! `localhost:1455` and OpenAI's own device callback, neither of which reaches
//! a server behind a public domain. With device code every call is outbound —
//! the operator's browser and this server are linked only by an
//! eight-character user code — so nothing needs a callback URL, an inbound
//! route, or a proxy change.

use crate::config::ConfigStore;
use crate::db::Db;
use horsie_models::settings::SettingsUpdate;
use horsie_openai_responses::chatgpt::{
    DEFAULT_ISSUER, DeviceLogin, StoredTokens, poll_device_login, start_device_login,
};
use sqlx::Row;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// What the UI shows the operator after starting a login.
pub struct StartedLogin {
    pub user_code: String,
    pub verification_url: String,
    pub interval_secs: u64,
}

/// One poll's outcome.
pub enum PollOutcome {
    /// Still waiting for the person to approve the code in their browser.
    Pending,
    /// Signed in; the registry now has a usable provider.
    Complete { account_id: String },
}

#[derive(Debug)]
pub enum LoginError {
    /// No provider by that name.
    UnknownProvider,
    /// The provider exists but is not a ChatGPT-plan provider.
    NotChatGpt,
    /// No login is in flight for this provider.
    NotStarted,
    Upstream(String),
}

pub struct ChatGptLoginService {
    db: Db,
    config: Arc<dyn ConfigStore>,
    http: reqwest::Client,
    issuer: String,
    /// In-flight logins, keyed by provider name. Deliberately in memory: a
    /// device code is valid for minutes, so a restart mid-login is a restart of
    /// the login, not a state-recovery problem.
    pending: RwLock<HashMap<String, DeviceLogin>>,
}

impl ChatGptLoginService {
    #[must_use]
    pub fn new(db: Db, config: Arc<dyn ConfigStore>) -> Self {
        Self {
            db,
            config,
            http: reqwest::Client::new(),
            issuer: DEFAULT_ISSUER.to_string(),
            pending: RwLock::new(HashMap::new()),
        }
    }

    /// Point the service at a different issuer. Tests only — the real one is
    /// OpenAI's, and there is no second deployment of it.
    #[must_use]
    pub fn with_issuer(mut self, issuer: impl Into<String>) -> Self {
        self.issuer = issuer.into();
        self
    }

    async fn require_chatgpt_provider(&self, provider: &str) -> Result<(), LoginError> {
        let row = sqlx::query(&self.db.q("SELECT kind FROM providers WHERE name = ?"))
            .bind(provider)
            .fetch_optional(self.db.pool())
            .await
            .map_err(|e| LoginError::Upstream(e.to_string()))?;

        match row {
            None => Err(LoginError::UnknownProvider),
            Some(r) => {
                let kind: String = r
                    .try_get("kind")
                    .map_err(|e| LoginError::Upstream(e.to_string()))?;
                if kind == "chatgpt" {
                    Ok(())
                } else {
                    Err(LoginError::NotChatGpt)
                }
            }
        }
    }

    /// Ask OpenAI for a user code and hold the handle for polling.
    pub async fn start(&self, provider: &str) -> Result<StartedLogin, LoginError> {
        self.require_chatgpt_provider(provider).await?;

        let login = start_device_login(&self.http, &self.issuer)
            .await
            .map_err(|e| LoginError::Upstream(e.to_string()))?;

        let started = StartedLogin {
            user_code: login.user_code.clone(),
            verification_url: DeviceLogin::verification_url(&self.issuer),
            interval_secs: login.interval_secs,
        };
        self.pending
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(provider.to_string(), login);
        Ok(started)
    }

    /// Poll once. On success the credential is stored and the provider registry
    /// is rebuilt, so the models on this provider work without a restart.
    pub async fn poll(&self, provider: &str) -> Result<PollOutcome, LoginError> {
        self.require_chatgpt_provider(provider).await?;

        let login = self
            .pending
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(provider)
            .cloned()
            .ok_or(LoginError::NotStarted)?;

        let now = now_secs();
        let tokens = poll_device_login(&self.http, &self.issuer, &login, now)
            .await
            .map_err(|e| LoginError::Upstream(e.to_string()))?;

        let Some(tokens) = tokens else {
            return Ok(PollOutcome::Pending);
        };

        self.persist_and_apply(provider, &tokens).await?;
        self.pending
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(provider);

        Ok(PollOutcome::Complete {
            account_id: tokens.account_id,
        })
    }

    async fn persist_and_apply(
        &self,
        provider: &str,
        tokens: &StoredTokens,
    ) -> Result<(), LoginError> {
        crate::config::store::write_provider_oauth(&self.db, provider, tokens)
            .await
            .map_err(|e| LoginError::Upstream(e.to_string()))?;

        // An empty update changes nothing but rebuilds and swaps the registry,
        // which is exactly what a fresh credential needs: the provider was
        // unbuildable a moment ago.
        self.config
            .update(SettingsUpdate {
                providers: None,
                models: None,
                default_vendor: None,
            })
            .await
            .map_err(LoginError::Upstream)?;
        Ok(())
    }

    /// Forget a sign-in. The models on this provider stop working until it is
    /// signed in again — which is the point.
    pub async fn sign_out(&self, provider: &str) -> Result<(), LoginError> {
        self.require_chatgpt_provider(provider).await?;

        sqlx::query(&self.db.q("DELETE FROM provider_oauth WHERE provider = ?"))
            .bind(provider)
            .execute(self.db.pool())
            .await
            .map_err(|e| LoginError::Upstream(e.to_string()))?;
        self.pending
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(provider);
        Ok(())
    }

    /// The signed-in account for a provider, if any. Drives the settings panel.
    pub async fn account_id(&self, provider: &str) -> Result<Option<String>, LoginError> {
        let row = sqlx::query(
            &self
                .db
                .q("SELECT account_id FROM provider_oauth WHERE provider = ?"),
        )
        .bind(provider)
        .fetch_optional(self.db.pool())
        .await
        .map_err(|e| LoginError::Upstream(e.to_string()))?;

        row.map(|r| r.try_get("account_id"))
            .transpose()
            .map_err(|e: sqlx::Error| LoginError::Upstream(e.to_string()))
    }
}

fn now_secs() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    )
    .unwrap_or(i64::MAX)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use crate::config::store::{DbConfigStore, StoreDeps};
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use horsie_models::settings::{ProviderInput, ServerInfo};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A fake `auth.openai.com`. `approve_after_first_poll` makes the first poll
    /// answer "not yet" and the second succeed, which is what a real login looks
    /// like from the server's side.
    async fn mock_issuer(approve_after_first_poll: bool) -> String {
        use axum::{Router, routing::post};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let polled = Arc::new(AtomicBool::new(false));

        let usercode = || async {
            axum::Json(serde_json::json!({
                "device_auth_id": "dev-1",
                "user_code": "ABCD-EFGH",
                "interval": "5",
            }))
        };

        let device_token = {
            let polled = polled.clone();
            move || {
                let polled = polled.clone();
                async move {
                    let first = !polled.swap(true, Ordering::SeqCst);
                    if approve_after_first_poll && first {
                        return (
                            axum::http::StatusCode::BAD_REQUEST,
                            axum::Json(serde_json::json!({ "error": "authorization_pending" })),
                        );
                    }
                    (
                        axum::http::StatusCode::OK,
                        axum::Json(serde_json::json!({
                            "authorization_code": "auth-code",
                            "code_verifier": "verifier",
                        })),
                    )
                }
            }
        };

        let token = || async {
            let claims = serde_json::json!({ "chatgpt_account_id": "acct_42" }).to_string();
            let id_token = format!("h.{}.s", URL_SAFE_NO_PAD.encode(claims));
            axum::Json(serde_json::json!({
                "access_token": "access-1",
                "refresh_token": "refresh-1",
                "expires_in": 3600,
                "id_token": id_token,
            }))
        };

        let app = Router::new()
            .route("/api/accounts/deviceauth/usercode", post(usercode))
            .route("/api/accounts/deviceauth/token", post(device_token))
            .route("/oauth/token", post(token));

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        base
    }

    struct Fixture {
        service: ChatGptLoginService,
        opened: crate::config::OpenedConfig,
        _tmp: tempfile::TempDir,
    }

    async fn fixture(approve_after_first_poll: bool) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}?mode=rwc", tmp.path().join("s.db").display());
        let opened = DbConfigStore::open(
            &url,
            StoreDeps {
                info: ServerInfo {
                    config_path: String::new(),
                    database: String::new(),
                    state_dir: String::new(),
                    data_dir: String::new(),
                    plugins_dir: String::new(),
                    version: "test".into(),
                    journal_backend: "file".into(),
                },
            },
        )
        .await
        .unwrap();

        let issuer = mock_issuer(approve_after_first_poll).await;
        let service =
            ChatGptLoginService::new(opened.db.clone(), opened.store.clone()).with_issuer(issuer);

        Fixture {
            service,
            opened,
            _tmp: tmp,
        }
    }

    async fn add_provider(f: &Fixture, name: &str, kind: &str) {
        f.opened
            .store
            .update(SettingsUpdate {
                providers: Some(vec![ProviderInput {
                    name: name.into(),
                    kind: kind.into(),
                    base_url: None,
                    api_key: (kind != "chatgpt").then(|| "sk-x".to_string()),
                    keep_thinking_signature: None,
                }]),
                models: Some(vec![]),
                default_vendor: None,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_login_against_an_unknown_provider_is_not_found() {
        let f = fixture(false).await;

        assert!(matches!(
            f.service.start("ghost").await,
            Err(LoginError::UnknownProvider)
        ));
    }

    #[tokio::test]
    async fn a_login_against_a_non_chatgpt_provider_is_rejected() {
        let f = fixture(false).await;
        add_provider(&f, "p", "anthropic").await;

        assert!(matches!(
            f.service.start("p").await,
            Err(LoginError::NotChatGpt)
        ));
    }

    #[tokio::test]
    async fn polling_before_starting_says_so() {
        let f = fixture(false).await;
        add_provider(&f, "p", "chatgpt").await;

        assert!(matches!(
            f.service.poll("p").await,
            Err(LoginError::NotStarted)
        ));
    }

    /// The whole flow: the operator gets a code, approves it elsewhere, and the
    /// credential lands with the account id read out of the id_token.
    #[tokio::test]
    async fn an_approved_device_login_stores_the_credential() {
        let f = fixture(true).await;
        add_provider(&f, "p", "chatgpt").await;

        let started = f.service.start("p").await.unwrap();
        assert_eq!(started.user_code, "ABCD-EFGH");
        assert!(
            started.verification_url.ends_with("/codex/device"),
            "the operator needs a URL they can actually open: {}",
            started.verification_url
        );

        assert!(matches!(
            f.service.poll("p").await.unwrap(),
            PollOutcome::Pending
        ));

        match f.service.poll("p").await.unwrap() {
            PollOutcome::Complete { account_id } => assert_eq!(account_id, "acct_42"),
            PollOutcome::Pending => panic!("the second poll should have completed"),
        }

        assert_eq!(
            f.service.account_id("p").await.unwrap().as_deref(),
            Some("acct_42")
        );
    }

    #[tokio::test]
    async fn signing_out_forgets_the_credential() {
        let f = fixture(false).await;
        add_provider(&f, "p", "chatgpt").await;
        f.service.start("p").await.unwrap();
        f.service.poll("p").await.unwrap();
        assert!(f.service.account_id("p").await.unwrap().is_some());

        f.service.sign_out("p").await.unwrap();

        assert!(f.service.account_id("p").await.unwrap().is_none());
    }
}
