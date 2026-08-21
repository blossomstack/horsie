//! GitHub connection endpoints: App config, OAuth connect/callback, disconnect,
//! and the repo/branch listings behind the session repo picker. Thin wrappers
//! over [`crate::github::GithubService`]; `auth`/`callback` return redirects.

use crate::github::urlencode;
use crate::http::Scope;
use crate::http::error::Api;
use axum::Json;
use axum::extract::Query;
use axum::http::HeaderMap;
use axum::response::Redirect;
use horsie_models::github::{
    GitHubAppConfigInput, GitHubAppConfigView, GitHubBranchList, GitHubRepoList, GitHubStatus,
};
use serde::Deserialize;

pub async fn status(Scope(state): Scope) -> Result<Json<GitHubStatus>, Api> {
    state.github.status().await.map(Json).map_err(Api::internal)
}

pub async fn auth(Scope(state): Scope, headers: HeaderMap) -> Result<Redirect, Api> {
    let base = crate::http::request_base(&headers);
    let url = state
        .github
        .auth_redirect(&base)
        .await
        .map_err(Api::unprocessable)?;
    Ok(Redirect::temporary(&url))
}

/// Where a callback sends the browser back to.
///
/// `/settings/integrations`, not `/settings`: the index route redirects to
/// `/settings/models` and drops the query string on the way, so the server's
/// perfectly good error message was thrown away by the client's own router and
/// the user saw an unremarkable settings page. `IntegrationsSettings` is the
/// only page that reads these params, so it is the only page worth sending
/// them to.
pub(crate) const SETTINGS_PAGE: &str = "/settings/integrations";

/// That page, inside a project.
///
/// The web router is rooted at `/p/<project>`, so a bare `/settings/…` is not a
/// route it serves — it lands on the project redirect, which drops the query
/// string these callbacks carry their whole result in.
pub(crate) fn settings_page(project: &crate::projects::ProjectId) -> String {
    format!("/p/{project}{SETTINGS_PAGE}")
}

pub async fn callback(
    Scope(state): Scope,
    Query(q): Query<CallbackQuery>,
    headers: HeaderMap,
) -> Redirect {
    let base = crate::http::request_base(&headers);
    let page = settings_page(&state.project);
    let dest = match q.code {
        Some(code) => match state.github.handle_callback(&code, &base).await {
            Ok(()) => format!("{page}?github_connected=1"),
            Err(e) => format!("{page}?github_error={}", urlencode(&e)),
        },
        None => format!(
            "{page}?github_error={}",
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
pub async fn get_app_config(Scope(state): Scope) -> Result<Json<GitHubAppConfigView>, Api> {
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
    Scope(state): Scope,
    Json(input): Json<GitHubAppConfigInput>,
) -> Result<Json<GitHubAppConfigView>, Api> {
    state
        .github
        .save_app_config(input)
        .await
        .map(Json)
        .map_err(Api::unprocessable)
}

pub async fn disconnect(Scope(state): Scope) -> Result<(), Api> {
    state.github.disconnect().await.map_err(Api::internal)
}

pub async fn repos(
    Scope(state): Scope,
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
    Scope(state): Scope,
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
