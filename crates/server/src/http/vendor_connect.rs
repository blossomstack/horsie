//! `GET /api/vendor/connect` — the one endpoint runtime *vendor agents* dial.
//!
//! Distinct from `/api/runtime/connect`, which *runtimes* dial and which this
//! design retires in a later change. An agent that completes the handshake is
//! published as a selectable vendor under the name it announced.
//!
//! We perform a *raw* WebSocket upgrade (not axum's `WebSocketUpgrade`, whose
//! `WebSocket` type can't be handed to `tokio_tungstenite`) so the upgraded
//! connection can be wrapped in a `WebSocketStream` and driven by
//! [`WebsocketRuntimeVendor::start`] — the same mechanics `runtime_connect.rs` uses.

use crate::auth::{Principal, TokenKind};
use crate::http::AppState;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;

/// Resolve the credential on a vendor dial.
///
/// Only `access` and `agent` kinds are accepted: a `web` cookie or a `refresh`
/// token verifies perfectly well but has no business driving a machine link,
/// and accepting one would let a stolen browser session become a runtime.
/// Read the bearer out of the headers. Separate from [`authenticate`] and
/// synchronous on purpose: `Request<Body>` is not `Sync`, so holding a
/// reference to it across an await would make the handler future non-`Send`
/// and it would stop being an axum handler at all.
fn bearer_of(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

async fn authenticate(
    state: &AppState,
    bearer: Option<String>,
    delegated: Option<crate::http::auth::DelegatedIdentity>,
) -> Result<Principal, Response> {
    let refused = || {
        (
            StatusCode::UNAUTHORIZED,
            axum::Json(horsie_models::session_api::ApiError {
                code: "unauthorized".to_string(),
                message: "a vendor agent must present an access or machine token".to_string(),
            }),
        )
            .into_response()
    };
    match state.auth.mode() {
        crate::auth::AuthMode::Off => return Ok(Principal::Anonymous),
        // The kind rule below does not survive into this mode, and should not:
        // only whoever issues credentials knows what kinds it has. A front
        // layer that tells a browser session apart from a machine one refuses
        // this dial itself, on the path, where it has that information.
        crate::auth::AuthMode::Delegated => {
            return delegated.map(|d| Principal::User(d.0)).ok_or_else(refused);
        }
        crate::auth::AuthMode::Password => {}
    }
    let Some(secret) = bearer else {
        return Err(refused());
    };
    match state.auth.verify(&secret).await {
        Ok(Some(v)) if matches!(v.kind, TokenKind::Access | TokenKind::Agent) => Ok(v.principal),
        Ok(_) => Err(refused()),
        Err(e) => {
            tracing::error!(error = %e, "verifying a vendor credential failed");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not verify the credential",
            )
                .into_response())
        }
    }
}

pub async fn vendor_connect(
    State(state): State<AppState>,
    mut req: axum::extract::Request,
) -> Response {
    // Authenticate before anything else, and answer 401 rather than completing
    // the upgrade and closing: an upgrade that opens and dies looks to the
    // agent like a transport fault worth retrying, whereas a 401 is a fact it
    // can report to whoever launched it.
    let delegated = req
        .extensions()
        .get::<crate::http::auth::DelegatedIdentity>()
        .cloned();
    let owner = match authenticate(&state, bearer_of(req.headers()), delegated).await {
        Ok(p) => p,
        Err(response) => return response,
    };
    let Some(key) = req
        .headers()
        .get(header::SEC_WEBSOCKET_KEY)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
    else {
        return (StatusCode::BAD_REQUEST, "expected a websocket upgrade").into_response();
    };
    let Some(on_upgrade) = req.extensions_mut().remove::<OnUpgrade>() else {
        return (StatusCode::BAD_REQUEST, "connection is not upgradable").into_response();
    };
    let accept = derive_accept_key(key.as_bytes());

    // The agent publishes into *its owner's* vendor map, resolved here because
    // this is the moment the owner is known and the upgrade has not completed.
    // Two accounts can therefore each run `horsie connect --runtime-id main`,
    // and neither one's sessions can select the other's runtime — the link is
    // not in the map they read.
    let owner_id = match &owner {
        Principal::Anonymous => state.shared.anonymous.clone(),
        Principal::User(id) => id.clone(),
    };
    let agents = match state.users.get(&owner_id).await {
        Ok(services) => services.vendor_agents.clone(),
        Err(e) => {
            tracing::error!(user = %owner_id, error = %e, "resolving a vendor's account failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not resolve the account",
            )
                .into_response();
        }
    };

    tokio::spawn(async move {
        match on_upgrade.await {
            Ok(upgraded) => {
                let ws =
                    WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, None)
                        .await;
                match crate::runtime_vendor::WebsocketRuntimeVendor::start(
                    ws,
                    owner,
                    agents.links(),
                )
                .await
                {
                    Ok(link) => {
                        let name = link.vendor_name().to_string();
                        match agents.publish(link.clone()) {
                            Ok(()) => {
                                if let Err(e) = link.confirm_registration().await {
                                    tracing::warn!(
                                        vendor = %name,
                                        error = %e,
                                        "could not acknowledge a vendor registration"
                                    );
                                }
                                tracing::info!(
                                    vendor = %name,
                                    instance = %link.instance_id(),
                                    "vendor agent connected"
                                );
                            }
                            // Tell the agent why, then drop the link, which
                            // closes the socket. The refusal is the whole
                            // response: this name belongs to a live agent and
                            // nothing about retrying will change that.
                            Err(e) => {
                                link.reject_registration(&e.client_reason(&name)).await;
                                tracing::warn!(
                                    vendor = %name,
                                    error = %e,
                                    "refused a vendor agent claiming a name already in use"
                                );
                            }
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "vendor agent handshake failed"),
                }
            }
            Err(e) => tracing::warn!(error = %e, "vendor_connect: websocket upgrade failed"),
        }
    });

    // 101 Switching Protocols; hyper completes the upgrade once this response is
    // sent, resolving the `on_upgrade` future above.
    match Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, "websocket")
        .header(header::SEC_WEBSOCKET_ACCEPT, accept)
        .body(axum::body::Body::empty())
    {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!(error = %e, "vendor_connect: failed to build 101 response");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
