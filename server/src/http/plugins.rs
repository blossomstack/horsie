//! HTTP surface for the plugin-bundle library: CRUD for the operator (web UI)
//! plus a token-guarded artifact endpoint the session runtime fetches from.

use super::AppState;
use super::error::Api;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use horsie_models::plugins::{PluginDefaultInput, PluginInstallInput, PluginView};

/// GET /api/plugins — the installed bundle library (metadata only).
pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<PluginView>>, Api> {
    state.plugins.list().await.map(Json).map_err(Api::internal)
}

/// POST /api/plugins — install a bundle from a git repo.
pub async fn install(
    State(state): State<AppState>,
    Json(input): Json<PluginInstallInput>,
) -> Result<(StatusCode, Json<PluginView>), Api> {
    state
        .plugins
        .install(input)
        .await
        .map(|v| (StatusCode::CREATED, Json(v)))
        .map_err(Api::unprocessable)
}

/// POST /api/plugins/:name/update — re-clone from the remembered source.
pub async fn update(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<PluginView>, Api> {
    state
        .plugins
        .update(&name)
        .await
        .map(Json)
        .map_err(Api::unprocessable)
}

/// PUT /api/plugins/:name — toggle whether the bundle is enabled by default.
pub async fn set_default(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(input): Json<PluginDefaultInput>,
) -> Result<Json<PluginView>, Api> {
    state
        .plugins
        .set_default(&name, input)
        .await
        .map(Json)
        .map_err(Api::unprocessable)
}

/// DELETE /api/plugins/:name — remove the bundle and GC its artifact.
pub async fn remove(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, Api> {
    state
        .plugins
        .remove(&name)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(Api::unprocessable)
}

/// GET /api/plugin-artifacts/:file — stream a bundle zip. `:file` is
/// `<hash>.zip`; requires `Authorization: Bearer <token>` scoping that hash.
/// Served under a distinct prefix so a bundle named "artifacts" can't collide
/// with the `/api/plugins/:name` routes.
pub async fn get_artifact(
    State(state): State<AppState>,
    Path(file): Path<String>,
    headers: HeaderMap,
) -> Result<Response, Api> {
    let hash = file
        .strip_suffix(".zip")
        .ok_or_else(|| Api::not_found("not an artifact"))?;
    let token = bearer(&headers).ok_or_else(|| Api::forbidden("missing bearer token"))?;
    state
        .plugins
        .verify_token(&token, hash)
        .map_err(Api::forbidden)?;
    let path = state.plugins.artifact_path(hash);
    let bytes = std::fs::read(&path).map_err(|_| Api::not_found("artifact not found"))?;
    Ok(([(header::CONTENT_TYPE, "application/zip")], bytes).into_response())
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::to_string)
}
