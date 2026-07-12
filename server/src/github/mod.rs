//! Deployment-global GitHub connection: SQLite-backed app config + OAuth
//! credentials, a GitHub API client (App JWT → scoped installation tokens,
//! repo listing), and the session-facing token minter.

mod api;
mod service;
mod store;

pub(crate) use api::urlencode;
pub use api::{ExchangedToken, GithubApi, decode_private_key, make_app_jwt};
pub use service::GithubService;
pub use store::{AppConfigRow, CredentialsRow, GithubStore};

/// Mints a short-lived GitHub token scoped to exactly `repo_urls`' repos, for
/// injection into a session's runtime at provision time. `Ok(None)` = nothing
/// to mint (no github URLs / not connected) — the caller proceeds tokenless
/// (public repos still clone). `Err` = a configured connection failed to mint.
#[async_trait::async_trait]
pub trait GithubTokenMinter: Send + Sync {
    async fn mint_for(&self, repo_urls: &[String]) -> Result<Option<String>, String>;
}

#[async_trait::async_trait]
impl GithubTokenMinter for GithubService {
    async fn mint_for(&self, repo_urls: &[String]) -> Result<Option<String>, String> {
        self.mint_token_for(repo_urls).await
    }
}
