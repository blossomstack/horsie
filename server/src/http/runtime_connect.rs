//! `GET /api/runtime/connect` — the single reverse-dial endpoint every runtime
//! (velos-scheduled container or user-launched local daemon) connects to, over
//! the server's one HTTP port. Replaces the per-vendor TCP listeners.
//!
//! We perform a *raw* WebSocket upgrade (not axum's `WebSocketUpgrade`, whose
//! `WebSocket` type can't be handed to the executor handshake code) so the
//! upgraded connection can be wrapped in a `tokio_tungstenite::WebSocketStream`
//! and driven by [`horsie_executor::handle_runtime_connection`] — the exact
//! same handshake/registration/race-safe logic the old listeners used.
//!
//! The `register` query parameter is the only vendor discriminator:
//! - `?register=local` → a user daemon; fire the local-vendor registration hook
//!   so the label becomes a selectable vendor. Gated by the `local_runtime`
//!   opt-in (absent hook → `403`).
//! - anything else → a velos container; no hook, it just lands in the shared
//!   registry for a waiting `provision()`.

use crate::http::AppState;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;

#[derive(Debug, Deserialize)]
pub struct ConnectParams {
    /// `local` marks a shared-local-runtime daemon that should be registered as
    /// a vendor. Any other value (or absent) is a velos-style dial-back.
    #[serde(default)]
    register: Option<String>,
}

pub async fn runtime_connect(
    State(state): State<AppState>,
    Query(params): Query<ConnectParams>,
    mut req: axum::extract::Request,
) -> Response {
    // Must be a WebSocket upgrade request.
    let Some(key) = req
        .headers()
        .get(header::SEC_WEBSOCKET_KEY)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
    else {
        return (StatusCode::BAD_REQUEST, "expected a websocket upgrade").into_response();
    };

    // Resolve the (optional) local-vendor registration hook. `register=local`
    // requires the opt-in; without it the daemon is refused rather than silently
    // treated as a velos dial-back.
    let hook = if params.register.as_deref() == Some("local") {
        match &state.local_daemon_hook {
            Some(h) => Some(h.clone()),
            None => {
                return (
                    StatusCode::FORBIDDEN,
                    "shared local runtime vendor is disabled (set local_runtime = true)",
                )
                    .into_response();
            }
        }
    } else {
        None
    };

    let Some(on_upgrade) = req.extensions_mut().remove::<OnUpgrade>() else {
        return (StatusCode::BAD_REQUEST, "connection is not upgradable").into_response();
    };
    let accept = derive_accept_key(key.as_bytes());
    let registry = state.runtime_registry.clone();

    tokio::spawn(async move {
        match on_upgrade.await {
            Ok(upgraded) => {
                let ws =
                    WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, None)
                        .await;
                horsie_executor::handle_runtime_connection(ws, registry, hook).await;
            }
            Err(e) => tracing::warn!(error = %e, "runtime_connect: websocket upgrade failed"),
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
            tracing::error!(error = %e, "runtime_connect: failed to build 101 response");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
