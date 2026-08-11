//! HTTP surface for the plugin-bundle library: CRUD for the operator (web UI)
//! plus a token-guarded artifact endpoint the session runtime fetches from.

use super::error::Api;
use super::{AppState, Scope};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use horsie_models::plugins::{InstallOutcome, PluginDefaultInput, PluginInstallInput, PluginView};

/// GET /api/plugins — the installed bundle library (metadata only).
pub async fn list(Scope(state): Scope) -> Result<Json<Vec<PluginView>>, Api> {
    state.plugins.list().await.map(Json).map_err(Api::internal)
}

/// GET /api/builtins — the slash commands horsie answers itself.
///
/// Its own endpoint rather than a field on the plugin list, and deliberately
/// not scoped to a session: a built-in is offered whether or not any bundle is
/// installed and whether or not `use_plugins` is on. Folded into the plugin
/// list it would inherit that gating and vanish from exactly the plainest
/// session.
pub async fn builtins() -> Json<Vec<horsie_models::plugins::CatalogEntryView>> {
    Json(horsie_support::plugin::builtins::catalogue_entries())
}

/// POST /api/plugins — install a bundle, or register the catalogue a URL turned
/// out to be. One box: the caller does not have to know which it pasted, and
/// both outcomes create a row, so both are 201.
pub async fn install(
    Scope(state): Scope,
    Json(input): Json<PluginInstallInput>,
) -> Result<(StatusCode, Json<InstallOutcome>), Api> {
    state
        .plugins
        .install(input)
        .await
        .map(|v| (StatusCode::CREATED, Json(v)))
        .map_err(Api::unprocessable)
}

/// POST /api/plugins/:name/update — re-clone from the remembered source.
pub async fn update(
    Scope(state): Scope,
    Path(name): Path<String>,
) -> Result<Json<PluginView>, Api> {
    state
        .plugins
        .update(&name)
        .await
        .map(Json)
        .map_err(Api::unprocessable)
}

/// PUT /api/plugins/:name — toggle whether the bundle is enabled by default.
pub async fn set_default(
    Scope(state): Scope,
    Path(name): Path<String>,
    Json(input): Json<PluginDefaultInput>,
) -> Result<Json<PluginView>, Api> {
    state
        .plugins
        .set_default(&name, input)
        .await
        .map(Json)
        .map_err(Api::unprocessable)
}

/// DELETE /api/plugins/:name — remove the bundle and GC its artifact.
pub async fn remove(Scope(state): Scope, Path(name): Path<String>) -> Result<StatusCode, Api> {
    state
        .plugins
        .remove(&name)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(Api::unprocessable)
}

/// GET /api/plugin-artifacts/:file — stream a bundle zip. `:file` is
/// `<hash>.zip`; the bearer is the runtime's dial token. Served under a
/// distinct prefix so a bundle named "artifacts" can't collide with the
/// `/api/plugins/:name` routes.
///
/// **The one plugin route reached by something holding no session credential.**
/// A runtime has a dial token and nothing else, so this runs ahead of the auth
/// layer and verifies that token itself, exactly as `runtime_connect` does:
/// read the claimed account, fetch *that account's* secret, check the tag.
///
/// It used to verify a separate HS256 capability token naming an exact hash
/// set, signed with a deployment-global secret. That had two problems. The
/// token expired after an hour, which nothing could renew — a runtime older
/// than that simply stopped being able to fetch. And the secret was global, so
/// the route had no account boundary at all: any account's token fetched any
/// account's artifact, with the hash being hard to guess as the only obstacle.
///
/// Scoping is per *account* rather than per session, deliberately. The old
/// per-session hash list protected nothing real — the same principal can select
/// any of its own bundles into a new session whenever it likes — while the
/// boundary that does matter was missing entirely.
pub async fn get_artifact(
    State(state): State<AppState>,
    Path(file): Path<String>,
    headers: HeaderMap,
) -> Result<Response, Api> {
    let hash = file
        .strip_suffix(".zip")
        .ok_or_else(|| Api::not_found("not an artifact"))?;
    let token = bearer(&headers).ok_or_else(|| Api::forbidden("missing bearer token"))?;

    // One answer for an unknown account, a bad signature, and a hash this
    // account never installed: none of them tells a stranger whether an account
    // or an artifact exists.
    let refused = || Api::forbidden("not authorized for this artifact");
    let account = horsie_support::dial_token::claimed_account(&token).ok_or_else(refused)?;
    let account = crate::auth::UserId::new(account);
    let secret = crate::config::dial_secret_of(&state.shared.db, &account)
        .await
        .map_err(Api::internal)?
        .ok_or_else(refused)?;
    horsie_support::dial_token::verify(&secret, &token).map_err(|_| refused())?;

    let services = state.users.get(&account).await.map_err(Api::internal)?;
    let installed = services
        .plugins
        .installed_hashes()
        .await
        .map_err(Api::internal)?;
    if !installed.contains(hash) {
        return Err(refused());
    }

    let path = state.shared.artifacts.path(hash);
    let bytes = std::fs::read(&path).map_err(|_| Api::not_found("artifact not found"))?;
    Ok(([(header::CONTENT_TYPE, "application/zip")], bytes).into_response())
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::to_string)
}
