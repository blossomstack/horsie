//! `/api/auth/*` — the browser's login surface, plus the middleware every other
//! `/api` route sits behind.
//!
//! The browser authenticates by cookie rather than a header because it has no
//! choice: both event streams use the native `EventSource`, which cannot set
//! headers. Non-browser callers (the CLI, vendor processes) send
//! `Authorization: Bearer` and are accepted by the same code path.

use crate::auth::{DeviceError, LoginError, Principal};
use crate::http::AppState;
use crate::http::error::Api;
use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use horsie_models::auth::{
    AgentTokenCreateInput, AgentTokenCreated, AgentTokenView, AuthStatus, DeviceApprovalRequest,
    DeviceCodeResponse, DeviceTokenRequest, LoginRequest, PasswordChangeRequest, RefreshRequest,
    TokenPair,
};

pub const COOKIE_NAME: &str = "horsie_session";

/// Browser sessions last 30 days — matches the token TTL the service mints.
const COOKIE_MAX_AGE_SECS: i64 = 30 * 24 * 60 * 60;

/// Paths reachable without a credential. `/api/auth/status` and `/api/auth/login`
/// are how a caller becomes authenticated in the first place; `/api/health` is a
/// liveness probe; plugin artifacts carry their own capability token and are
/// fetched by runtimes that have no session cookie.
fn is_public(path: &str) -> bool {
    path == "/api/health"
        || path == "/api/auth/status"
        || path == "/api/auth/login"
        // How a CLI becomes authenticated in the first place. Approval, which
        // is the actual authorization step, requires the browser cookie.
        || path == "/api/device/auth/code"
        || path == "/api/device/auth/token"
        || path == "/api/device/auth/refresh"
        || path.starts_with("/api/plugin-artifacts/")
}

/// The account a delegating front layer has already authenticated.
///
/// A request extension rather than a header: an extension can only have been
/// set by code running in this process, whereas a header is whatever the
/// caller sent unless every deployment remembers to strip it at every edge.
#[derive(Clone, Debug)]
pub struct DelegatedIdentity(pub crate::auth::UserId);

/// Resolve a caller into a [`Principal`] and put it in the request extensions,
/// or answer `401`.
///
/// One branch per [`AuthMode`]. The delegated branch is the one to read
/// carefully: an absent identity is `401`, deliberately, and never a fall back
/// to the anonymous account. Falling back would mean a single missing or
/// mis-ordered layer silently serves every caller the *same* account's data,
/// while every request succeeds and every page renders — the failure would not
/// announce itself, so it is a test rather than a comment.
pub async fn require_auth(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    match state.auth.mode() {
        crate::auth::AuthMode::Off => {
            req.extensions_mut().insert(Principal::Anonymous);
            return next.run(req).await;
        }
        crate::auth::AuthMode::Delegated => {
            if is_public(req.uri().path()) {
                return next.run(req).await;
            }
            let Some(id) = req.extensions().get::<DelegatedIdentity>().cloned() else {
                tracing::warn!(
                    path = %req.uri().path(),
                    "a request reached a delegated deployment with no identity attached"
                );
                return unauthorized();
            };
            req.extensions_mut().insert(Principal::User(id.0));
            return next.run(req).await;
        }
        crate::auth::AuthMode::Password => {}
    }
    if is_public(req.uri().path()) {
        return next.run(req).await;
    }
    let Some(secret) = credential(req.headers()) else {
        return unauthorized();
    };
    match state.auth.verify(&secret).await {
        Ok(Some(v)) => {
            req.extensions_mut().insert(v.principal);
            next.run(req).await
        }
        Ok(None) => unauthorized(),
        Err(e) => {
            tracing::error!(error = %e, "verifying a credential failed");
            Api::internal("could not verify the credential").into_response()
        }
    }
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(horsie_models::session_api::ApiError {
            code: "unauthorized".to_string(),
            message: "authentication required".to_string(),
        }),
    )
        .into_response()
}

/// The bearer header if present, else the session cookie.
fn credential(headers: &HeaderMap) -> Option<String> {
    if let Some(bearer) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        return Some(bearer.to_string());
    }
    cookie_value(headers, COOKIE_NAME)
}

/// Pull one cookie out of the `Cookie` header. Hand-rolled rather than pulling
/// in a cookie crate: one name, no attributes to parse on the request side.
fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

/// `Secure` only when the request actually arrived over TLS. Setting it
/// unconditionally would make the cookie unusable on a plain-HTTP localhost
/// deployment, which is the default self-host shape.
fn arrived_over_tls(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .next()
                .is_some_and(|p| p.trim().eq_ignore_ascii_case("https"))
        })
}

fn set_cookie(res: &mut Response, value: &str) {
    match axum::http::HeaderValue::from_str(value) {
        Ok(v) => {
            res.headers_mut().insert(header::SET_COOKIE, v);
        }
        Err(e) => tracing::error!(error = %e, "building the session cookie failed"),
    }
}

/// horsie's own answer about itself: it owns the credential in every mode that
/// serves this route, so nothing is external and there is nowhere else to send
/// anyone. The three delegated fields exist for a front layer to fill when it
/// answers this endpoint instead.
fn local_status(enabled: bool, authenticated: bool, must_change_password: bool) -> AuthStatus {
    AuthStatus {
        enabled,
        authenticated,
        must_change_password,
        external: false,
        login_url: None,
        logout_url: None,
    }
}

/// `GET /api/auth/status` — reachable unauthenticated, since it is what tells
/// the UI to render a login page.
pub async fn status(State(state): State<AppState>, headers: HeaderMap) -> Json<AuthStatus> {
    if !state.auth.enabled() {
        return Json(local_status(false, false, false));
    }
    let authenticated = match credential(&headers) {
        Some(secret) => matches!(state.auth.verify(&secret).await, Ok(Some(_))),
        None => false,
    };
    // Only ever disclosed to someone already inside.
    let must_change_password =
        authenticated && state.auth.must_change_password().await.unwrap_or(false);
    Json(local_status(true, authenticated, must_change_password))
}

/// `POST /api/auth/login`
///
/// `Json` stays last: axum requires the body-consuming extractor in final
/// position.
pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> Result<Response, Api> {
    let secret = state.auth.login(&body.password).await.map_err(to_api)?;
    let must_change_password = state.auth.must_change_password().await.unwrap_or(false);
    let mut res = Json(local_status(true, true, must_change_password)).into_response();
    let secure = if arrived_over_tls(&headers) {
        "; Secure"
    } else {
        ""
    };
    set_cookie(
        &mut res,
        &format!(
            "{COOKIE_NAME}={secret}; HttpOnly; SameSite=Lax; Path=/; Max-Age={COOKIE_MAX_AGE_SECS}{secure}"
        ),
    );
    Ok(res)
}

/// `POST /api/auth/logout`
pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, Api> {
    if let Some(secret) = credential(&headers) {
        state.auth.logout(&secret).await.map_err(Api::internal)?;
    }
    let mut res = Json(local_status(state.auth.enabled(), false, false)).into_response();
    set_cookie(
        &mut res,
        &format!("{COOKIE_NAME}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0"),
    );
    Ok(res)
}

/// `POST /api/auth/password`
pub async fn change_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PasswordChangeRequest>,
) -> Result<Json<AuthStatus>, Api> {
    let active = credential(&headers).unwrap_or_default();
    state
        .auth
        .change_password(&body.current_password, &body.new_password, &active)
        .await
        .map_err(to_api)?;
    Ok(Json(local_status(true, true, false)))
}

/// `POST /api/device/auth/code` — start a device authorization.
pub async fn device_code(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<DeviceCodeResponse>, Api> {
    let d = state
        .auth
        .start_device_authorization()
        .await
        .map_err(Api::internal)?;
    // Same-origin: the browser page that approves this code is served by this
    // very server, so the request's own host is the right verification URI.
    let base = crate::http::request_base(&headers);
    Ok(Json(DeviceCodeResponse {
        verification_uri: format!("{base}/auth/device"),
        verification_uri_complete: format!("{base}/auth/device?code={}", d.user_code),
        device_code: d.device_code,
        user_code: d.user_code,
        expires_in: d.expires_in,
        interval: d.interval,
    }))
}

/// `POST /api/device/auth/token` — one poll.
pub async fn device_token(
    State(state): State<AppState>,
    Json(body): Json<DeviceTokenRequest>,
) -> Result<Json<TokenPair>, Api> {
    match state.auth.poll_device_token(&body.device_code).await {
        Ok(t) => Ok(Json(pair(t))),
        Err(e) => Err(device_error(e)),
    }
}

/// `POST /api/device/auth/refresh` — rotate a refresh token.
pub async fn refresh(
    State(state): State<AppState>,
    Json(body): Json<RefreshRequest>,
) -> Result<Json<TokenPair>, Api> {
    match state.auth.refresh(&body.refresh_token).await {
        Ok(t) => Ok(Json(pair(t))),
        Err(e) => Err(device_error(e)),
    }
}

fn pair(t: crate::auth::IssuedTokens) -> TokenPair {
    TokenPair {
        access_token: t.access_token,
        refresh_token: t.refresh_token,
        expires_in: u32::try_from(t.expires_in).unwrap_or(u32::MAX),
    }
}

/// `POST /api/device/approve` — cookie-authenticated. The principal comes
/// from the middleware, so a code is always approved *as* whoever is logged in.
pub async fn device_approve(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<Principal>,
    Json(body): Json<DeviceApprovalRequest>,
) -> Result<StatusCode, Api> {
    let approved = state
        .auth
        .approve_device(&body.user_code, &principal)
        .await
        .map_err(Api::internal)?;
    answered(approved)
}

/// `POST /api/device/deny` — cookie-authenticated.
pub async fn device_deny(
    State(state): State<AppState>,
    Json(body): Json<DeviceApprovalRequest>,
) -> Result<StatusCode, Api> {
    let denied = state
        .auth
        .deny_device(&body.user_code)
        .await
        .map_err(Api::internal)?;
    answered(denied)
}

/// Unknown, expired, and already-answered codes are one 404: the person at the
/// browser can do the same thing about all three — start over.
fn answered(ok: bool) -> Result<StatusCode, Api> {
    if ok {
        Ok(StatusCode::OK)
    } else {
        Err(Api::not_found(
            "that code is not waiting for an answer — it may have expired or already been used",
        ))
    }
}

/// Poll/refresh failures answer `400` with the RFC's error name as the code, so
/// a client can branch on `authorization_pending` vs `slow_down` without
/// parsing prose.
fn device_error(e: DeviceError) -> Api {
    let (code, message) = match e {
        DeviceError::AuthorizationPending => (
            "authorization_pending",
            "waiting for the code to be approved in a browser",
        ),
        DeviceError::SlowDown => ("slow_down", "polling too fast"),
        DeviceError::ExpiredToken => ("expired_token", "that code has expired or was already used"),
        DeviceError::AccessDenied => ("access_denied", "that request was denied"),
        DeviceError::Internal(m) => return Api::internal(m),
    };
    Api(
        StatusCode::BAD_REQUEST,
        horsie_models::session_api::ApiError {
            code: code.to_string(),
            message: message.to_string(),
        },
    )
}

/// `GET /api/device/tokens` — the machine tokens this deployment has minted.
pub async fn list_agent_tokens(
    State(state): State<AppState>,
) -> Result<Json<Vec<AgentTokenView>>, Api> {
    let tokens = state
        .auth
        .list_agent_tokens()
        .await
        .map_err(Api::internal)?;
    Ok(Json(tokens.into_iter().map(token_view).collect()))
}

/// `POST /api/device/tokens` — mint one. The secret comes back exactly once.
pub async fn create_agent_token(
    State(state): State<AppState>,
    axum::Extension(principal): axum::Extension<Principal>,
    Json(body): Json<AgentTokenCreateInput>,
) -> Result<(StatusCode, Json<AgentTokenCreated>), Api> {
    let (token, summary) = state
        .auth
        .mint_agent_token(&body.label, &principal)
        .await
        .map_err(Api::unprocessable)?;
    Ok((
        StatusCode::CREATED,
        Json(AgentTokenCreated {
            token,
            view: token_view(summary),
        }),
    ))
}

/// `DELETE /api/device/tokens/:id` — revoke. Idempotent: revoking an already-dead
/// token is the state the caller asked for.
pub async fn delete_agent_token(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<StatusCode, Api> {
    state
        .auth
        .revoke_agent_token(&id)
        .await
        .map_err(Api::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

fn token_view(t: crate::auth::TokenSummary) -> AgentTokenView {
    AgentTokenView {
        id: t.id,
        label: t.label.unwrap_or_default(),
        created_at: t.created_at.to_string(),
        last_used_at: t.last_used_at.map(|v| v.to_string()),
    }
}

fn to_api(e: LoginError) -> Api {
    match e {
        LoginError::BadCredentials => Api(
            StatusCode::UNAUTHORIZED,
            horsie_models::session_api::ApiError {
                code: "unauthorized".to_string(),
                message: "incorrect password".to_string(),
            },
        ),
        LoginError::WeakPassword(m) => Api::unprocessable(m),
        LoginError::Internal(m) => Api::internal(m),
    }
}
