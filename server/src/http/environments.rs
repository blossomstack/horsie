//! HTTP surface for environments: CRUD for the web UI. There is no invoke or
//! run endpoint — nothing consumes an environment yet.

use super::Scope;
use super::error::Api;
use crate::environments::EnvironmentError;
use axum::Json;
use axum::extract::Path;
use axum::http::StatusCode;
use horsie_models::environments::{EnvironmentInput, EnvironmentView};

/// Map the typed service error onto the envelope without string matching.
fn api_err(e: EnvironmentError) -> Api {
    match e {
        EnvironmentError::NotFound(m) => Api::not_found(m),
        EnvironmentError::Conflict(m) => Api::conflict("duplicate", m),
        EnvironmentError::Invalid(m) => Api::unprocessable(m),
        EnvironmentError::Internal(m) => Api::internal(m),
    }
}

/// GET /api/environments
pub async fn list_environments(Scope(state): Scope) -> Result<Json<Vec<EnvironmentView>>, Api> {
    state.environments.list().await.map(Json).map_err(api_err)
}

/// POST /api/environments
pub async fn create_environment(
    Scope(state): Scope,
    Json(input): Json<EnvironmentInput>,
) -> Result<(StatusCode, Json<EnvironmentView>), Api> {
    state
        .environments
        .create(input)
        .await
        .map(|v| (StatusCode::CREATED, Json(v)))
        .map_err(api_err)
}

/// GET /api/environments/:name
pub async fn get_environment(
    Scope(state): Scope,
    Path(name): Path<String>,
) -> Result<Json<EnvironmentView>, Api> {
    state
        .environments
        .get(&name)
        .await
        .map(Json)
        .map_err(api_err)
}

/// PUT /api/environments/:name — full replace; the path is the id of record.
pub async fn replace_environment(
    Scope(state): Scope,
    Path(name): Path<String>,
    Json(input): Json<EnvironmentInput>,
) -> Result<Json<EnvironmentView>, Api> {
    state
        .environments
        .replace(&name, input)
        .await
        .map(Json)
        .map_err(api_err)
}

/// DELETE /api/environments/:name
///
/// Unconditional: nothing references an environment yet, so there is no
/// in-use guard like the agents one. When wiring arrives, revisit this.
pub async fn delete_environment(
    Scope(state): Scope,
    Path(name): Path<String>,
) -> Result<StatusCode, Api> {
    state
        .environments
        .delete(&name)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(api_err)
}
