//! Settings API: read and mutate the runtime-editable configuration. Both
//! delegate to the injected [`crate::config::ConfigStore`].

use crate::http::Scope;
use crate::http::error::Api;
use axum::Json;
use axum::extract::Path;
use axum::http::StatusCode;
use horsie_models::settings::{
    ModelInput, ModelView, ProviderInput, ProviderView, SettingsUpdate, SettingsView,
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

/// `PUT /api/config` — validate, persist, and live-apply an update. A rejected
/// update changes nothing and comes back as a 422 with the reason.
pub async fn update_config(
    Scope(state): Scope,
    Json(update): Json<SettingsUpdate>,
) -> Result<Json<SettingsView>, Api> {
    state
        .config_store
        .update(update)
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
/// A provider still referenced by a model is a `409`, not a cascade: deleting
/// it would take a session's model with it.
pub async fn delete_provider(
    Scope(state): Scope,
    Path(name): Path<String>,
) -> Result<StatusCode, Api> {
    match state.config_store.delete_provider(&name).await {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(e) if e.starts_with("no such provider") => Err(Api::not_found(e)),
        Err(e) if e.contains("is still used by model") => Err(Api::conflict("provider_in_use", e)),
        Err(e) => Err(Api::unprocessable(e)),
    }
}
