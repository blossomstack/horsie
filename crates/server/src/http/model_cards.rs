//! Model-card catalog management, backing Settings → Model cards. The cards
//! are per-account (project-scoped) rather than deployment-wide, so these live
//! under `/api/settings/model-cards`, next to the rest of a user's settings.
//! Unauthenticated like the rest of `/api/*` (single-user, localhost-bound
//! deployment).

use crate::config::model_cards::ModelCardError;
use crate::http::error::Api;
use crate::http::{Scope, Scoped};
use axum::Json;
use axum::http::StatusCode;
use horsie_models::model_cards::{ModelCard, ModelCardInput, ModelCardUpdate};

/// Map a store error onto the HTTP envelope: 422 invalid input, 409
/// duplicate id, 404 unknown id, 500 anything else.
pub(crate) fn map_card_err(e: ModelCardError) -> Api {
    match e {
        ModelCardError::Invalid(m) => Api::unprocessable(m),
        ModelCardError::Duplicate(m) => Api::conflict("duplicate_model_id", m),
        ModelCardError::NotFound(m) => Api::not_found(m),
        ModelCardError::Db(m) => Api::internal(m),
    }
}

/// `GET /api/settings/model-cards` — the full catalog (kept separate from the
/// public search so management-only fields can be added later without touching
/// the public contract).
pub async fn list_cards(Scope(state): Scope) -> Result<Json<Vec<ModelCard>>, Api> {
    state
        .model_cards
        .list()
        .await
        .map(Json)
        .map_err(map_card_err)
}

/// `POST /api/settings/model-cards` — create a card; 409 on duplicate `model_id`.
pub async fn create_card(
    Scope(state): Scope,
    Json(input): Json<ModelCardInput>,
) -> Result<(StatusCode, Json<ModelCard>), Api> {
    state
        .model_cards
        .insert(&input)
        .await
        .map(|card| (StatusCode::CREATED, Json(card)))
        .map_err(map_card_err)
}

/// `PUT /api/settings/model-cards/:model_id` — update name/limits. `model_id`
/// itself is immutable (rename = delete + create).
pub async fn update_card(
    Scope(state): Scope,
    Scoped(model_id): Scoped<String>,
    Json(update): Json<ModelCardUpdate>,
) -> Result<Json<ModelCard>, Api> {
    state
        .model_cards
        .update(&model_id, &update)
        .await
        .map(Json)
        .map_err(map_card_err)
}

/// `DELETE /api/settings/model-cards/:model_id` — 204 on success, 404 when absent.
pub async fn delete_card(
    Scope(state): Scope,
    Scoped(model_id): Scoped<String>,
) -> Result<StatusCode, Api> {
    state
        .model_cards
        .delete(&model_id)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(map_card_err)
}
