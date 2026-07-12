//! GitHub REST client: OAuth code exchange, App JWT, scoped installation
//! tokens, repo/branch listing. Bases are injectable so tests run against a
//! local mock server. Adapted from agentx's `github_routes.rs`, converted to
//! `Result<_, String>`.

use base64::Engine;
use horsie_models::github::{GitHubBranch, GitHubRepo};
use serde::Deserialize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub struct GithubApi {
    web_base: String,
    api_base: String,
    http: reqwest::Client,
}

/// The result of a successful OAuth code exchange.
pub struct ExchangedToken {
    pub login: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<String>,
}

impl Default for GithubApi {
    fn default() -> Self {
        Self::new()
    }
}

impl GithubApi {
    pub fn new() -> Self {
        Self::with_bases("https://github.com", "https://api.github.com")
    }

    /// Inject bases (tests point these at a local mock server).
    pub fn with_bases(web_base: &str, api_base: &str) -> Self {
        Self {
            web_base: web_base.trim_end_matches('/').to_string(),
            api_base: api_base.trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .user_agent("horsie")
                .build()
                .unwrap_or_default(),
        }
    }

    pub fn authorize_url(&self, client_id: &str, redirect_uri: &str) -> String {
        format!(
            "{}/login/oauth/authorize?client_id={}&redirect_uri={}",
            self.web_base,
            urlencode(client_id),
            urlencode(redirect_uri),
        )
    }

    /// Exchange an OAuth `code` for an access token, then read the account login.
    pub async fn exchange_code(
        &self,
        client_id: &str,
        client_secret: &str,
        code: &str,
        redirect_uri: &str,
    ) -> Result<ExchangedToken, String> {
        #[derive(Deserialize)]
        struct TokenResp {
            access_token: Option<String>,
            refresh_token: Option<String>,
            expires_in: Option<u64>,
            error_description: Option<String>,
        }
        let resp: TokenResp = self
            .http
            .post(format!("{}/login/oauth/access_token", self.web_base))
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&serde_json::json!({
                "client_id": client_id,
                "client_secret": client_secret,
                "code": code,
                "redirect_uri": redirect_uri,
            }))
            .send()
            .await
            .map_err(|e| format!("oauth token request failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("oauth token response decode failed: {e}"))?;
        let access_token = resp.access_token.ok_or_else(|| {
            resp.error_description
                .unwrap_or_else(|| "github did not return an access token".to_string())
        })?;
        let login = self.fetch_login(&access_token).await?;
        let expires_at = resp.expires_in.map(|secs| {
            let at = now_secs().saturating_add(secs);
            at.to_string()
        });
        Ok(ExchangedToken {
            login,
            access_token,
            refresh_token: resp.refresh_token,
            expires_at,
        })
    }

    /// Refresh an expiring user OAuth token via its refresh token (same token
    /// endpoint as the code exchange, `grant_type=refresh_token`). GitHub
    /// rotates the refresh token, so the response carries a new one.
    pub async fn refresh_token(
        &self,
        client_id: &str,
        client_secret: &str,
        refresh_token: &str,
    ) -> Result<ExchangedToken, String> {
        #[derive(Deserialize)]
        struct TokenResp {
            access_token: Option<String>,
            refresh_token: Option<String>,
            expires_in: Option<u64>,
            error_description: Option<String>,
        }
        let resp: TokenResp = self
            .http
            .post(format!("{}/login/oauth/access_token", self.web_base))
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&serde_json::json!({
                "client_id": client_id,
                "client_secret": client_secret,
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
            }))
            .send()
            .await
            .map_err(|e| format!("oauth refresh request failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("oauth refresh response decode failed: {e}"))?;
        let access_token = resp.access_token.ok_or_else(|| {
            resp.error_description
                .unwrap_or_else(|| "github did not return a refreshed access token".to_string())
        })?;
        let login = self.fetch_login(&access_token).await?;
        let expires_at = resp
            .expires_in
            .map(|secs| now_secs().saturating_add(secs).to_string());
        Ok(ExchangedToken {
            login,
            access_token,
            refresh_token: resp.refresh_token,
            expires_at,
        })
    }

    async fn fetch_login(&self, access_token: &str) -> Result<String, String> {
        #[derive(Deserialize)]
        struct User {
            login: String,
        }
        let user: User = self
            .api_get("/user", access_token)
            .await
            .map_err(|e| format!("github /user failed: {e}"))?;
        Ok(user.login)
    }

    /// The installation id of the App identified by `app_id` for the user behind
    /// `access_token`, if the App is installed.
    pub async fn user_installation_id(
        &self,
        access_token: &str,
        app_id: u64,
    ) -> Result<Option<u64>, String> {
        #[derive(Deserialize)]
        struct Installation {
            id: u64,
            app_id: u64,
        }
        #[derive(Deserialize)]
        struct Resp {
            installations: Vec<Installation>,
        }
        let resp: Resp = self
            .api_get("/user/installations", access_token)
            .await
            .map_err(|e| format!("github installations failed: {e}"))?;
        Ok(resp
            .installations
            .into_iter()
            .find(|i| i.app_id == app_id)
            .map(|i| i.id))
    }

    /// Mint an installation access token. `repos` (short names) scopes it to
    /// exactly those repositories; empty leaves it unscoped.
    pub async fn installation_token(
        &self,
        app_id: u64,
        pem: &str,
        installation_id: u64,
        repos: &[String],
    ) -> Result<String, String> {
        let jwt = make_app_jwt(app_id, pem)?;
        let mut body = serde_json::Map::new();
        if !repos.is_empty() {
            body.insert("repositories".to_string(), serde_json::json!(repos));
        }
        #[derive(Deserialize)]
        struct TokenResp {
            token: String,
        }
        let resp: TokenResp = self
            .http
            .post(format!(
                "{}/app/installations/{installation_id}/access_tokens",
                self.api_base
            ))
            .bearer_auth(&jwt)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .json(&serde_json::Value::Object(body))
            .send()
            .await
            .map_err(|e| format!("installation token request failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("installation token request rejected: {e}"))?
            .json()
            .await
            .map_err(|e| format!("installation token decode failed: {e}"))?;
        Ok(resp.token)
    }

    /// Every repository the installation can see (paginated).
    pub async fn list_installation_repos(
        &self,
        app_id: u64,
        pem: &str,
        installation_id: u64,
    ) -> Result<Vec<GitHubRepo>, String> {
        let token = self
            .installation_token(app_id, pem, installation_id, &[])
            .await?;
        #[derive(Deserialize)]
        struct Repo {
            full_name: String,
            private: bool,
            default_branch: String,
        }
        #[derive(Deserialize)]
        struct Page {
            repositories: Vec<Repo>,
        }
        let mut out = Vec::new();
        let mut page = 1;
        loop {
            let p: Page = self
                .api_get(
                    &format!("/installation/repositories?per_page=100&page={page}"),
                    &token,
                )
                .await
                .map_err(|e| format!("list installation repos failed: {e}"))?;
            let n = p.repositories.len();
            out.extend(p.repositories.into_iter().map(|r| GitHubRepo {
                full_name: r.full_name,
                private: r.private,
                default_branch: r.default_branch,
            }));
            if n < 100 {
                break;
            }
            page += 1;
        }
        Ok(out)
    }

    /// The branches of `full_name` ("owner/name").
    pub async fn list_branches(
        &self,
        token: &str,
        full_name: &str,
    ) -> Result<Vec<GitHubBranch>, String> {
        #[derive(Deserialize)]
        struct Branch {
            name: String,
        }
        let branches: Vec<Branch> = self
            .api_get(&format!("/repos/{full_name}/branches?per_page=100"), token)
            .await
            .map_err(|e| format!("list branches failed: {e}"))?;
        Ok(branches
            .into_iter()
            .map(|b| GitHubBranch { name: b.name })
            .collect())
    }

    /// GET `{api_base}{path}` as a bearer-authenticated GitHub API JSON call.
    async fn api_get<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        token: &str,
    ) -> Result<T, String> {
        self.http
            .get(format!("{}{path}", self.api_base))
            .bearer_auth(token)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())
    }
}

pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Percent-encode a value for a URL query component.
pub(crate) fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Short-lived JWT authenticating as the GitHub App (10-minute max).
pub fn make_app_jwt(app_id: u64, pem: &str) -> Result<String, String> {
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    let now = now_secs();
    #[derive(serde::Serialize)]
    struct Claims {
        iat: u64,
        exp: u64,
        iss: String,
    }
    let claims = Claims {
        iat: now.saturating_sub(60),  // clock-skew buffer
        exp: now.saturating_add(540), // 9 min (max 10)
        iss: app_id.to_string(),
    };
    let key = EncodingKey::from_rsa_pem(pem.as_bytes())
        .map_err(|e| format!("invalid RSA private key: {e}"))?;
    encode(&Header::new(Algorithm::RS256), &claims, &key).map_err(|e| format!("JWT encode: {e}"))
}

/// Accept a raw PEM or a base64-encoded PEM (copy-paste friendly).
pub fn decode_private_key(raw: &str) -> Result<String, String> {
    let t = raw.trim();
    if t.starts_with("-----BEGIN") {
        return Ok(t.to_string());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(t)
        .map_err(|_| "private key is neither a PEM nor base64-encoded PEM".to_string())?;
    let s = String::from_utf8(bytes).map_err(|_| "decoded private key is not UTF-8".to_string())?;
    if !s.trim_start().starts_with("-----BEGIN") {
        return Err("decoded value is not a PEM".to_string());
    }
    Ok(s)
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
    use base64::Engine;

    /// 2048-bit RSA test key, generated for tests only (not a real credential).
    /// Generate once with: openssl genrsa -traditional 2048
    const TEST_PEM: &str = include_str!("testdata/test_rsa.pem");

    #[test]
    fn decode_private_key_accepts_raw_and_base64() {
        assert_eq!(
            decode_private_key(TEST_PEM).unwrap().trim(),
            TEST_PEM.trim()
        );
        let b64 = base64::engine::general_purpose::STANDARD.encode(TEST_PEM);
        assert_eq!(decode_private_key(&b64).unwrap().trim(), TEST_PEM.trim());
        assert!(decode_private_key("garbage").is_err());
    }

    #[test]
    fn make_app_jwt_produces_rs256_token() {
        let jwt = make_app_jwt(1234, TEST_PEM).unwrap();
        // header.payload.signature
        assert_eq!(jwt.split('.').count(), 3);
        let header = jwt.split('.').next().unwrap();
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(header)
            .unwrap();
        assert!(String::from_utf8_lossy(&decoded).contains("RS256"));
    }

    #[test]
    fn authorize_url_carries_client_and_redirect() {
        let api = GithubApi::new();
        let url = api.authorize_url("cid-1", "https://h.example/api/github/callback");
        assert!(url.starts_with("https://github.com/login/oauth/authorize?"));
        assert!(url.contains("client_id=cid-1"));
        assert!(url.contains("redirect_uri=https%3A%2F%2Fh.example%2Fapi%2Fgithub%2Fcallback"));
    }

    #[tokio::test]
    async fn installation_token_scopes_to_repo_short_names() {
        // Mock GitHub: capture the token-request body, return a token.
        use axum::{Json, Router, extract::State, routing::post};
        use std::sync::{Arc, Mutex};
        let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
        let app = Router::new()
            .route(
                "/app/installations/:id/access_tokens",
                post(
                    |State(cap): State<Arc<Mutex<Option<serde_json::Value>>>>,
                     Json(body): Json<serde_json::Value>| async move {
                        *cap.lock().unwrap() = Some(body);
                        Json(serde_json::json!({"token": "ghs_test"}))
                    },
                ),
            )
            .with_state(captured.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let api = GithubApi::with_bases("http://x", &format!("http://{addr}"));
        let token = api
            .installation_token(1234, TEST_PEM, 42, &["api".into(), "web".into()])
            .await
            .unwrap();
        assert_eq!(token, "ghs_test");
        let body = captured.lock().unwrap().clone().unwrap();
        assert_eq!(body["repositories"], serde_json::json!(["api", "web"]));
    }
}
