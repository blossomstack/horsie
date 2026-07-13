//! GitHub connection endpoints: App config, OAuth connect/callback, disconnect,
//! and the repo/branch listings behind the session repo picker. Thin wrappers
//! over [`crate::github::GithubService`]; `auth`/`callback` return redirects.

use crate::github::urlencode;
use crate::http::AppState;
use crate::http::error::Api;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::Redirect;
use horsie_models::github::{
    GitHubAppConfigInput, GitHubAppConfigView, GitHubBranchList, GitHubRepoList, GitHubStatus,
};
use serde::Deserialize;

pub async fn status(State(state): State<AppState>) -> Result<Json<GitHubStatus>, Api> {
    state.github.status().await.map(Json).map_err(Api::internal)
}

pub async fn auth(State(state): State<AppState>, headers: HeaderMap) -> Result<Redirect, Api> {
    let base = crate::http::request_base(&headers);
    let url = state
        .github
        .auth_redirect(&base)
        .await
        .map_err(Api::unprocessable)?;
    Ok(Redirect::temporary(&url))
}

pub async fn callback(
    State(state): State<AppState>,
    Query(q): Query<CallbackQuery>,
    headers: HeaderMap,
) -> Redirect {
    let base = crate::http::request_base(&headers);
    let dest = match q.code {
        Some(code) => match state.github.handle_callback(&code, &base).await {
            Ok(()) => "/settings?github_connected=1".to_string(),
            Err(e) => format!("/settings?github_error={}", urlencode(&e)),
        },
        None => format!(
            "/settings?github_error={}",
            urlencode(
                &q.error_description
                    .or(q.error)
                    .unwrap_or_else(|| "authorization denied".to_string())
            )
        ),
    };
    Redirect::temporary(&dest)
}

/// `GET /api/github/app-config` — the redacted App config, or empty defaults
/// when nothing is stored yet (simpler for the UI than a 404).
pub async fn get_app_config(
    State(state): State<AppState>,
) -> Result<Json<GitHubAppConfigView>, Api> {
    let view = state
        .github
        .app_config_view()
        .await
        .map_err(Api::internal)?
        .unwrap_or(GitHubAppConfigView {
            client_id: String::new(),
            app_id: None,
            app_slug: None,
            has_client_secret: false,
            has_private_key: false,
            callback_base: None,
        });
    Ok(Json(view))
}

pub async fn put_app_config(
    State(state): State<AppState>,
    Json(input): Json<GitHubAppConfigInput>,
) -> Result<Json<GitHubAppConfigView>, Api> {
    state
        .github
        .save_app_config(input)
        .await
        .map(Json)
        .map_err(Api::unprocessable)
}

pub async fn disconnect(State(state): State<AppState>) -> Result<(), Api> {
    state.github.disconnect().await.map_err(Api::internal)
}

pub async fn repos(
    State(state): State<AppState>,
    Query(q): Query<ReposQuery>,
) -> Result<Json<GitHubRepoList>, Api> {
    let refresh = q.refresh.as_deref() == Some("1");
    let repos = state
        .github
        .repos(refresh)
        .await
        .map_err(Api::unprocessable)?;
    Ok(Json(GitHubRepoList { repos }))
}

pub async fn branches(
    State(state): State<AppState>,
    Query(q): Query<BranchesQuery>,
) -> Result<Json<GitHubBranchList>, Api> {
    let branches = state
        .github
        .branches(&q.repo)
        .await
        .map_err(Api::unprocessable)?;
    Ok(Json(GitHubBranchList { branches }))
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    pub code: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

#[derive(Deserialize)]
pub struct ReposQuery {
    pub refresh: Option<String>,
}

#[derive(Deserialize)]
pub struct BranchesQuery {
    pub repo: String,
}
