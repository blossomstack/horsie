//! Settings API: read and mutate the runtime-editable configuration. Both
//! delegate to the injected [`crate::config::ConfigStore`].

use crate::http::AppState;
use crate::http::error::Api;
use axum::Json;
use axum::extract::{Path, State};
use horsie_models::settings::{SettingsUpdate, SettingsView, VendorTestResult};

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

/// `POST /api/config/vendors/:name/test` — an on-demand reachability +
/// token check for a configured vendor (currently velos only). Always `200`
/// for a completed check; `ok: false` + `error` on a failed check, not an
/// HTTP error. An unknown vendor name is a 500 (mirrors `mcp::test`).
pub async fn test_vendor(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<VendorTestResult>, Api> {
    state
        .config_store
        .test_vendor(&name)
        .await
        .map(Json)
        .map_err(Api::internal)
}
