//! The MCP OAuth pair (`/api/mcp/servers/:name/connect` and its callback).
//!
//! Everything else about a configured server is a control-plane operation in
//! [`crate::control::mcp`]. These two are not, because both build their
//! `redirect_uri` from the host headers of the request that carried them, and
//! an operation is handed only its input.

use crate::github::urlencode;
use crate::http::error::Api;
use crate::http::{Scope, Scoped};
use axum::Json;
use axum::extract::Query;
use axum::http::HeaderMap;
use axum::response::Redirect;
use horsie_models::mcp::McpAuthorizeUrl;
use serde::Deserialize;

/// `POST /api/mcp/servers/:name/connect` — begin OAuth for an `oauth` server:
/// discover + (if needed) register a client, then return the authorize URL for
/// the browser to navigate to. Non-oauth servers use `/test` instead.
pub async fn connect(
    Scope(state): Scope,
    Scoped(name): Scoped<String>,
    headers: HeaderMap,
) -> Result<Json<McpAuthorizeUrl>, Api> {
    let base = crate::http::request_base(&headers);
    let url = state
        .mcp
        .connect_oauth(&name, &base)
        .await
        .map_err(Api::unprocessable)?;
    Ok(Json(McpAuthorizeUrl { url }))
}

/// `GET /api/mcp/servers/:name/oauth/callback` — exchange the code and redirect
/// back into the Settings UI with the outcome (mirrors the github callback).
pub async fn oauth_callback(
    Scope(state): Scope,
    Scoped(name): Scoped<String>,
    Query(q): Query<OAuthCallbackQuery>,
    headers: HeaderMap,
) -> Redirect {
    let base = crate::http::request_base(&headers);
    let page = crate::http::github::settings_page(&state.project);
    let dest = match (q.code, q.state) {
        (Some(code), Some(st)) => {
            match state
                .mcp
                .handle_oauth_callback(&name, &code, &st, &base)
                .await
            {
                Ok(()) => format!("{page}?mcp_connected={}", urlencode(&name)),
                Err(e) => format!("{page}?mcp_error={}", urlencode(&e)),
            }
        }
        _ => format!(
            "{page}?mcp_error={}",
            urlencode(
                &q.error_description
                    .or(q.error)
                    .unwrap_or_else(|| "authorization was denied".to_string())
            )
        ),
    };
    Redirect::temporary(&dest)
}

#[derive(Deserialize)]
pub struct OAuthCallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}
