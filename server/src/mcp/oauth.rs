//! OAuth 2.1 for generic remote MCP servers: RFC 9728 protected-resource +
//! RFC 8414 authorization-server discovery, RFC 7591 dynamic client
//! registration, and authorization-code-with-PKCE token exchange / refresh.
//! Bases are taken from arguments (discovered or manual), so tests point them at
//! a local mock authorization server, mirroring `github::GithubApi`.

use base64::Engine;
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// `urlencode` is re-exported `pub(crate)` from the github module; `now_secs` is
// defined locally (github's `api` module is private).
use crate::github::urlencode;

/// The authorization-server endpoints horsie needs, cached in `oauth_meta`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AsMetadata {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: Option<String>,
}

/// A dynamically-registered (or manually-entered) OAuth client.
#[derive(Debug, Clone)]
pub struct RegisteredClient {
    pub client_id: String,
    pub client_secret: Option<String>,
}

/// A token pair from a code exchange or refresh.
#[derive(Debug, Clone)]
pub struct OAuthTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix-epoch-seconds string, or `None` for non-expiring tokens.
    pub expires_at: Option<String>,
}

/// A PKCE verifier and its S256 challenge.
#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

pub struct McpOAuthClient {
    http: reqwest::Client,
}

impl Default for McpOAuthClient {
    fn default() -> Self {
        Self::new()
    }
}

impl McpOAuthClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .user_agent("horsie")
                .build()
                .unwrap_or_default(),
        }
    }

    /// Discover the authorization server for a resource: try RFC 9728
    /// protected-resource metadata at the resource origin, take the first
    /// `authorization_servers` issuer (falling back to the origin itself), then
    /// fetch RFC 8414 authorization-server metadata (OIDC discovery as fallback).
    pub async fn discover(&self, resource_url: &str) -> Result<AsMetadata, String> {
        let origin = origin_of(resource_url)?;
        let issuer = match self
            .get_json(&format!("{origin}/.well-known/oauth-protected-resource"))
            .await
        {
            Ok(v) => v
                .get("authorization_servers")
                .and_then(|a| a.as_array())
                .and_then(|a| a.first())
                .and_then(|s| s.as_str())
                .map(|s| s.trim_end_matches('/').to_string())
                .unwrap_or_else(|| origin.clone()),
            Err(_) => origin.clone(),
        };
        let meta = match self
            .get_json(&format!("{issuer}/.well-known/oauth-authorization-server"))
            .await
        {
            Ok(v) => v,
            Err(_) => self
                .get_json(&format!("{issuer}/.well-known/openid-configuration"))
                .await
                .map_err(|e| format!("no authorization-server metadata: {e}"))?,
        };
        let authorization_endpoint = str_field(&meta, "authorization_endpoint")?;
        let token_endpoint = str_field(&meta, "token_endpoint")?;
        let registration_endpoint = meta
            .get("registration_endpoint")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Ok(AsMetadata {
            authorization_endpoint,
            token_endpoint,
            registration_endpoint,
        })
    }

    /// RFC 7591 dynamic client registration.
    pub async fn register(
        &self,
        registration_endpoint: &str,
        redirect_uri: &str,
    ) -> Result<RegisteredClient, String> {
        let body = serde_json::json!({
            "client_name": "horsie",
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        });
        let v: serde_json::Value = self
            .http
            .post(registration_endpoint)
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("client registration failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("client registration rejected: {e}"))?
            .json()
            .await
            .map_err(|e| format!("client registration decode failed: {e}"))?;
        let client_id = str_field(&v, "client_id")?;
        let client_secret = v
            .get("client_secret")
            .and_then(|s| s.as_str())
            .map(str::to_string);
        Ok(RegisteredClient {
            client_id,
            client_secret,
        })
    }

    /// Exchange an authorization `code` (+ PKCE verifier) for tokens.
    pub async fn exchange_code(
        &self,
        token_endpoint: &str,
        client_id: &str,
        client_secret: Option<&str>,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
    ) -> Result<OAuthTokens, String> {
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id),
            ("code_verifier", verifier),
        ];
        if let Some(sec) = client_secret {
            form.push(("client_secret", sec));
        }
        self.post_token(token_endpoint, &form).await
    }

    /// Refresh an expiring token via its refresh token.
    pub async fn refresh(
        &self,
        token_endpoint: &str,
        client_id: &str,
        client_secret: Option<&str>,
        refresh_token: &str,
    ) -> Result<OAuthTokens, String> {
        let mut form = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ];
        if let Some(sec) = client_secret {
            form.push(("client_secret", sec));
        }
        self.post_token(token_endpoint, &form).await
    }

    /// POST a form to the token endpoint and parse the token response.
    async fn post_token(
        &self,
        token_endpoint: &str,
        form: &[(&str, &str)],
    ) -> Result<OAuthTokens, String> {
        #[derive(Deserialize)]
        struct TokenResp {
            access_token: Option<String>,
            refresh_token: Option<String>,
            expires_in: Option<u64>,
            error_description: Option<String>,
            error: Option<String>,
        }
        let resp: TokenResp = self
            .http
            .post(token_endpoint)
            .header(reqwest::header::ACCEPT, "application/json")
            .form(form)
            .send()
            .await
            .map_err(|e| format!("token request failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("token response decode failed: {e}"))?;
        let access_token = resp.access_token.ok_or_else(|| {
            resp.error_description
                .or(resp.error)
                .unwrap_or_else(|| "authorization server returned no access token".to_string())
        })?;
        let expires_at = resp
            .expires_in
            .map(|secs| now_secs().saturating_add(secs).to_string());
        Ok(OAuthTokens {
            access_token,
            refresh_token: resp.refresh_token,
            expires_at,
        })
    }

    async fn get_json(&self, url: &str) -> Result<serde_json::Value, String> {
        self.http
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
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

/// Build the authorization-code + PKCE authorize URL. `resource` (RFC 8707) ties
/// the grant to the MCP server the token is for.
pub fn build_authorize_url(
    md: &AsMetadata,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
    resource: &str,
) -> String {
    format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256&resource={}",
        md.authorization_endpoint,
        urlencode(client_id),
        urlencode(redirect_uri),
        urlencode(state),
        urlencode(challenge),
        urlencode(resource),
    )
}

/// A fresh PKCE verifier (43 url-safe chars from 32 random bytes) + S256 challenge.
pub fn gen_pkce() -> Pkce {
    let verifier = random_b64url(2); // 2 × 16 random bytes = 32 → 43 chars
    let challenge = challenge_s256(&verifier);
    Pkce { verifier, challenge }
}

/// A random opaque `state` (128 bits, url-safe).
pub fn gen_state() -> String {
    random_b64url(1)
}

/// The base64url-no-pad SHA-256 of `verifier` (PKCE S256 `code_challenge`).
fn challenge_s256(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// `count × 16` cryptographically-random bytes (UUIDv4 is getrandom-backed) as
/// base64url-no-pad.
fn random_b64url(count: usize) -> String {
    let mut bytes = Vec::with_capacity(count * 16);
    for _ in 0..count {
        bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Current unix time in whole seconds (oauth `expires_at` is stored as this).
/// `pub(crate)` so `mcp::service` reuses it in `needs_refresh`.
pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// The scheme+authority of a URL (its origin), for building well-known paths.
fn origin_of(url: &str) -> Result<String, String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("not an absolute URL: {url}"))?;
    let authority = rest.split('/').next().unwrap_or(rest);
    if authority.is_empty() {
        return Err(format!("URL has no host: {url}"));
    }
    Ok(format!("{scheme}://{authority}"))
}

fn str_field(v: &serde_json::Value, key: &str) -> Result<String, String> {
    v.get(key)
        .and_then(|s| s.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("metadata missing `{key}`"))
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

    #[test]
    fn pkce_s256_matches_the_known_rfc7636_vector() {
        // RFC 7636 Appendix B verifier → challenge.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            challenge_s256(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn gen_pkce_and_state_are_url_safe_and_sized() {
        let p = gen_pkce();
        assert!(p.verifier.len() >= 43 && p.verifier.len() <= 128);
        assert!(
            p.verifier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
        );
        assert!(!p.challenge.is_empty());
        let s = gen_state();
        assert!(s.len() >= 16);
        assert_ne!(gen_state(), s, "state is random");
    }

    #[test]
    fn authorize_url_carries_pkce_and_resource() {
        let md = AsMetadata {
            authorization_endpoint: "https://as/authorize".into(),
            token_endpoint: "https://as/token".into(),
            registration_endpoint: None,
        };
        let url = build_authorize_url(&md, "cid", "http://h/cb", "st", "chal", "https://mcp/");
        assert!(url.starts_with("https://as/authorize?"));
        assert!(url.contains("client_id=cid"));
        assert!(url.contains("code_challenge=chal"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=st"));
        assert!(url.contains("resource=https%3A%2F%2Fmcp%2F"));
    }

    #[tokio::test]
    async fn discover_reads_protected_resource_then_as_metadata() {
        let base = mock_as().await;
        let cli = McpOAuthClient::new();
        let md = cli.discover(&format!("{base}/mcp/")).await.unwrap();
        assert_eq!(md.authorization_endpoint, format!("{base}/authorize"));
        assert_eq!(md.token_endpoint, format!("{base}/token"));
        assert_eq!(
            md.registration_endpoint.as_deref(),
            Some(format!("{base}/register").as_str())
        );
    }

    #[tokio::test]
    async fn register_returns_a_client_id() {
        let base = mock_as().await;
        let cli = McpOAuthClient::new();
        let c = cli
            .register(&format!("{base}/register"), "http://h/cb")
            .await
            .unwrap();
        assert_eq!(c.client_id, "dcr-client");
    }

    #[tokio::test]
    async fn exchange_and_refresh_parse_tokens() {
        let base = mock_as().await;
        let cli = McpOAuthClient::new();
        let t = cli
            .exchange_code(
                &format!("{base}/token"),
                "cid",
                None,
                "the-code",
                "http://h/cb",
                "verf",
            )
            .await
            .unwrap();
        assert_eq!(t.access_token, "at-1");
        assert_eq!(t.refresh_token.as_deref(), Some("rt-1"));
        assert!(t.expires_at.is_some());
        let r = cli
            .refresh(&format!("{base}/token"), "cid", None, "rt-1")
            .await
            .unwrap();
        assert_eq!(r.access_token, "at-2");
    }

    /// A mock authorization server exposing RFC 9728/8414 metadata, DCR, and a
    /// token endpoint. Returns its base URL.
    pub(super) async fn mock_as() -> String {
        use axum::{
            Json, Router,
            routing::{get, post},
        };
        use serde_json::json;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let prm = {
            let b = base.clone();
            move || {
                let b = b.clone();
                async move { Json(json!({ "authorization_servers": [b] })) }
            }
        };
        let asm = {
            let b = base.clone();
            move || {
                let b = b.clone();
                async move {
                    Json(json!({
                        "issuer": b,
                        "authorization_endpoint": format!("{b}/authorize"),
                        "token_endpoint": format!("{b}/token"),
                        "registration_endpoint": format!("{b}/register"),
                    }))
                }
            }
        };
        let app = Router::new()
            .route("/.well-known/oauth-protected-resource", get(prm))
            .route("/.well-known/oauth-authorization-server", get(asm))
            .route(
                "/register",
                post(|| async { Json(json!({ "client_id": "dcr-client" })) }),
            )
            .route(
                "/token",
                post(
                    |axum::extract::Form(f): axum::extract::Form<
                        std::collections::HashMap<String, String>,
                    >| async move {
                        let at = if f.get("grant_type").map(String::as_str) == Some("refresh_token")
                        {
                            "at-2"
                        } else {
                            "at-1"
                        };
                        Json(json!({
                            "access_token": at,
                            "refresh_token": "rt-1",
                            "token_type": "bearer",
                            "expires_in": 3600
                        }))
                    },
                ),
            );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        base
    }
}
