//! `GET /api/vendor/connect` — the one endpoint runtime *vendor agents* dial.
//!
//! Distinct from `/api/runtime/connect`, which *runtimes* dial and which this
//! design retires in a later change. An agent that completes the handshake is
//! published as a selectable vendor under the name it announced.
//!
//! We perform a *raw* WebSocket upgrade (not axum's `WebSocketUpgrade`, whose
//! `WebSocket` type can't be handed to `tokio_tungstenite`) so the upgraded
//! connection can be wrapped in a `WebSocketStream` and driven by
//! [`VendorLink::start`] — the same mechanics `runtime_connect.rs` uses.

use crate::http::AppState;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;

pub async fn vendor_connect(
    State(state): State<AppState>,
    mut req: axum::extract::Request,
) -> Response {
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
    let agents = state.vendor_agents.clone();

    tokio::spawn(async move {
        match on_upgrade.await {
            Ok(upgraded) => {
                let ws =
                    WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, None)
                        .await;
                match crate::vendor::VendorLink::start(ws).await {
                    Ok(link) => {
                        tracing::info!(vendor = %link.vendor_name(), "vendor agent connected");
                        agents.register(link);
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
