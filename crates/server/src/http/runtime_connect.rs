//! `GET /api/runtime/connect` — the one endpoint *runtimes* dial.
//!
//! Distinct from `/api/vendor/connect`, which vendor *processes* dial. A vendor
//! running inside this server has no listener of its own, so its runtimes come
//! back here: one port, and TLS and reverse proxies for free.
//!
//! The bearer is self-describing — `<project>.<runtime>.<tag>`, signed with the
//! project's dial secret — so this handler knows which secret to check it
//! against. A sandbox learning its own project id is not a disclosure: it is
//! that project's own sandbox.
//!
//! This route stays *outside* the `/api/p/{project}` prefix for that reason:
//! the token already names the scope, and a path segment beside it would be a
//! second answer to one question, checkable only against the first.
//!
//! **The secret is per account, so verification is two-phase.** The account has
//! to be read out of the token before the secret that validates it can be
//! fetched, which means the id is *claimed* until the tag checks out. The only
//! thing done with the claim is a bare settings read for that account's secret
//! — deliberately not [`ProjectRegistry::get`], which builds the account when it
//! is absent, and would let any stranger's `Bearer whatever.x.y` spawn a
//! supervisor, a dial secret and a sweep task per request. Nothing else touches
//! the account until the tag has checked out.
//!
//! **This route is outside `require_auth`'s allowlist on purpose.** A runtime
//! holds no session credential and never will; the token below is the whole
//! authentication, and the middleware — which only knows how to verify a
//! session credential — answered 401 to every dial-back on any deployment with
//! authentication turned on.
//!
//! [`ProjectRegistry::get`]: crate::projects::ProjectRegistry::get
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
    let Some(account) = horsie_support::dial_token::claimed_account(&token) else {
        return refused();
    };
    let account = crate::projects::ProjectId::new(account);

    // An unknown account, an account that has never owned a runtime, and a bad
    // signature are all the same answer on the wire: none tells a stranger
    // whether an account exists.
    let secret = match crate::config::dial_secret_of(&state.shared.db, &account).await {
        Ok(Some(secret)) => secret,
        Ok(None) => return refused(),
        Err(e) => {
            tracing::error!(error = %e, "reading a dial secret failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not verify the token",
            )
                .into_response();
        }
    };
    let claims = match horsie_support::dial_token::verify(&secret, &token) {
        Ok(claims) => claims,
        Err(_) => return refused(),
    };

    // The account's services are deliberately *not* built here any more. They
    // were, to reach that account's connected-runtime registry; a pump needs
    // only a bus and three names off the token, so materialising an entire
    // account — a supervisor, a dial secret, a sweep task — on a dial-back is
    // now pure cost. The account is already known to exist: the dial secret
    // read above is what proved it.
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

    let bus = state.shared.bus.clone();
    let account = claims.user_id;
    let runtime_id = claims.runtime_id;
    let incarnation = claims.incarnation;
    tokio::spawn(async move {
        match on_upgrade.await {
            Ok(upgraded) => {
                let ws =
                    WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, None)
                        .await;
                // Every name comes from the *verified token*, never from what
                // the peer announces. A token authorises exactly one runtime,
                // which is what stops an authenticated peer answering as a
                // different one; exactly one provision of it, which is what
                // stops a sandbox left over from an earlier attempt sharing the
                // live one's topic and running every tool call twice; and
                // exactly one account, which is what keeps two accounts' equally
                // named sandboxes off each other's topics.
                crate::http::runtime_pump::pump(ws, bus, &account, &runtime_id, &incarnation).await;
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
