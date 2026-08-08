//! HTTP surface for *configured* runtime vendors — the settings CRUD behind
//! the WebUI's runtime-vendor section.
//!
//! Only vendors the server builds itself appear here. The ones that dial in
//! announce themselves and are listed in the settings view instead; there is
//! nothing to create or delete about a process someone else is running.

use super::Scope;
use super::error::Api;
use crate::runtime_vendor::config::VendorConfigError;
use axum::Json;
use axum::extract::Path;
use axum::http::StatusCode;
use horsie_models::runtime_vendor::{RuntimeVendorConfigInput, RuntimeVendorConfigView};

fn api_err(e: VendorConfigError) -> Api {
    match e {
        VendorConfigError::NotFound(m) => Api::not_found(m),
        VendorConfigError::Conflict(m) => Api::conflict("name_in_use", m),
        VendorConfigError::Invalid(m) => Api::unprocessable(m),
        VendorConfigError::Internal(m) => Api::internal(m),
    }
}

/// GET /api/runtime-vendors
pub async fn list_runtime_vendors(
    Scope(state): Scope,
) -> Result<Json<Vec<RuntimeVendorConfigView>>, Api> {
    state
        .runtime_vendors
        .list_views()
        .await
        .map(Json)
        .map_err(api_err)
}

/// PUT /api/runtime-vendors/:name — create or fully replace.
///
/// One verb rather than POST-then-PUT: a vendor row is a connection setting
/// keyed by its name, and re-saving one is how a rotated token is applied.
pub async fn put_runtime_vendor(
    Scope(state): Scope,
    Path(name): Path<String>,
    Json(input): Json<RuntimeVendorConfigInput>,
) -> Result<Json<RuntimeVendorConfigView>, Api> {
    state
        .runtime_vendors
        .save_input(&name, input)
        .await
        .map(Json)
        .map_err(api_err)
}

/// DELETE /api/runtime-vendors/:name
pub async fn delete_runtime_vendor(
    Scope(state): Scope,
    Path(name): Path<String>,
) -> Result<StatusCode, Api> {
    state
        .runtime_vendors
        .delete_named(&name)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(api_err)
}
