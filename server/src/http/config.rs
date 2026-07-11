//! Settings API: read and mutate the runtime-editable configuration. Both
//! delegate to the injected [`crate::config::ConfigStore`].

use crate::http::AppState;
use crate::http::error::Api;
use axum::Json;
use axum::extract::State;
use horsie_models::settings::{SettingsUpdate, SettingsView};

/// `GET /api/config` — the current redacted settings view.
pub async fn get_config(State(state): State<AppState>) -> Result<Json<SettingsView>, Api> {
    state
        .config_store
        .view()
        .await
        .map(Json)
        .map_err(Api::internal)
}

/// `PUT /api/config` — validate, persist, and live-apply an update. A rejected
/// update changes nothing and comes back as a 422 with the reason.
pub async fn update_config(
    State(state): State<AppState>,
    Json(update): Json<SettingsUpdate>,
) -> Result<Json<SettingsView>, Api> {
    state
        .config_store
        .update(update)
        .await
        .map(Json)
        .map_err(Api::unprocessable)
}
