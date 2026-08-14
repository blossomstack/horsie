//! The HTTP surface of the control plane: every JSON route, folded out of the
//! operation table.

use crate::control::{Expose, Method, Operation, Success};
use crate::http::{AppState, Scope, error::Api};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Query, RawPathParams};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, delete, get, post, put};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

/// Mount every routed operation.
///
/// Paths are collected before being mounted because two operations may share a
/// path with different methods, and axum panics when two `route` calls claim
/// one path. [`Expose::ToolOnly`] operations are skipped — that is what the
/// variant means.
pub fn router(operations: &[Operation]) -> axum::Router<AppState> {
    let mut by_path: BTreeMap<&'static str, MethodRouter<AppState>> = BTreeMap::new();
    for operation in operations.iter().filter(|o| o.expose != Expose::ToolOnly) {
        let mounted = handler_for(operation.clone());
        by_path
            .entry(operation.path)
            .and_modify(|existing| *existing = existing.clone().merge(mounted.clone()))
            .or_insert(mounted);
    }
    by_path
        .into_iter()
        .fold(axum::Router::new(), |router, (path, methods)| {
            router.route(path, methods)
        })
}

/// One operation as an axum handler.
///
/// `RawPathParams` rather than `Path<HashMap<_, _>>`: the latter rejects a
/// request on a route with no path params at all, which is most of them.
fn handler_for(operation: Operation) -> MethodRouter<AppState> {
    let method = operation.method;
    let success = operation.success;
    let run = move |Scope(services): Scope,
                    params: RawPathParams,
                    Query(query): Query<HashMap<String, String>>,
                    body: Bytes| {
        let operation = operation.clone();
        async move {
            let mut input = if body.is_empty() {
                Value::Object(serde_json::Map::new())
            } else {
                serde_json::from_slice(&body)
                    .map_err(|e| Api::unprocessable(format!("malformed JSON body: {e}")))?
            };
            let path_params: Vec<(String, String)> = params
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect();
            let mut conflicts = merge_params(&mut input, path_params.into_iter());
            conflicts.extend(merge_params(&mut input, query.into_iter()));
            if let Some(key) = conflicts.first() {
                return Err(Api::unprocessable(format!(
                    "'{key}' is immutable; the path is the id of record"
                )));
            }
            let value = operation.run(services, input).await.map_err(Api::from)?;
            Ok::<Response, Api>(match success {
                Success::Ok => (StatusCode::OK, Json(value)).into_response(),
                Success::Created => (StatusCode::CREATED, Json(value)).into_response(),
                // No body at all, so nothing to serialise: the operation's
                // output type for these is `()`.
                Success::NoContent => StatusCode::NO_CONTENT.into_response(),
            })
        }
    };
    match method {
        Method::Get => get(run),
        Method::Post => post(run),
        Method::Put => put(run),
        Method::Delete => delete(run),
    }
}

/// Fold path and query params into the input object, reporting any key where
/// the body disagreed with the URL.
///
/// Fill-missing-only, and a disagreement is never silently resolved. The path
/// is the id of record: `PUT /api/agents/deploy` with a body naming `renamed`
/// is a rename attempt, and answering it by quietly preferring either name
/// would make one of them a lie. The caller turns a conflict into a 422.
///
/// This is why the rule lives here rather than in each service. It is a
/// statement about URLs, so it belongs to the surface that has them — a tool
/// call carries no path and cannot produce a conflict at all.
pub(crate) fn merge_params(
    input: &mut Value,
    params: impl Iterator<Item = (String, String)>,
) -> Vec<String> {
    if !input.is_object() {
        *input = Value::Object(serde_json::Map::new());
    }
    let Some(object) = input.as_object_mut() else {
        return Vec::new();
    };
    let mut conflicts = Vec::new();
    for (key, value) in params {
        match object.get(&key) {
            Some(existing) if existing.as_str() != Some(value.as_str()) => {
                conflicts.push(key);
            }
            Some(_) => {}
            None => {
                object.insert(key, Value::String(value));
            }
        }
    }
    conflicts
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
    use serde_json::json;

    #[test]
    fn a_param_fills_a_missing_key() {
        let mut input = json!({});
        let _ = merge_params(
            &mut input,
            [("name".to_string(), "deploy".to_string())].into_iter(),
        );
        assert_eq!(input["name"], "deploy");
    }

    #[test]
    fn a_param_that_disagrees_with_the_body_is_reported_not_resolved() {
        // Silently preferring either name would turn a rejected rename into a
        // successful one, or into a no-op the caller thinks succeeded.
        let mut input = json!({"name": "renamed"});
        let conflicts = merge_params(
            &mut input,
            [("name".to_string(), "original".to_string())].into_iter(),
        );
        assert_eq!(conflicts, ["name"]);
        assert_eq!(input["name"], "renamed", "the body is left as it was");
    }

    #[test]
    fn a_param_that_agrees_with_the_body_is_not_a_conflict() {
        let mut input = json!({"name": "deploy"});
        let conflicts = merge_params(
            &mut input,
            [("name".to_string(), "deploy".to_string())].into_iter(),
        );
        assert!(conflicts.is_empty());
    }

    #[test]
    fn a_non_object_body_becomes_an_object() {
        let mut input = json!(null);
        let _ = merge_params(
            &mut input,
            [("name".to_string(), "deploy".to_string())].into_iter(),
        );
        assert_eq!(input, json!({"name": "deploy"}));
    }

    use crate::control::{ControlError, Expose, Method, NameRef, NoInput, Operation, op};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use tower::ServiceExt;

    /// Two operations on one path plus one that is not routed at all.
    fn fixture() -> Vec<Operation> {
        vec![
            op(
                "list",
                Method::Get,
                "/api/ghosts",
                "List ghosts.",
                Expose::ApiAndTool,
                |_s, _i: NoInput| async move { Ok::<Value, ControlError>(json!(["casper"])) },
            ),
            op(
                "create",
                Method::Post,
                "/api/ghosts",
                "Summon a ghost.",
                Expose::ApiAndTool,
                |_s, i: NameRef| async move { Ok::<Value, ControlError>(json!({"name": i.name})) },
            ),
            op(
                "peek",
                Method::Get,
                "/api/ghosts/hidden",
                "Never routed.",
                Expose::ToolOnly,
                |_s, _i: NoInput| async move { Ok::<Value, ControlError>(json!({})) },
            ),
        ]
    }

    async fn call(request: Request<Body>) -> (StatusCode, Value) {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::testing::state(dir.path()).build().await;
        let app = router(&fixture()).with_state(state.state.clone());
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        (status, value)
    }

    fn request(method: &str, uri: &str, body: &'static str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            // `Scope` reads the principal the auth layer would have put here;
            // the fold is mounted below that layer, so the test supplies it.
            .extension(crate::auth::Principal::Anonymous)
            .body(Body::from(body))
            .unwrap()
    }

    #[tokio::test]
    async fn both_methods_on_one_path_are_mounted() {
        let (status, body) = call(request("GET", "/api/ghosts", "")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!(["casper"]));

        let (status, body) = call(request("POST", "/api/ghosts", r#"{"name":"slimer"}"#)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"name": "slimer"}));
    }

    #[tokio::test]
    async fn a_tool_only_operation_is_not_mounted() {
        let (status, _) = call(request("GET", "/api/ghosts/hidden", "")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_query_param_reaches_the_input() {
        let (status, body) = call(request("POST", "/api/ghosts?name=query", "")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"name": "query"}));
    }

    #[tokio::test]
    async fn a_malformed_body_is_a_422_not_a_500() {
        let (status, _) = call(request("POST", "/api/ghosts", "{not json")).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn an_input_that_does_not_deserialize_is_a_422() {
        let (status, _) = call(request("POST", "/api/ghosts", "{}")).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    /// Unused in the fixture but proves the signature compiles for the real
    /// resources, which all take `Arc<UserServices>`.
    #[allow(dead_code)]
    fn typed_services(_: Arc<crate::users::UserServices>) {}
}
