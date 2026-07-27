//! Admin API: operator-facing management surfaces. v1 is the model-card
//! catalog; future admin settings add handlers here and routes under
//! `/api/admin`. Unauthenticated like the rest of `/api/*` (single-user,
//! localhost-bound deployment).

use crate::config::model_cards::ModelCardError;
use crate::http::AppState;
use crate::http::error::Api;
use axum::Json;
use axum::extract::{Path, State};
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

/// `GET /api/admin/model-cards` — the full catalog (kept separate from the
/// public search so admin-only fields can be added later without touching
/// the public contract).
pub async fn list_cards(State(state): State<AppState>) -> Result<Json<Vec<ModelCard>>, Api> {
    state
        .model_cards
        .list()
        .await
        .map(Json)
        .map_err(map_card_err)
}

/// `POST /api/admin/model-cards` — create a card; 409 on duplicate `model_id`.
pub async fn create_card(
    State(state): State<AppState>,
    Json(input): Json<ModelCardInput>,
) -> Result<(StatusCode, Json<ModelCard>), Api> {
    state
        .model_cards
        .insert(&input)
        .await
        .map(|card| (StatusCode::CREATED, Json(card)))
        .map_err(map_card_err)
}

/// `PUT /api/admin/model-cards/:model_id` — update name/limits. `model_id`
/// itself is immutable (rename = delete + create).
pub async fn update_card(
    State(state): State<AppState>,
    Path(model_id): Path<String>,
    Json(update): Json<ModelCardUpdate>,
) -> Result<Json<ModelCard>, Api> {
    state
        .model_cards
        .update(&model_id, &update)
        .await
        .map(Json)
        .map_err(map_card_err)
}

/// `DELETE /api/admin/model-cards/:model_id` — 204 on success, 404 when absent.
pub async fn delete_card(
    State(state): State<AppState>,
    Path(model_id): Path<String>,
) -> Result<StatusCode, Api> {
    state
        .model_cards
        .delete(&model_id)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(map_card_err)
}
