//! `/api/auth/*` — the browser's login surface, plus the middleware every other
//! `/api` route sits behind.
//!
//! The browser authenticates by cookie rather than a header because it has no
//! choice: both event streams use the native `EventSource`, which cannot set
//! headers. Non-browser callers (the CLI, vendor agents) send
//! `Authorization: Bearer` and are accepted by the same code path.

use crate::auth::{LoginError, Principal};
use crate::http::AppState;
use crate::http::error::Api;
use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use horsie_models::auth::{AuthStatus, LoginRequest, PasswordChangeRequest};

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
        || path.starts_with("/api/plugin-artifacts/")
}

/// Resolve a credential into a [`Principal`] and put it in the request
/// extensions, or answer `401`. With auth disabled every request is
/// `Principal::Anonymous`, which is today's behaviour exactly.
pub async fn require_auth(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    if !state.auth.enabled() {
        req.extensions_mut().insert(Principal::Anonymous);
        return next.run(req).await;
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

/// `GET /api/auth/status` — reachable unauthenticated, since it is what tells
/// the UI to render a login page.
pub async fn status(State(state): State<AppState>, headers: HeaderMap) -> Json<AuthStatus> {
    if !state.auth.enabled() {
        return Json(AuthStatus {
            enabled: false,
            authenticated: false,
            must_change_password: false,
        });
    }
    let authenticated = match credential(&headers) {
        Some(secret) => matches!(state.auth.verify(&secret).await, Ok(Some(_))),
        None => false,
    };
    // Only ever disclosed to someone already inside.
    let must_change_password =
        authenticated && state.auth.must_change_password().await.unwrap_or(false);
    Json(AuthStatus {
        enabled: true,
        authenticated,
        must_change_password,
    })
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
    let mut res = Json(AuthStatus {
        enabled: true,
        authenticated: true,
        must_change_password,
    })
    .into_response();
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
    let mut res = Json(AuthStatus {
        enabled: state.auth.enabled(),
        authenticated: false,
        must_change_password: false,
    })
    .into_response();
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
    Ok(Json(AuthStatus {
        enabled: true,
        authenticated: true,
        must_change_password: false,
    }))
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
