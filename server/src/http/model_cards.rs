//! Public model-card read API: prefix search consumed by the Settings model
//! form's model-id autocomplete. Mutations live under `/api/admin`.

use crate::http::Scope;
use crate::http::error::Api;
use axum::Json;
use axum::extract::Query;
use horsie_models::model_cards::ModelCard;
use serde::Deserialize;

#[derive(Deserialize)]
pub struct ListQuery {
    prefix: Option<String>,
}

/// `GET /api/model-cards?prefix=` — cards whose `model_id` starts with
/// `prefix` (all cards when omitted), ordered by `model_id`, capped.
pub async fn list(
    Scope(state): Scope,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<ModelCard>>, Api> {
    state
        .model_cards
        .search_by_prefix(q.prefix.as_deref().unwrap_or(""))
        .await
        .map(Json)
        .map_err(super::admin::map_card_err)
}
