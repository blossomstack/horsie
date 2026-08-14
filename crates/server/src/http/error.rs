//! Uniform HTTP error envelope: every failure body is a wire `ApiError`.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use horsie_models::session_api::ApiError;

pub struct Api(pub StatusCode, pub ApiError);

impl Api {
    fn new(status: StatusCode, code: &str, message: impl Into<String>) -> Self {
        Self(
            status,
            ApiError {
                code: code.to_string(),
                message: message.into(),
            },
        )
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    pub fn conflict(code: &str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code, message)
    }

    pub fn unprocessable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_spec", message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message)
    }

    pub fn bad_gateway(code: &str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, code, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }

    /// This node cannot serve the request, but another one can.
    ///
    /// A clustered node that has lost touch with a quorum: it cannot know
    /// whether its instances have been handed to somebody else, so it must not
    /// answer from them. Distinct from a 500 because nothing is broken and the
    /// caller should retry rather than report a fault.
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable", message)
    }
}

/// A spec that could not be assembled: the caller's fault is a 422, ours a 500.
impl From<crate::sessions::builder::SpecError> for Api {
    fn from(e: crate::sessions::builder::SpecError) -> Self {
        match e {
            crate::sessions::builder::SpecError::Invalid(m) => Self::unprocessable(m),
            crate::sessions::builder::SpecError::Internal(m) => Self::internal(m),
        }
    }
}

impl IntoResponse for Api {
    fn into_response(self) -> Response {
        (self.0, Json(self.1)).into_response()
    }
}
