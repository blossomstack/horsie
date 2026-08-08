//! `GET /api/runtime/connect` — the one endpoint *runtimes* dial.
//!
//! Distinct from `/api/vendor/connect`, which vendor *processes* dial. A vendor
//! running inside this server has no listener of its own, so its runtimes come
//! back here: one port, and TLS and reverse proxies for free.
//!
//! The bearer is self-describing — `<account>.<runtime>.<tag>`, signed with the
//! account's dial secret — so this handler resolves the owning account without a
//! database read. A sandbox learning its own account id is not a disclosure: it
//! is that account's own sandbox.
//!
//! **The secret is per account, so verification is two-phase.** The account has
//! to be read out of the token before the secret that validates it can be
//! fetched, which means the id is *claimed* until the tag checks out. Nothing
//! is done with the claim except look up a secret, and a wrong claim simply
//! fails the tag — but the ordering is worth naming, because reversing it would
//! mean trusting an unverified string.
//!
//! A *raw* WebSocket upgrade rather than axum's `WebSocketUpgrade`, whose
//! `WebSocket` type cannot be handed to `tokio_tungstenite` — the same
//! mechanics `vendor_connect.rs` uses.

use crate::http::AppState;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;

/// The account a token claims, before anything has verified it.
fn claimed_account(token: &str) -> Option<String> {
    token
        .split('.')
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn bearer_of(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

pub async fn runtime_connect(
    State(state): State<AppState>,
    mut req: axum::extract::Request,
) -> Response {
    let refused = || {
        (
            StatusCode::UNAUTHORIZED,
            "a runtime must present a dial token for its own id",
        )
            .into_response()
    };

    let Some(token) = bearer_of(req.headers()) else {
        return refused();
    };
    let Some(account) = claimed_account(&token) else {
        return refused();
    };

    let services = match state.users.get(&crate::auth::UserId::new(account)).await {
        Ok(services) => services,
        // An unknown account and a bad signature are the same answer on the
        // wire: neither tells a stranger whether an account exists.
        Err(_) => return refused(),
    };

    let claims = match horsie_support::dial_token::verify(&services.dial_secret, &token) {
        Ok(claims) => claims,
        Err(_) => return refused(),
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

    let registry = services.connected_runtimes.clone();
    let runtime_id = claims.runtime_id;
    tokio::spawn(async move {
        match on_upgrade.await {
            Ok(upgraded) => {
                let ws =
                    WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, None)
                        .await;
                // The verified id, never the announced one: a token authorises
                // exactly one runtime, and this is what stops an authenticated
                // peer registering as a different one.
                horsie_runtime_vendor::handle_runtime_connection(ws, registry, runtime_id).await;
            }
            Err(e) => tracing::warn!(error = %e, "a runtime connection failed to upgrade"),
        }
    });

    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, "websocket")
        .header(header::SEC_WEBSOCKET_ACCEPT, accept)
        .body(axum::body::Body::empty())
        .map_or_else(
            |_| (StatusCode::INTERNAL_SERVER_ERROR, "upgrade failed").into_response(),
            IntoResponse::into_response,
        )
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;

    #[test]
    fn the_claimed_account_is_the_first_segment() {
        assert_eq!(claimed_account("u1.s1.deadbeef").as_deref(), Some("u1"));
    }

    #[test]
    fn a_token_with_no_account_is_not_claimable() {
        // Guards the lookup: an empty first segment must not become a lookup
        // for the empty account.
        assert_eq!(claimed_account(".s1.deadbeef"), None);
        assert_eq!(claimed_account(""), None);
    }

    #[test]
    fn a_bearer_is_read_only_when_it_is_a_non_empty_bearer() {
        let mut headers = axum::http::HeaderMap::new();
        assert_eq!(bearer_of(&headers), None);
        headers.insert(header::AUTHORIZATION, "Bearer   ".parse().unwrap());
        assert_eq!(bearer_of(&headers), None);
        headers.insert(header::AUTHORIZATION, "Basic abc".parse().unwrap());
        assert_eq!(bearer_of(&headers), None);
        headers.insert(header::AUTHORIZATION, "Bearer u1.s1.tag".parse().unwrap());
        assert_eq!(bearer_of(&headers).as_deref(), Some("u1.s1.tag"));
    }
}
