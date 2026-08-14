//! The control plane: one declaration per management operation, from which
//! both the HTTP route and the agent toolbox are derived.
//!
//! A route that is not an operation cannot be mounted through the fold in
//! [`http`] — the fold *is* the mounting point, so classifying an operation and
//! mounting it are the same act rather than two lists someone keeps in step.

use crate::http::error::Api;
use horsie_agentcore::ToolCallError;

pub mod http;

/// The shared failure vocabulary. Each surface renders it its own way: HTTP as
/// a status plus an `ApiError` body, a tool as a `ToolCallError` the model reads
/// and retries against.
#[derive(Debug)]
pub enum ControlError {
    NotFound(String),
    /// `code` is the machine-readable envelope tag. The service errors carry no
    /// code of their own, so conversions default it to `duplicate`; a call site
    /// with a better one (`agent_in_use`) builds this variant directly.
    Conflict { code: String, message: String },
    Invalid(String),
    Internal(String),
}

impl From<ControlError> for Api {
    fn from(e: ControlError) -> Self {
        match e {
            ControlError::NotFound(m) => Self::not_found(m),
            ControlError::Conflict { code, message } => Self::conflict(&code, message),
            ControlError::Invalid(m) => Self::unprocessable(m),
            ControlError::Internal(m) => Self::internal(m),
        }
    }
}

impl From<ControlError> for ToolCallError {
    fn from(e: ControlError) -> Self {
        match e {
            // Everything the model can act on by calling again with different
            // input reads as invalid input, whatever its HTTP status would be.
            ControlError::NotFound(m) | ControlError::Invalid(m) => Self::InvalidInput(m),
            ControlError::Conflict { message, .. } => Self::InvalidInput(message),
            // Ours, not theirs. Retrying the same call will not help.
            ControlError::Internal(m) => Self::ExecutionFailed(m),
        }
    }
}

/// The four service error enums are structurally identical, so their
/// conversions are too.
macro_rules! from_service_error {
    ($($ty:path),+ $(,)?) => {$(
        impl From<$ty> for ControlError {
            fn from(e: $ty) -> Self {
                // Aliased because a `path` macro fragment cannot be followed by
                // `::Variant` in a pattern.
                use $ty as ServiceError;
                match e {
                    ServiceError::NotFound(m) => Self::NotFound(m),
                    ServiceError::Conflict(m) => Self::Conflict {
                        code: "duplicate".to_string(),
                        message: m,
                    },
                    ServiceError::Invalid(m) => Self::Invalid(m),
                    ServiceError::Internal(m) => Self::Internal(m),
                }
            }
        }
    )+};
}

from_service_error!(
    crate::agents::AgentError,
    crate::workflows::WorkflowError,
    crate::routines::RoutineError,
    crate::environments::EnvironmentError,
);

/// A spec that could not be assembled: the caller's fault is invalid, ours is
/// internal. Mirrors the `Api` conversion at `http/error.rs`.
impl From<crate::sessions::builder::SpecError> for ControlError {
    fn from(e: crate::sessions::builder::SpecError) -> Self {
        match e {
            crate::sessions::builder::SpecError::Invalid(m) => Self::Invalid(m),
            crate::sessions::builder::SpecError::Internal(m) => Self::Internal(m),
        }
    }
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
    fn service_errors_keep_their_status() {
        let api: Api = ControlError::from(crate::agents::AgentError::NotFound("nope".into())).into();
        assert_eq!(api.0, axum::http::StatusCode::NOT_FOUND);

        let api: Api =
            ControlError::from(crate::workflows::WorkflowError::Invalid("bad".into())).into();
        assert_eq!(api.0, axum::http::StatusCode::UNPROCESSABLE_ENTITY);

        let api: Api = ControlError::Conflict {
            code: "agent_in_use".into(),
            message: "still used".into(),
        }
        .into();
        assert_eq!(api.0, axum::http::StatusCode::CONFLICT);
        assert_eq!(api.1.code, "agent_in_use");
    }

    #[test]
    fn a_service_conflict_keeps_the_default_code() {
        let api: Api =
            ControlError::from(crate::routines::RoutineError::Conflict("taken".into())).into();
        assert_eq!(api.1.code, "duplicate");
    }

    #[test]
    fn invalid_becomes_invalid_input_for_a_tool() {
        let err: ToolCallError = ControlError::Invalid("bad".into()).into();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    #[test]
    fn internal_is_not_something_the_model_can_retry() {
        let err: ToolCallError = ControlError::Internal("db is on fire".into()).into();
        assert!(matches!(err, ToolCallError::ExecutionFailed(_)));
    }
}
