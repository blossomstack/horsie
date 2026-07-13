//! Configured-MCP-server endpoints (`/api/mcp/servers`): CRUD plus a
//! connect/smoke-test. Thin wrappers over [`crate::mcp::McpService`]; bodies are
//! fluorite wire types and views never carry secrets.

use crate::github::urlencode;
use crate::http::AppState;
use crate::http::error::Api;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Redirect;
use horsie_models::mcp::{
    McpAuthorizeUrl, McpConnectResult, McpServerInput, McpServerList, McpServerView,
};
use serde::Deserialize;

pub async fn list(State(state): State<AppState>) -> Result<Json<McpServerList>, Api> {
    let servers = state.mcp.list().await.map_err(Api::internal)?;
    Ok(Json(McpServerList { servers }))
}

/// `PUT /api/mcp/servers/:name` — upsert; the path is the id of record, so it
/// overrides any `name` in the body.
pub async fn upsert(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(mut input): Json<McpServerInput>,
) -> Result<Json<McpServerView>, Api> {
    input.name = name;
    state
        .mcp
        .upsert(input)
        .await
        .map(Json)
        .map_err(Api::unprocessable)
}

pub async fn delete(State(state): State<AppState>, Path(name): Path<String>) -> Result<(), Api> {
    state.mcp.delete(&name).await.map_err(Api::internal)
}

/// `POST /api/mcp/servers/:name/test` — connect (`initialize` + `tools/list`),
/// persist the outcome, and return it. Always `200` with the result envelope;
/// a failed connect is `ok: false` with `error`, not an HTTP error.
pub async fn test(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<McpConnectResult>, Api> {
    state.mcp.test(&name).await.map(Json).map_err(Api::internal)
}

/// `POST /api/mcp/servers/:name/connect` — begin OAuth for an `oauth` server:
/// discover + (if needed) register a client, then return the authorize URL for
/// the browser to navigate to. Non-oauth servers use `/test` instead.
pub async fn connect(
    State(state): State<AppState>,
    Path(name): Path<String>,
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
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<OAuthCallbackQuery>,
    headers: HeaderMap,
) -> Redirect {
    let base = crate::http::request_base(&headers);
    let dest = match (q.code, q.state) {
        (Some(code), Some(st)) => {
            match state
                .mcp
                .handle_oauth_callback(&name, &code, &st, &base)
                .await
            {
                Ok(()) => format!("/settings?mcp_connected={}", urlencode(&name)),
                Err(e) => format!("/settings?mcp_error={}", urlencode(&e)),
            }
        }
        _ => format!(
            "/settings?mcp_error={}",
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
