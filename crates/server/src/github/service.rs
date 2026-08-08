//! The GitHub connection service: ties the SQLite store to the REST client and
//! a short repo-list cache, and exposes the operations the HTTP layer and the
//! session token minter need. Secrets stay inside — nothing here returns a
//! token to the caller except `mint_token_for`, whose scoped token goes only
//! into a runtime's env.

use std::time::{Duration, Instant};

use horsie_agentcore::Secret;
use horsie_models::github::{
    GitHubAppConfigInput, GitHubAppConfigView, GitHubBranch, GitHubRepo, GitHubStatus,
};

use super::api::{GithubApi, now_secs};
use super::decode_private_key;
use super::store::{AppConfigRow, CredentialsRow, GithubStore};

/// How long a fetched repo list is served from memory before a refetch.
const CACHE_TTL: Duration = Duration::from_secs(300);

/// Refresh a user token this many seconds before its stored expiry, to avoid
/// racing an expiry mid-use.
const REFRESH_SKEW_SECS: u64 = 120;

pub struct GithubService {
    store: GithubStore,
    api: GithubApi,
    cache: tokio::sync::Mutex<Option<(Instant, Vec<GitHubRepo>)>>,
}

impl GithubService {
    pub fn new(store: GithubStore, api: GithubApi) -> Self {
        Self {
            store,
            api,
            cache: tokio::sync::Mutex::new(None),
        }
    }

    pub async fn status(&self) -> Result<GitHubStatus, String> {
        let creds = self.store.credentials().await?;
        let app = self.store.app_config().await?;
        let app_configured = app.map(|a| !a.client_id.is_empty()).unwrap_or(false);
        // Never hit the network from status — report the cached count (0 cold).
        let repo_count = self
            .cache
            .lock()
            .await
            .as_ref()
            .map(|(_, repos)| repos.len() as u32)
            .unwrap_or(0);
        Ok(GitHubStatus {
            connected: creds.is_some(),
            login: creds.map(|c| c.login),
            app_configured,
            repo_count,
        })
    }

    pub async fn app_config_view(&self) -> Result<Option<GitHubAppConfigView>, String> {
        Ok(self.store.app_config().await?.map(|row| app_view(&row)))
    }

    pub async fn save_app_config(
        &self,
        input: GitHubAppConfigInput,
    ) -> Result<GitHubAppConfigView, String> {
        let row = self.store.save_app_config(&input).await?;
        Ok(app_view(&row))
    }

    /// The GitHub OAuth authorize URL, or an error when the App is unconfigured.
    pub async fn auth_redirect(&self, request_base: &str) -> Result<String, String> {
        let app = self
            .store
            .app_config()
            .await?
            .filter(|a| !a.client_id.is_empty())
            .ok_or_else(|| "GitHub App is not configured".to_string())?;
        let redirect_uri = callback_url(&app, request_base);
        Ok(self.api.authorize_url(&app.client_id, &redirect_uri))
    }

    /// Exchange the OAuth `code`, discover the installation, and store the
    /// resulting credentials.
    pub async fn handle_callback(&self, code: &str, request_base: &str) -> Result<(), String> {
        let app = self
            .store
            .app_config()
            .await?
            .ok_or_else(|| "GitHub App is not configured".to_string())?;
        let client_secret = app
            .client_secret
            .as_ref()
            .ok_or_else(|| "GitHub App client secret is not set".to_string())?;
        let redirect_uri = callback_url(&app, request_base);
        let exchanged = self
            .api
            .exchange_code(&app.client_id, client_secret.expose(), code, &redirect_uri)
            .await?;
        let installation_id = match app.app_id {
            Some(app_id) => {
                self.api
                    .user_installation_id(&exchanged.access_token, app_id)
                    .await?
            }
            None => None,
        };
        self.store
            .save_credentials(&CredentialsRow {
                login: exchanged.login,
                access_token: Secret::from(exchanged.access_token),
                refresh_token: exchanged.refresh_token.map(Secret::from),
                expires_at: exchanged.expires_at,
                installation_id,
            })
            .await?;
        *self.cache.lock().await = None;
        Ok(())
    }

    pub async fn disconnect(&self) -> Result<(), String> {
        self.store.clear_credentials().await?;
        *self.cache.lock().await = None;
        Ok(())
    }

    /// Repos visible to the installation, served from a 5-minute cache unless
    /// `refresh` forces a refetch.
    pub async fn repos(&self, refresh: bool) -> Result<Vec<GitHubRepo>, String> {
        if !refresh
            && let Some((at, repos)) = self.cache.lock().await.as_ref()
            && at.elapsed() < CACHE_TTL
        {
            return Ok(repos.clone());
        }
        let (app_id, pem, installation_id) = self.installation_creds().await?;
        let repos = self
            .api
            .list_installation_repos(app_id, &pem, installation_id)
            .await?;
        *self.cache.lock().await = Some((Instant::now(), repos.clone()));
        Ok(repos)
    }

    pub async fn branches(&self, full_name: &str) -> Result<Vec<GitHubBranch>, String> {
        let (app_id, pem, installation_id) = self.installation_creds().await?;
        let token = self
            .api
            .installation_token(app_id, &pem, installation_id, &[])
            .await?;
        self.api.list_branches(&token, full_name).await
    }

    /// A short-lived token scoped to exactly the github.com repos in
    /// `repo_urls`. `Ok(None)` when there is nothing to mint (no github URLs) or
    /// GitHub is not fully connected — the caller then clones tokenless (works
    /// for public repos). `Err` only when a configured connection fails to mint.
    pub async fn mint_token_for(&self, repo_urls: &[String]) -> Result<Option<String>, String> {
        let mut names: Vec<String> = Vec::new();
        for url in repo_urls {
            if let Some(name) = github_short_name(url)
                && !names.contains(&name)
            {
                names.push(name);
            }
        }
        if names.is_empty() {
            return Ok(None);
        }
        let Some(app) = self.store.app_config().await? else {
            return Ok(None);
        };
        let (Some(app_id), Some(pem)) = (app.app_id, app.private_key.as_ref()) else {
            return Ok(None);
        };
        let Some(creds) = self.store.credentials().await? else {
            return Ok(None);
        };
        let Some(installation_id) = creds.installation_id else {
            return Ok(None);
        };
        let pem = decode_private_key(pem.expose())?;
        let token = self
            .api
            .installation_token(app_id, &pem, installation_id, &names)
            .await?;
        Ok(Some(token))
    }

    /// The connected account's **user** OAuth token, refreshed if within the
    /// skew window of expiry (or unconditionally when `force`, e.g. after a
    /// remote 401). `Ok(None)` when GitHub is not connected. Used as
    /// the `Authorization: Bearer` for the GitHub remote MCP server. Unlike the
    /// per-repo installation token from `mint_token_for`, this is account-scoped
    /// (the documented Bearer shape for the remote endpoint).
    pub async fn user_token(&self, force: bool) -> Result<Option<String>, String> {
        let Some(creds) = self.store.credentials().await? else {
            return Ok(None);
        };
        if !force && !needs_refresh(creds.expires_at.as_deref()) {
            return Ok(Some(creds.access_token.expose().to_string()));
        }
        // Expiring: refresh when we can, else hand back the current token and
        // let the caller's smoke test surface a dead one.
        let Some(refresh) = creds.refresh_token.as_ref().map(|s| s.expose().to_string()) else {
            return Ok(Some(creds.access_token.expose().to_string()));
        };
        let app = self
            .store
            .app_config()
            .await?
            .ok_or_else(|| "GitHub App is not configured".to_string())?;
        let client_secret = app
            .client_secret
            .as_ref()
            .ok_or_else(|| "GitHub App client secret is not set".to_string())?;
        let exchanged = self
            .api
            .refresh_token(&app.client_id, client_secret.expose(), &refresh)
            .await?;
        self.store
            .save_credentials(&CredentialsRow {
                login: exchanged.login,
                access_token: Secret::from(exchanged.access_token.clone()),
                // GitHub rotates the refresh token; keep the old one if absent.
                refresh_token: exchanged
                    .refresh_token
                    .map(Secret::from)
                    .or(creds.refresh_token),
                expires_at: exchanged.expires_at,
                installation_id: creds.installation_id,
            })
            .await?;
        Ok(Some(exchanged.access_token))
    }

    /// Resolve `(app_id, decoded_pem, installation_id)` or fail with a
    /// "connect GitHub first" style message.
    async fn installation_creds(&self) -> Result<(u64, String, u64), String> {
        let app = self
            .store
            .app_config()
            .await?
            .ok_or_else(|| "connect GitHub first".to_string())?;
        let app_id = app
            .app_id
            .ok_or_else(|| "GitHub App id is not set".to_string())?;
        let pem = app
            .private_key
            .as_ref()
            .ok_or_else(|| "GitHub App private key is not set".to_string())?;
        let creds = self
            .store
            .credentials()
            .await?
            .ok_or_else(|| "connect GitHub first".to_string())?;
        let installation_id = creds
            .installation_id
            .ok_or_else(|| "no GitHub App installation for the connected account".to_string())?;
        let pem = decode_private_key(pem.expose())?;
        Ok((app_id, pem, installation_id))
    }
}

fn app_view(row: &AppConfigRow) -> GitHubAppConfigView {
    GitHubAppConfigView {
        client_id: row.client_id.clone(),
        app_id: row.app_id,
        app_slug: row.app_slug.clone(),
        has_client_secret: row.client_secret.is_some(),
        has_private_key: row.private_key.is_some(),
        callback_base: row.callback_base.clone(),
    }
}

/// Whether a user token with this stored `expires_at` (unix seconds, as written
/// by the token exchange) should be refreshed now. An absent expiry means the
/// token does not expire (the App has token expiration disabled) → no refresh.
fn needs_refresh(expires_at: Option<&str>) -> bool {
    match expires_at.and_then(|s| s.trim().parse::<u64>().ok()) {
        Some(exp) => now_secs().saturating_add(REFRESH_SKEW_SECS) >= exp,
        None => false,
    }
}

/// The OAuth callback URL: a configured `callback_base` overrides the request
/// host (behind a proxy the request host may be wrong).
fn callback_url(app: &AppConfigRow, request_base: &str) -> String {
    let base = app
        .callback_base
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(request_base)
        .trim_end_matches('/');
    format!("{base}/api/github/callback")
}

/// The GitHub repository short name (used to scope an installation token) from
/// an `https://github.com/owner/repo(.git)` URL; `None` for non-github URLs.
fn github_short_name(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://github.com/")?;
    let repo = rest
        .trim_end_matches('/')
        .rsplit('/')
        .next()?
        .trim_end_matches(".git");
    (!repo.is_empty()).then(|| repo.to_string())
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
    fn github_short_name_extracts_repo_and_skips_others() {
        assert_eq!(
            github_short_name("https://github.com/o/repo.git"),
            Some("repo".into())
        );
        assert_eq!(
            github_short_name("https://github.com/o/repo"),
            Some("repo".into())
        );
        assert_eq!(github_short_name("https://gitlab.com/o/repo"), None);
        assert_eq!(github_short_name("file:///tmp/x"), None);
    }

    #[test]
    fn needs_refresh_honors_expiry_and_skew() {
        // No expiry → non-expiring token, never refresh.
        assert!(!needs_refresh(None));
        // Far future → no refresh.
        let future = (now_secs() + 3600).to_string();
        assert!(!needs_refresh(Some(&future)));
        // Within the skew window → refresh.
        let soon = (now_secs() + REFRESH_SKEW_SECS - 10).to_string();
        assert!(needs_refresh(Some(&soon)));
        // Already past → refresh.
        assert!(needs_refresh(Some("1")));
        // Garbage → treat as non-expiring (don't spuriously refresh).
        assert!(!needs_refresh(Some("not-a-number")));
    }
}
