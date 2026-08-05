//! HTTP surface for registered marketplaces: list what is on offer, re-read a
//! catalogue, drop a source.
//!
//! There is deliberately no POST here. A marketplace is registered by pasting
//! its URL into `POST /api/plugins`, which is the one box the whole design turns
//! on; a second way in would be a second thing to keep consistent.
//!
//! These sit beside `/api/plugins` rather than under `/api/admin/`: bundle
//! install already accepts arbitrary git URLs at this trust level, and adding a
//! catalogue of URLs does not change it.

use super::AppState;
use super::error::Api;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use horsie_models::plugins::MarketplaceView;

/// GET /api/marketplaces — every registered source and its cached catalogue.
///
/// The entries ride along, so the picker needs no second request: they are
/// already on the row, and a `/:name/plugins` endpoint would be a second read
/// of the same column.
pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<MarketplaceView>>, Api> {
    state
        .plugins
        .list_marketplaces()
        .await
        .map(Json)
        .map_err(Api::internal)
}

/// POST /api/marketplaces/:name/refresh — re-clone and re-parse the index.
pub async fn refresh(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<MarketplaceView>, Api> {
    state
        .plugins
        .refresh_marketplace(&name)
        .await
        .map(Json)
        .map_err(Api::unprocessable)
}

/// DELETE /api/marketplaces/:name — drop the source. Bundles installed from it
/// stay installed.
pub async fn remove(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, Api> {
    state
        .plugins
        .remove_marketplace(&name)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(Api::unprocessable)
}
