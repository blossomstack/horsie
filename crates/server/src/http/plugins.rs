//! The one plugin route that is not a control-plane operation: the
//! token-guarded endpoint a session runtime fetches a bundle zip from.
//! Managing the library itself lives in `control::plugins`.

use super::AppState;
use super::error::Api;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};

/// GET /api/plugin-bundles/:name/:version — stream a bundle zip. `:version` is
/// [`horsie_models::bundle_version_slug`]; the bearer is the runtime's dial
/// token. Served under a distinct prefix so a bundle named "bundles" cannot
/// collide with the `/api/plugins/:name` routes.
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
/// Naming the bundle rather than a hash is what replaced that last obstacle
/// with a real check: the row has to belong to the account asking. An authored
/// bundle has no artifact file to be hard to guess at all, so a route keyed on
/// hashes could not have served one.
///
/// Scoping is per *account* rather than per session, deliberately. The old
/// per-session hash list protected nothing real — the same principal can select
/// any of its own bundles into a new session whenever it likes — while the
/// boundary that does matter was missing entirely.
pub async fn get_bundle(
    State(state): State<AppState>,
    Path((name, version)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, Api> {
    let token = bearer(&headers).ok_or_else(|| Api::forbidden("missing bearer token"))?;

    // One answer for an unknown account, a bad signature, and a bundle this
    // account never installed: none of them tells a stranger whether an account
    // or a bundle exists.
    let refused = || Api::forbidden("not authorized for this bundle");
    let version = horsie_models::parse_bundle_version_slug(&version).ok_or_else(refused)?;
    let account = horsie_support::dial_token::claimed_account(&token).ok_or_else(refused)?;
    let account = crate::projects::ProjectId::new(account);
    let secret = crate::config::dial_secret_of(&state.shared.db, &account)
        .await
        .map_err(Api::internal)?
        .ok_or_else(refused)?;
    horsie_support::dial_token::verify(&secret, &token).map_err(|_| refused())?;

    let services = state.projects.get(&account).await.map_err(Api::internal)?;
    // Absent, another account's, or a version this bundle is not at — all
    // refused the same way, for the same reason.
    let bytes = services
        .plugins
        .package(&name, &version)
        .await
        .map_err(|_| refused())?;
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
