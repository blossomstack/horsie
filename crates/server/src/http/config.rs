//! Settings API: read and mutate the runtime-editable configuration. Both
//! delegate to the injected [`crate::config::ConfigStore`].

use crate::http::Scope;
use crate::http::error::Api;
use axum::Json;
use axum::extract::Path;
use axum::http::StatusCode;
use horsie_models::settings::{
    DefaultRuntimeVendorInput, ModelInput, ModelView, ProviderInput, ProviderView, SettingsView,
};

/// `GET /api/config` — the current redacted settings view.
pub async fn get_config(Scope(state): Scope) -> Result<Json<SettingsView>, Api> {
    state
        .config_store
        .view()
        .await
        .map(Json)
        .map_err(Api::internal)
}

/// `PUT /api/config/default-runtime-vendor` — the runtime vendor new sessions
/// default to.
///
/// Returns the whole settings view rather than the bare string: the caller is
/// the Settings page, which renders `isDefault` per vendor and would otherwise
/// have to refetch.
pub async fn put_default_runtime_vendor(
    Scope(state): Scope,
    Json(input): Json<DefaultRuntimeVendorInput>,
) -> Result<Json<SettingsView>, Api> {
    state
        .config_store
        .set_default_runtime_vendor(&input.vendor)
        .await
        .map(Json)
        .map_err(Api::unprocessable)
}

/// `GET /api/config/models` — the configured model aliases.
pub async fn list_models(Scope(state): Scope) -> Result<Json<Vec<ModelView>>, Api> {
    state
        .config_store
        .view()
        .await
        .map(|v| Json(v.models))
        .map_err(Api::internal)
}

/// `PUT /api/config/models/{alias}` — create or replace one model.
///
/// The path segment is the identity. A body disagreeing with it is rejected
/// rather than treated as a rename, because a rename that silently moved the
/// row would strand nothing here but would elsewhere: aliases are what sessions
/// and agent presets store.
pub async fn put_model(
    Scope(state): Scope,
    Path(alias): Path<String>,
    Json(mut input): Json<ModelInput>,
) -> Result<Json<ModelView>, Api> {
    if input.alias.trim().is_empty() {
        input.alias = alias.clone();
    } else if input.alias.trim() != alias.trim() {
        return Err(Api::unprocessable(format!(
            "body alias '{}' does not match path '{alias}'",
            input.alias
        )));
    }
    state
        .config_store
        .upsert_model(input)
        .await
        .map(Json)
        .map_err(Api::unprocessable)
}

/// `DELETE /api/config/models/{alias}`.
pub async fn delete_model(
    Scope(state): Scope,
    Path(alias): Path<String>,
) -> Result<StatusCode, Api> {
    match state.config_store.delete_model(&alias).await {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(e) if e.starts_with("no such model") => Err(Api::not_found(e)),
        Err(e) => Err(Api::unprocessable(e)),
    }
}

/// `GET /api/config/model-providers` — the configured providers, redacted.
pub async fn list_providers(Scope(state): Scope) -> Result<Json<Vec<ProviderView>>, Api> {
    state
        .config_store
        .view()
        .await
        .map(|v| Json(v.providers))
        .map_err(Api::internal)
}

/// `PUT /api/config/model-providers/{name}` — create or replace one provider.
pub async fn put_provider(
    Scope(state): Scope,
    Path(name): Path<String>,
    Json(mut input): Json<ProviderInput>,
) -> Result<Json<ProviderView>, Api> {
    if input.name.trim().is_empty() {
        input.name = name.clone();
    } else if input.name.trim() != name.trim() {
        return Err(Api::unprocessable(format!(
            "body name '{}' does not match path '{name}'",
            input.name
        )));
    }
    state
        .config_store
        .upsert_provider(input)
        .await
        .map(Json)
        .map_err(Api::unprocessable)
}

/// `DELETE /api/config/model-providers/{name}`.
///
/// A cascade: the provider's models go with it. They route nowhere without it,
/// so refusing until each one was deleted by hand only made the caller do the
/// cascade itself.
pub async fn delete_provider(
    Scope(state): Scope,
    Path(name): Path<String>,
) -> Result<StatusCode, Api> {
    match state.config_store.delete_provider(&name).await {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(e) if e.starts_with("no such provider") => Err(Api::not_found(e)),
        Err(e) => Err(Api::unprocessable(e)),
    }
}

/// `DELETE /api/config/default-runtime-vendor` — forget the preference.
///
/// Distinct from setting it to `""`, which is refused: this removes the row and
/// falls back to the built-in default, which is what the Settings page's
/// "Clear the default" action means.
pub async fn delete_default_runtime_vendor(Scope(state): Scope) -> Result<Json<SettingsView>, Api> {
    state
        .config_store
        .clear_default_runtime_vendor()
        .await
        .map(Json)
        .map_err(Api::unprocessable)
}
