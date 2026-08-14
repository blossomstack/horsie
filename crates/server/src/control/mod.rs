//! The control plane: one declaration per management operation, from which
//! both the HTTP route and the agent toolbox are derived.
//!
//! A route that is not an operation cannot be mounted through the fold in
//! [`http`] — the fold *is* the mounting point, so classifying an operation and
//! mounting it are the same act rather than two lists someone keeps in step.

use crate::http::error::Api;
use crate::users::UserServices;
use futures_util::future::BoxFuture;
use horsie_agentcore::ToolCallError;
use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::future::Future;
use std::sync::Arc;

pub mod agents;
pub mod environments;
pub mod http;
pub mod routines;

/// One manageable noun and everything you can do to it.
///
/// The name lives here rather than on each operation, so a resource cannot end
/// up with two spellings of itself — which would quietly split it across two
/// tools and two sets of routes.
pub trait Resource: Send + Sync {
    /// The noun. Becomes the tool `horsie_<name>` and groups the routes.
    fn name(&self) -> &'static str;

    /// Every operation on this resource, in any order.
    fn operations(&self) -> Vec<Operation>;
}

/// Every resource the control plane manages. A new one is one line.
pub fn resources() -> Vec<Box<dyn Resource>> {
    vec![
        Box::new(agents::Agents),
        Box::new(routines::Routines),
        Box::new(environments::Environments),
    ]
}

/// The whole control plane, with every operation stamped with its resource.
pub fn operations() -> Vec<Operation> {
    resources()
        .iter()
        .flat_map(|resource| {
            let name = resource.name();
            resource
                .operations()
                .into_iter()
                .map(move |operation| operation.on(name))
        })
        .collect()
}

/// Ask the session supervisor a question, mapping a closed mailbox to an
/// internal error.
///
/// Lives here rather than in `http::handlers` because both surfaces need it and
/// `ControlError` is the base vocabulary; `handlers::ask` is now a rendering of
/// this into `Api`.
pub(crate) async fn ask<T, F>(services: &UserServices, make: F) -> Result<T, ControlError>
where
    F: FnOnce(horsie_actor::ReplyTo<T>) -> crate::sessions::supervisor::SessionSupervisorCommand,
    T: Send + 'static,
{
    services
        .supervisor
        .ask(make)
        .await
        .map_err(|_| ControlError::Internal("session supervisor unavailable".to_string()))
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
}

/// Which surfaces an operation reaches.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Expose {
    /// Route only.
    Api,
    /// Route and tool. The common case.
    ApiAndTool,
    /// Tool only, with no route of its own — for a route that cannot be an
    /// operation because it is a stream, but whose non-streaming half a tool
    /// still wants. Both then call one shared function.
    ToolOnly,
}

/// What a successful operation answers with over HTTP.
///
/// Declared per operation rather than inferred from the method: `POST` means
/// 201 when it creates a preset and 200 when it runs a routine, and the
/// difference is part of each route's contract.
///
/// A tool never sees this — it gets the JSON value either way.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Success {
    /// 200 with the JSON body.
    Ok,
    /// 201 with the JSON body.
    Created,
    /// 204 with no body.
    NoContent,
}

type Run = Arc<
    dyn Fn(Arc<UserServices>, Value) -> BoxFuture<'static, Result<Value, ControlError>>
        + Send
        + Sync,
>;

/// One management operation, and the single place it is declared.
#[derive(Clone)]
pub struct Operation {
    /// Groups operations into one tool: "agents", "workflows", …
    ///
    /// Empty until [`Resource`] stamps it in [`operations`], which is the only
    /// way an operation reaches either surface. `resource_is_always_stamped`
    /// is the guard.
    pub resource: &'static str,
    /// The `action` value within that tool: "list", "create", "invoke", …
    pub action: &'static str,
    pub method: Method,
    /// axum path template. Every `{param}` must also be a field of the input
    /// type — the HTTP adapter merges path and query params into the input
    /// object, so a tool and a route see one identical shape.
    pub path: &'static str,
    /// Written for the model, and the OpenAPI summary if we ever want one.
    pub summary: &'static str,
    pub expose: Expose,
    pub success: Success,
    pub schema: Value,
    run: Run,
}

impl Operation {
    pub async fn run(
        &self,
        services: Arc<UserServices>,
        input: Value,
    ) -> Result<Value, ControlError> {
        (self.run)(services, input).await
    }

    /// This operation answers 201 rather than 200.
    #[must_use]
    pub fn created(mut self) -> Self {
        self.success = Success::Created;
        self
    }

    /// This operation answers 204 with no body.
    #[must_use]
    pub fn no_content(mut self) -> Self {
        self.success = Success::NoContent;
        self
    }

    /// Name the resource this belongs to. Called once, by [`operations`].
    #[must_use]
    fn on(mut self, resource: &'static str) -> Self {
        self.resource = resource;
        self
    }
}

/// Declare an operation. `f` is the whole implementation; the HTTP handler is a
/// fold over the table rather than a second copy of it.
#[allow(clippy::too_many_arguments)]
pub fn op<I, O, F, Fut>(
    action: &'static str,
    method: Method,
    path: &'static str,
    summary: &'static str,
    expose: Expose,
    f: F,
) -> Operation
where
    I: DeserializeOwned + JsonSchema + Send + 'static,
    O: Serialize + Send + 'static,
    F: Fn(Arc<UserServices>, I) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<O, ControlError>> + Send + 'static,
{
    Operation {
        // Stamped by `Resource`, which is where the name is written once.
        resource: "",
        action,
        method,
        path,
        summary,
        expose,
        success: Success::Ok,
        schema: schema_for::<I>(),
        run: Arc::new(move |services, raw| {
            let f = f.clone();
            Box::pin(async move {
                let input: I = serde_json::from_value(raw)
                    .map_err(|e| ControlError::Invalid(e.to_string()))?;
                let out = f(services, input).await?;
                serde_json::to_value(out).map_err(|e| ControlError::Internal(e.to_string()))
            })
        }),
    }
}

/// Subschemas are inlined: a tool `input_schema` carrying `$defs` and `$ref` is
/// not reliably read by every provider, and these schemas are small enough that
/// inlining costs nothing.
fn schema_for<I: JsonSchema>() -> Value {
    schemars::generate::SchemaSettings::default()
        .with(|s| s.inline_subschemas = true)
        .into_generator()
        .into_root_schema_for::<I>()
        .to_value()
}

/// The input of an operation that takes none.
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct NoInput {}

/// The input of an operation addressed only by its slug.
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct NameRef {
    /// Slug of the resource, as it appears in the path.
    pub name: String,
}

/// The shared failure vocabulary. Each surface renders it its own way: HTTP as
/// a status plus an `ApiError` body, a tool as a `ToolCallError` the model reads
/// and retries against.
#[derive(Debug)]
pub enum ControlError {
    NotFound(String),
    /// `code` is the machine-readable envelope tag. The service errors carry no
    /// code of their own, so conversions default it to `duplicate`; a call site
    /// with a better one (`agent_in_use`) builds this variant directly.
    Conflict {
        code: String,
        message: String,
    },
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
/// The conflict code is per-resource because the routes already differ:
/// agents and environments answer `duplicate`, routines and workflows answer
/// `conflict`. Neither is better, but changing either silently would alter a
/// wire contract for no reason.
macro_rules! from_service_error {
    ($($ty:path => $code:literal),+ $(,)?) => {$(
        impl From<$ty> for ControlError {
            fn from(e: $ty) -> Self {
                // Aliased because a `path` macro fragment cannot be followed by
                // `::Variant` in a pattern.
                use $ty as ServiceError;
                match e {
                    ServiceError::NotFound(m) => Self::NotFound(m),
                    ServiceError::Conflict(m) => Self::Conflict {
                        code: $code.to_string(),
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
    crate::agents::AgentError => "duplicate",
    crate::workflows::WorkflowError => "conflict",
    crate::routines::RoutineError => "conflict",
    crate::environments::EnvironmentError => "duplicate",
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
pub(crate) mod tests {
    use super::*;

    /// The merge in [`http`] can only fill a path param the input type
    /// declares. A mismatch would 422 every call to that route, so every
    /// resource asserts this over its own table.
    pub(crate) fn assert_path_params_are_inputs(operations: &[Operation]) {
        for operation in operations {
            for param in operation
                .path
                .split('/')
                .filter_map(|s| s.strip_prefix('{').and_then(|s| s.strip_suffix('}')))
            {
                assert!(
                    operation.schema["properties"].get(param).is_some(),
                    "{}.{} takes {{{}}} in its path but not in its input",
                    operation.resource,
                    operation.action,
                    param
                );
            }
        }
    }

    #[test]
    fn resource_is_always_stamped() {
        // `op` cannot know its resource, so nothing may reach a surface with
        // the placeholder still on it.
        for operation in operations() {
            assert!(
                !operation.resource.is_empty(),
                "{} escaped `Resource` unstamped",
                operation.action
            );
        }
    }

    #[test]
    fn every_resource_declares_a_distinct_action_set() {
        // Two operations answering to one (resource, action) would make the
        // toolbox's dispatch map silently drop one of them.
        let mut seen = std::collections::BTreeSet::new();
        for operation in operations() {
            assert!(
                seen.insert((operation.resource, operation.action)),
                "{}.{} is declared twice",
                operation.resource,
                operation.action
            );
        }
    }

    #[test]
    fn no_two_operations_claim_one_method_and_path() {
        // The fold merges method routers per path; a genuine duplicate would
        // panic axum at boot rather than fail a test, so catch it here.
        let mut seen = std::collections::BTreeSet::new();
        for operation in operations().iter().filter(|o| o.expose != Expose::ToolOnly) {
            assert!(
                seen.insert((operation.method, operation.path)),
                "{:?} {} is claimed twice",
                operation.method,
                operation.path
            );
        }
    }

    #[test]
    fn service_errors_keep_their_status() {
        let api: Api =
            ControlError::from(crate::agents::AgentError::NotFound("nope".into())).into();
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
    fn each_resource_keeps_the_conflict_code_its_route_already_answered() {
        let api: Api =
            ControlError::from(crate::routines::RoutineError::Conflict("taken".into())).into();
        assert_eq!(api.1.code, "conflict");

        let api: Api =
            ControlError::from(crate::agents::AgentError::Conflict("taken".into())).into();
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

    #[derive(serde::Deserialize, schemars::JsonSchema)]
    struct Greeting {
        /// Who to greet.
        who: String,
    }

    fn greet() -> Operation {
        op(
            "say",
            Method::Post,
            "/api/greetings",
            "Say hello.",
            Expose::ApiAndTool,
            |_services: Arc<UserServices>, input: Greeting| async move {
                Ok::<String, ControlError>(format!("hello {}", input.who))
            },
        )
    }

    #[test]
    fn op_derives_its_schema_from_the_input_type() {
        let operation = greet();
        assert_eq!(operation.schema["properties"]["who"]["type"], "string");
        assert_eq!(operation.schema["required"][0], "who");
        assert!(
            operation.schema.get("$defs").is_none(),
            "subschemas must be inlined: a provider that ignores $ref would see an empty schema"
        );
    }

    #[tokio::test]
    async fn op_runs_its_fn_and_rejects_input_that_does_not_deserialize() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::testing::state(dir.path()).build().await;
        let services = state.services().await;

        let out = greet()
            .run(services.clone(), serde_json::json!({"who": "world"}))
            .await
            .unwrap();
        assert_eq!(out, serde_json::json!("hello world"));

        let err = greet()
            .run(services, serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ControlError::Invalid(_)));
    }
}
