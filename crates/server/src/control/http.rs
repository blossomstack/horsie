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

/// The routes that are deliberately *not* operations, with why.
///
/// Every route in `http/mod.rs` is either folded out of the operation table or
/// listed here. `every_route_is_classified` reads that file and fails if a
/// third possibility appears — which is the whole mechanism: a route added
/// without a decision is a route the control plane silently cannot see.
///
/// Paths are written exactly as `http/mod.rs` writes them, which means two
/// shapes. A route inside `scoped()` is relative to `/api/p/{project}`, because
/// that is the prefix the router nests it under and an operation has never had
/// a scope in its path. Everything else is absolute.
pub const NON_OPERATIONS: &[(&str, &str)] = &[
    // Liveness, before anything is resolved.
    ("/api/health", "no account, no state, nothing to manage"),
    // A table compiled into this binary. There is nothing here to manage: a
    // control tool that listed the tools would be answering a question about
    // the build, not about this account.
    ("/api/tools", "static catalogue, not a resource"),
    // Credentials. The control plane never issues or spends one.
    ("/api/auth/status", "credential surface"),
    ("/api/auth/login", "credential surface"),
    ("/api/auth/logout", "credential surface"),
    ("/api/auth/password", "credential surface"),
    ("/api/device/auth/code", "credential surface"),
    ("/api/device/auth/token", "credential surface"),
    ("/api/device/auth/refresh", "credential surface"),
    ("/api/device/approve", "credential surface"),
    ("/api/device/deny", "credential surface"),
    ("/api/device/tokens", "credential surface"),
    ("/api/device/tokens/{id}", "credential surface"),
    // Third-party sign-in: browser redirects and polling, not JSON operations.
    ("/github/status", "oauth flow"),
    ("/github/auth", "oauth flow"),
    ("/github/callback", "oauth redirect"),
    (
        "/github/app-config",
        "oauth configuration, carries a client secret",
    ),
    ("/github/disconnect", "oauth flow"),
    (
        "/github/repos",
        "reads a third-party API with the user's token",
    ),
    (
        "/github/repos/branches",
        "reads a third-party API with the user's token",
    ),
    ("/config/model-providers/{name}/chatgpt", "login flow"),
    ("/config/model-providers/{name}/chatgpt/login", "login flow"),
    ("/config/model-providers/{name}/chatgpt/poll", "login flow"),
    (
        "/mcp/servers/{name}/connect",
        "builds a redirect_uri from the request's own Host",
    ),
    ("/mcp/servers/{name}/oauth/callback", "oauth redirect"),
    // Streams and sockets: no request/response body to be an operation over.
    ("/events", "server-sent events"),
    ("/vendor/connect", "websocket upgrade"),
    ("/api/runtime/connect", "websocket upgrade"),
    (
        "/api/runtime/github-credential",
        "the runtime's own credential channel",
    ),
    // Bytes, not JSON.
    ("/api/plugin-bundles/{name}/{version}", "serves a file"),
    // Session traffic that is talking rather than management. A tool
    // that could message a session could talk to itself.
    (
        "/sessions",
        "POST creates a session; the GET half is an operation",
    ),
    (
        "/sessions/{id}/messages",
        "POST sends a message, GET is the stream",
    ),
    ("/sessions/{id}/answers", "answers an ask, mid-session"),
    ("/sessions/{id}/annotations", "session metadata"),
    (
        "/sessions/{id}/agents/{agent_id}",
        "reads or deletes one agent of a live session",
    ),
    (
        "/sessions/{id}/workflow",
        "a run projected onto its graph, hung off the session",
    ),
    (
        "/sessions/{id}/workflow/retry",
        "retries a step of a live run",
    ),
    // Individual memories. Agents reach these through `MemoryToolbox`, which
    // scopes them to the session's declared spaces; a second, unscoped door
    // would defeat that.
    (
        "/memories",
        "MemoryToolbox owns this, scoped to the session's spaces",
    ),
    (
        "/memories/{id}",
        "MemoryToolbox owns this, scoped to the session's spaces",
    ),
    // Deployment-wide catalogue, not an account's to manage.
    ("/admin/model-cards", "deployment administration"),
    ("/admin/model-cards/{model_id}", "deployment administration"),
    // The 404 catch-all.
    (
        "/api/{*rest}",
        "unmatched paths answer JSON rather than the SPA shell",
    ),
    // An account's projects, which is what puts an id in every other route's
    // `{project}` segment. Not an operation on purpose: an operation runs
    // *inside* a project, and one that could create or delete them would be
    // reaching outside the scope everything else here exists to hold.
    (
        "/api/projects",
        "an account's scopes, not a project's contents",
    ),
    (
        "/api/projects/{id}",
        "an account's scopes, not a project's contents",
    ),
];

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
            // `{project}` comes from the prefix the router nests these under,
            // not from the operation's own path, so it is dropped rather than
            // merged: an operation's input describes what to do *inside* a
            // scope that is already resolved, and every one of them would
            // otherwise reject an unknown field it never declared.
            let path_params: Vec<(String, String)> = params
                .iter()
                .filter(|(key, _)| *key != crate::http::PROJECT_PARAM)
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

    /// Every route under `/api` is an operation or a named exception.
    ///
    /// Read from the source rather than from the router because an
    /// `axum::Router` cannot be enumerated once built. That makes this a
    /// text scan, which is coarse — but it is the only thing that fails when
    /// somebody adds a route and forgets the control plane exists, and that is
    /// the failure this design is for.
    #[test]
    fn every_route_is_classified() {
        let source = include_str!("../http/mod.rs");
        // The test module below re-uses `/api/...` literals as request URLs.
        let routes = source.split("mod tests {").next().unwrap_or(source);

        let operation_paths: std::collections::BTreeSet<&str> = crate::control::operations()
            .iter()
            .filter(|o| o.expose != crate::control::Expose::ToolOnly)
            .map(|o| o.path)
            .collect();
        let excused: std::collections::BTreeSet<&str> =
            NON_OPERATIONS.iter().map(|(path, _)| *path).collect();

        let mut unclassified = Vec::new();
        let mut rest = routes;
        while let Some(at) = rest.find(".route(") {
            rest = &rest[at + ".route(".len()..];
            let Some(open) = rest.find('"') else { break };
            let Some(close) = rest[open + 1..].find('"') else {
                break;
            };
            let path = &rest[open + 1..open + 1 + close];
            if !path.starts_with("/api") {
                continue;
            }
            if !operation_paths.contains(path) && !excused.contains(path) {
                unclassified.push(path);
            }
        }
        assert!(
            unclassified.is_empty(),
            "these routes are neither operations nor named in NON_OPERATIONS: {unclassified:?}"
        );
    }

    /// One path is split by method, and only one.
    ///
    /// `GET /api/sessions` lists — an operation — while `POST` creates a
    /// session, which is not. That is deliberate, so it is named here rather
    /// than excused in general: a second split is a decision somebody has to
    /// make on purpose.
    ///
    /// `/api/sessions/{id}/messages` looks like it should be here and is not.
    /// Its tool-side half (`sessions.read`) is [`Expose::ToolOnly`], so it is
    /// never mounted and the route is wholly a non-operation.
    #[test]
    fn only_these_paths_are_split_by_method() {
        let operation_paths: std::collections::BTreeSet<&str> = crate::control::operations()
            .iter()
            .filter(|o| o.expose != crate::control::Expose::ToolOnly)
            .map(|o| o.path)
            .collect();
        let mut split: Vec<&str> = NON_OPERATIONS
            .iter()
            .map(|(path, _)| *path)
            .filter(|path| operation_paths.contains(path))
            .collect();
        split.sort_unstable();
        assert_eq!(split, ["/sessions"]);
    }

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
                "/ghosts",
                "List ghosts.",
                Expose::ApiAndTool,
                |_s, _i: NoInput| async move { Ok::<Value, ControlError>(json!(["casper"])) },
            ),
            op(
                "create",
                Method::Post,
                "/ghosts",
                "Summon a ghost.",
                Expose::ApiAndTool,
                |_s, i: NameRef| async move { Ok::<Value, ControlError>(json!({"name": i.name})) },
            ),
            op(
                "peek",
                Method::Get,
                "/ghosts/hidden",
                "Never routed.",
                Expose::ToolOnly,
                |_s, _i: NoInput| async move { Ok::<Value, ControlError>(json!({})) },
            ),
        ]
    }

    /// Mounted under the project prefix, exactly as `http::app` mounts it —
    /// the fold is nested there, and `Scope` reads the segment, so a fixture
    /// router mounted flat would 500 on every request for the right reason.
    async fn call(method: &str, path: &str, body: &'static str) -> (StatusCode, Value) {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::testing::state(dir.path()).build().await;
        let app = axum::Router::new()
            .nest("/api/p/{project}", router(&fixture()))
            .with_state(state.state.clone());
        let response = app
            .oneshot(request(method, &state.url(path), body))
            .await
            .unwrap();
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
        let (status, body) = call("GET", "/ghosts", "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!(["casper"]));

        let (status, body) = call("POST", "/ghosts", r#"{"name":"slimer"}"#).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"name": "slimer"}));
    }

    #[tokio::test]
    async fn a_tool_only_operation_is_not_mounted() {
        let (status, _) = call("GET", "/ghosts/hidden", "").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_query_param_reaches_the_input() {
        let (status, body) = call("POST", "/ghosts?name=query", "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"name": "query"}));
    }

    #[tokio::test]
    async fn a_malformed_body_is_a_422_not_a_500() {
        let (status, _) = call("POST", "/ghosts", "{not json").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn an_input_that_does_not_deserialize_is_a_422() {
        let (status, _) = call("POST", "/ghosts", "{}").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    /// Unused in the fixture but proves the signature compiles for the real
    /// resources, which all take `Arc<ProjectServices>`.
    #[allow(dead_code)]
    fn typed_services(_: Arc<crate::projects::ProjectServices>) {}
}
