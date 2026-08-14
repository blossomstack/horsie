# Control Plane Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let an agent manage the horsie server it runs on, through tools that share one declaration with the HTTP routes.

**Architecture:** A table of `Operation` values in `crates/server/src/control/` is the single declaration of every JSON management operation. The axum router is folded out of that table, and so is a per-resource agent toolbox. Both call one async fn per operation over `Arc<UserServices>` — no in-process HTTP, no duplicated handler logic.

**Tech Stack:** Rust, axum 0.8, schemars 1.2, sqlx (`sqlx::Any`), fluorite codegen, tokio.

**Spec:** `docs/superpowers/specs/2026-08-14-control-plane-design.md`

## Scope of this plan

This plan covers **PR 1 and PR 2** of the spec's four-PR staging. Together they deliver a working control-plane agent over agents, workflows, routines and environments. PR 3 (remaining resources, `sessions.read`, classification test) and PR 4 (CLI generation) get their own plans once this lands — they are additive over the same table and gate nothing here.

## Global Constraints

- Workspace lints deny `unwrap_used`, `expect_used`, `panic`, `wildcard_enum_match_arm` in production code. Test modules opt out with the `#![cfg_attr(test, allow(...))]` pattern already used across the crate.
- Pre-PR verification is exactly: `cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt --check`, `cargo test --workspace`. CI adds `-D warnings`; a local clippy without it exits 0 on code CI rejects.
- Iterate with `cargo test -p horsie-server --lib`; run the full workspace suite once before pushing. `-p horsie-server` alone is a false green — the e2e tests in `crates/tests` hit these routes.
- Protocol types live in `crates/models/fluorite/*.fl` and need `make types` after editing. Never use fluorite for persisted structures.
- Unit tests live in the same file under `#[cfg(test)] mod tests`. Full-stack tests go in `tests/` at the crate root.
- Every migration is written twice, identically: `crates/server/migrations/sqlite/` and `crates/server/migrations/postgres/`. Booleans are stored as `INTEGER` because the `sqlx::Any` driver cannot decode SQLite's `BOOLEAN`.
- Never list an AI tool as author or co-author on any commit.

## File Structure

**PR 1 — created:**

| File | Responsibility |
|---|---|
| `crates/server/src/control/mod.rs` | `Operation`, `Expose`, `Method`, `ControlError`, the `op()` constructor, `operations()`. |
| `crates/server/src/control/http.rs` | The router fold, the generic handler, param merging, `NON_OPERATIONS`. |
| `crates/server/src/control/agents.rs` | The agents resource: six operations plus the two bodies lifted out of axum. |
| `crates/server/src/control/workflows.rs` | The workflows resource, including the lifted `start_run`. |
| `crates/server/src/control/routines.rs` | The routines resource. |
| `crates/server/src/control/environments.rs` | The environments resource. |

**PR 1 — modified:** `crates/server/src/lib.rs` (add `pub mod control;`), `crates/server/src/http/mod.rs` (drop four resources' routes, merge the fold), and `crates/server/src/http/{agents,workflows,routines,environments}.rs` shrink to whatever is not an operation (`workflows.rs` keeps `get_run_graph` and its projection helpers for now).

**PR 2 — created:** `crates/server/src/control/toolbox.rs`, `crates/server/migrations/{sqlite,postgres}/0037_agent_control_plane.sql`.

**PR 2 — modified:** `crates/models/fluorite/agents.fl`, `crates/models/fluorite/session.fl`, `crates/server/src/agents/{store,service}.rs`, `crates/server/src/sessions/session_actor/context.rs`, `crates/server/src/http/agents.rs` (the invoke path passes the new flag), `crates/server/src/workflows/service.rs:329` and `crates/server/src/runtime_manager.rs:531` (both construct `AgentSettings` literals and will not compile without the new field), and the web UI agent form.

---

## Task 1: `ControlError` and its conversions

**Files:**
- Create: `crates/server/src/control/mod.rs`
- Modify: `crates/server/src/lib.rs`

**Interfaces:**
- Produces: `ControlError` with variants `NotFound(String)`, `Conflict { code: String, message: String }`, `Invalid(String)`, `Internal(String)`; `impl From<ControlError> for crate::http::error::Api`; `impl From<ControlError> for horsie_agentcore::ToolCallError`; `From<AgentError>`, `From<WorkflowError>`, `From<RoutineError>`, `From<EnvironmentError>` for `ControlError`.

All four service errors are already the same four-variant shape (`agents/service.rs:19`, `workflows/service.rs:18`, `routines/service.rs:25`, `environments/service.rs:18`), so the conversions come from one macro.

`ControlError::Conflict` carries a `code` because `Api::conflict` takes one and the service errors do not: `http/agents.rs:25` supplies `"duplicate"`, and `delete_agent` supplies `"agent_in_use"`. The macro defaults to `"duplicate"`; call sites that need another build the variant directly.

- [ ] **Step 1: Write the failing test**

In `crates/server/src/control/mod.rs`:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::wildcard_enum_match_arm)]
mod tests {
    use super::*;

    #[test]
    fn service_errors_keep_their_status() {
        let api: crate::http::error::Api =
            ControlError::from(crate::agents::AgentError::NotFound("nope".into())).into();
        assert_eq!(api.0, axum::http::StatusCode::NOT_FOUND);

        let api: crate::http::error::Api =
            ControlError::from(crate::workflows::WorkflowError::Invalid("bad".into())).into();
        assert_eq!(api.0, axum::http::StatusCode::UNPROCESSABLE_ENTITY);

        let api: crate::http::error::Api = ControlError::Conflict {
            code: "agent_in_use".into(),
            message: "still used".into(),
        }
        .into();
        assert_eq!(api.0, axum::http::StatusCode::CONFLICT);
        assert_eq!(api.1.code, "agent_in_use");
    }

    #[test]
    fn invalid_becomes_invalid_input_for_a_tool() {
        let err: horsie_agentcore::ToolCallError = ControlError::Invalid("bad".into()).into();
        assert!(matches!(err, horsie_agentcore::ToolCallError::InvalidInput(_)));
    }
}
```

- [ ] **Step 2: Run the test and watch it fail**

Run: `cargo test -p horsie-server --lib control::`
Expected: FAIL — `unresolved module or unlinked crate 'control'`.

- [ ] **Step 3: Write the module**

In `crates/server/src/control/mod.rs`, above the test module:

```rust
//! The control plane: one declaration per management operation, from which
//! both the HTTP route and the agent toolbox are derived.
//!
//! A route that is not an operation cannot be mounted through the fold in
//! [`http`] — see `NON_OPERATIONS` for the named exceptions.

use crate::http::error::Api;
use horsie_agentcore::ToolCallError;

pub mod http;

/// The shared failure vocabulary. Each surface renders it its own way: HTTP as
/// a status plus an `ApiError` body, a tool as a `ToolCallError` the model reads
/// and retries against.
#[derive(Debug)]
pub enum ControlError {
    NotFound(String),
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
            // The model can fix these by calling again with different input.
            ControlError::NotFound(m) | ControlError::Invalid(m) => Self::InvalidInput(m),
            ControlError::Conflict { message, .. } => Self::InvalidInput(message),
            // It cannot fix ours.
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
                match e {
                    <$ty>::NotFound(m) => Self::NotFound(m),
                    <$ty>::Conflict(m) => Self::Conflict { code: "duplicate".to_string(), message: m },
                    <$ty>::Invalid(m) => Self::Invalid(m),
                    <$ty>::Internal(m) => Self::Internal(m),
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
```

Create an empty `crates/server/src/control/http.rs` containing only `//! Router fold. Filled in by Task 4.` so the module resolves.

Add to `crates/server/src/lib.rs`, in alphabetical position among the existing `pub mod` lines:

```rust
pub mod control;
```

- [ ] **Step 4: Run the test and watch it pass**

Run: `cargo test -p horsie-server --lib control::`
Expected: PASS, 2 tests.

If the `Api` fields are private to `http::error`, make the tuple fields `pub` — they are already declared `pub struct Api(pub StatusCode, pub ApiError)` at `http/error.rs:8`, so no change should be needed.

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/control/ crates/server/src/lib.rs
git commit -m "feat(control): the shared error vocabulary"
```

---

## Task 2: `Operation` and the `op()` constructor

**Files:**
- Modify: `crates/server/src/control/mod.rs`

**Interfaces:**
- Consumes: `ControlError` from Task 1.
- Produces: `pub enum Method { Get, Post, Put, Delete }`; `pub enum Expose { Api, ApiAndTool, ToolOnly }`; `pub struct Operation` with public fields `resource: &'static str`, `action: &'static str`, `method: Method`, `path: &'static str`, `summary: &'static str`, `expose: Expose`, `schema: serde_json::Value`, and `pub async fn run(&self, services: Arc<UserServices>, input: Value) -> Result<Value, ControlError>`; `pub fn op<I, O, F, Fut>(...) -> Operation`; `pub struct NoInput;` and `pub struct NameRef { pub name: String }`.

`NoInput` and `NameRef` are hand-written here rather than in fluorite. They carry no information the path template does not already state, and putting them in `.fl` would generate TypeScript no client uses.

**Schema generation must inline subschemas.** schemars emits `$ref` into `$defs` for nested types by default; a tool `input_schema` containing `$defs` is not reliably understood by every provider. The generator below turns that off.

- [ ] **Step 1: Write the failing test**

Append to the `mod tests` block in `control/mod.rs`:

```rust
#[derive(serde::Deserialize, schemars::JsonSchema)]
struct Greeting {
    /// Who to greet.
    who: String,
}

#[tokio::test]
async fn op_derives_its_schema_and_runs_its_fn() {
    let operation = op(
        "greetings", "say", Method::Post, "/api/greetings", "Say hello.", Expose::ApiAndTool,
        |_services, input: Greeting| async move { Ok(format!("hello {}", input.who)) },
    );

    assert_eq!(operation.resource, "greetings");
    assert_eq!(operation.schema["properties"]["who"]["type"], "string");
    assert_eq!(operation.schema["required"][0], "who");
    assert!(operation.schema.get("$defs").is_none(), "subschemas must be inlined");
}

#[tokio::test]
async fn op_rejects_input_that_does_not_deserialize() {
    let state = crate::testing::TestStateBuilder::default().build().await;
    let operation = op(
        "greetings", "say", Method::Post, "/api/greetings", "Say hello.", Expose::ApiAndTool,
        |_services, input: Greeting| async move { Ok(format!("hello {}", input.who)) },
    );

    let out = operation
        .run(state.services().await, serde_json::json!({"who": "world"}))
        .await
        .unwrap();
    assert_eq!(out, serde_json::json!("hello world"));

    let err = operation
        .run(state.services().await, serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(matches!(err, ControlError::Invalid(_)));
}
```

- [ ] **Step 2: Run the tests and watch them fail**

Run: `cargo test -p horsie-server --lib control::`
Expected: FAIL — `cannot find function 'op' in this scope`.

- [ ] **Step 3: Write the implementation**

Add to `control/mod.rs`:

```rust
use crate::users::UserServices;
use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method { Get, Post, Put, Delete }

/// Which surfaces an operation reaches.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Expose {
    /// Route only.
    Api,
    /// Route and tool. The common case.
    ApiAndTool,
    /// Tool only, with no route of its own. See the spec's note on
    /// `sessions.read`; nothing in this plan uses it yet.
    ToolOnly,
}

type Run = Arc<
    dyn Fn(Arc<UserServices>, Value) -> BoxFuture<'static, Result<Value, ControlError>>
        + Send
        + Sync,
>;

#[derive(Clone)]
pub struct Operation {
    pub resource: &'static str,
    pub action: &'static str,
    pub method: Method,
    /// axum path template. Every `{param}` must also be a field of the input
    /// type — the HTTP adapter merges path and query params into the input
    /// object, so a tool and a route see one identical shape.
    pub path: &'static str,
    /// Written for the model, and the OpenAPI summary if we ever want one.
    pub summary: &'static str,
    pub expose: Expose,
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
}

/// Every operation is declared through this. `f` is the whole implementation;
/// the HTTP handler is a fold over the table, not a second copy.
#[allow(clippy::too_many_arguments)]
pub fn op<I, O, F, Fut>(
    resource: &'static str,
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
        resource,
        action,
        method,
        path,
        summary,
        expose,
        schema: schema_for::<I>(),
        run: Arc::new(move |services, raw| {
            let f = f.clone();
            Box::pin(async move {
                let input: I =
                    serde_json::from_value(raw).map_err(|e| ControlError::Invalid(e.to_string()))?;
                let out = f(services, input).await?;
                serde_json::to_value(out).map_err(|e| ControlError::Internal(e.to_string()))
            })
        }),
    }
}

/// Subschemas are inlined: a tool `input_schema` carrying `$defs` and `$ref`
/// is not reliably read by every provider, and the schema is small enough that
/// inlining costs nothing.
fn schema_for<I: JsonSchema>() -> Value {
    let settings = schemars::generate::SchemaSettings::default()
        .with(|s| s.inline_subschemas = true);
    settings.into_generator().into_root_schema_for::<I>().to_value()
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
```

Add `futures` to `crates/server/Cargo.toml` dependencies if it is not already there (check first — `futures-util` may be present instead, in which case use `futures_util::future::BoxFuture`).

If `Schema::to_value()` does not exist on the pinned schemars 1.2, replace that line with:

```rust
serde_json::to_value(settings.into_generator().into_root_schema_for::<I>())
    .unwrap_or_else(|_| serde_json::json!({"type": "object"}))
```

- [ ] **Step 4: Run the tests and watch them pass**

Run: `cargo test -p horsie-server --lib control::`
Expected: PASS, 4 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/control/mod.rs crates/server/Cargo.toml
git commit -m "feat(control): the operation descriptor"
```

---

## Task 3: Param merging

**Files:**
- Modify: `crates/server/src/control/http.rs`

**Interfaces:**
- Produces: `pub(crate) fn merge_params(input: &mut Value, params: impl Iterator<Item = (String, String)>)`.

**The rule is fill-missing-only: a param never overwrites a key the body already has.** This is load-bearing, not a nicety. `AgentService::replace` (`agents/service.rs:99`), and its equivalents on the other three services, reject a body whose `name` disagrees with the path — "the path is the id of record". If the merge overwrote the body's `name`, that check would become unreachable and a client renaming a preset by PUT would silently succeed.

- [ ] **Step 1: Write the failing test**

In `crates/server/src/control/http.rs`:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::wildcard_enum_match_arm)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_param_fills_a_missing_key() {
        let mut input = json!({});
        merge_params(&mut input, [("name".to_string(), "deploy".to_string())].into_iter());
        assert_eq!(input["name"], "deploy");
    }

    #[test]
    fn a_param_never_overwrites_the_body() {
        // The service's name-immutability check depends on seeing the
        // caller's mismatched body, not a silently corrected one.
        let mut input = json!({"name": "renamed"});
        merge_params(&mut input, [("name".to_string(), "original".to_string())].into_iter());
        assert_eq!(input["name"], "renamed");
    }

    #[test]
    fn a_non_object_body_is_replaced_by_an_object() {
        let mut input = json!(null);
        merge_params(&mut input, [("name".to_string(), "deploy".to_string())].into_iter());
        assert_eq!(input, json!({"name": "deploy"}));
    }
}
```

- [ ] **Step 2: Run the tests and watch them fail**

Run: `cargo test -p horsie-server --lib control::http::`
Expected: FAIL — `cannot find function 'merge_params'`.

- [ ] **Step 3: Write the implementation**

Replace the placeholder line in `control/http.rs` with:

```rust
//! The HTTP surface of the control plane: every JSON route, folded out of the
//! operation table.

use serde_json::Value;

/// Fold path and query params into the input object.
///
/// Fill-missing-only, deliberately: a service that treats the path as the id of
/// record rejects a body whose `name` disagrees with it. Overwriting here would
/// make that check unreachable and turn a rejected rename into a silent one.
pub(crate) fn merge_params(input: &mut Value, params: impl Iterator<Item = (String, String)>) {
    if !input.is_object() {
        *input = Value::Object(serde_json::Map::new());
    }
    let Some(object) = input.as_object_mut() else {
        return;
    };
    for (key, value) in params {
        object.entry(key).or_insert_with(|| Value::String(value));
    }
}
```

- [ ] **Step 4: Run the tests and watch them pass**

Run: `cargo test -p horsie-server --lib control::http::`
Expected: PASS, 3 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/control/http.rs
git commit -m "feat(control): merge path params without overwriting the body"
```

---

## Task 4: The router fold

**Files:**
- Modify: `crates/server/src/control/http.rs`

**Interfaces:**
- Consumes: `Operation`, `Method`, `Expose` from Task 2; `merge_params` from Task 3.
- Produces: `pub fn router(operations: &[Operation]) -> axum::Router<crate::http::AppState>`.

Two operations can share a path with different methods (`GET`/`POST /api/agents`), and in PR 3 a path will be claimed by both tables. axum panics if two `.route()` calls claim one path, so the fold collects into a `path -> MethodRouter` map and mounts each path exactly once. That is unconditionally correct whether or not `Router::merge` would also have handled it.

`Expose::ToolOnly` operations are skipped here — that is what the variant means.

- [ ] **Step 1: Write the failing test**

Append to `mod tests` in `control/http.rs`:

```rust
use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

#[tokio::test]
async fn the_fold_mounts_both_methods_on_one_path() {
    let state = crate::testing::TestStateBuilder::default().build().await;
    let app = router(&crate::control::agents::operations()).with_state(state.app_state());

    let response = app
        .clone()
        .oneshot(Request::builder().uri("/api/agents").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/agents")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"x","model":"nope"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    // Reaches the handler and is rejected on its merits, not routing.
    assert_ne!(response.status(), axum::http::StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn tool_only_operations_are_not_mounted() {
    let operations = vec![crate::control::op(
        "ghosts", "peek", crate::control::Method::Get, "/api/ghosts", "Not routed.",
        crate::control::Expose::ToolOnly,
        |_s, _i: crate::control::NoInput| async move { Ok(serde_json::json!({})) },
    )];
    let state = crate::testing::TestStateBuilder::default().build().await;
    let response = router(&operations)
        .with_state(state.app_state())
        .oneshot(Request::builder().uri("/api/ghosts").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
}
```

If `TestStateBuilder` has no `app_state()` accessor, add one returning the `AppState` it already builds for `serve()` (`crates/server/src/testing.rs:109` constructs the router, so the state is in reach).

These tests require the routes to run without the auth layer, which is how `router()` is used — `require_auth` is applied by `http::router()` above the merge. `Scope` needs a `Principal` in the extensions, so the test must insert one. Add to each request builder before `.body(...)`:

```rust
.extension(crate::auth::Principal::Anonymous)
```

- [ ] **Step 2: Run the tests and watch them fail**

Run: `cargo test -p horsie-server --lib control::http::`
Expected: FAIL — `cannot find function 'router'`.

- [ ] **Step 3: Write the implementation**

Add to `control/http.rs`:

```rust
use crate::control::{Expose, Method, Operation};
use crate::http::{AppState, Scope, error::Api};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Query, RawPathParams};
use axum::routing::{MethodRouter, delete, get, post, put};
use std::collections::{BTreeMap, HashMap};

/// Mount every routed operation. Paths are collected first and mounted once,
/// because two operations may share a path with different methods and axum
/// panics when two routes claim one path.
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

fn handler_for(operation: Operation) -> MethodRouter<AppState> {
    let run = move |Scope(services): Scope,
                    params: RawPathParams,
                    Query(query): Query<HashMap<String, String>>,
                    body: Bytes| {
        let operation = operation.clone();
        async move {
            let mut input = if body.is_empty() {
                serde_json::Value::Object(serde_json::Map::new())
            } else {
                serde_json::from_slice(&body)
                    .map_err(|e| Api::unprocessable(format!("malformed JSON body: {e}")))?
            };
            let path_params: Vec<(String, String)> = params
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            merge_params(&mut input, path_params.into_iter());
            merge_params(&mut input, query.into_iter());
            operation
                .run(services, input)
                .await
                .map(Json)
                .map_err(Api::from)
        }
    };
    match operation_method {
        Method::Get => get(run),
        Method::Post => post(run),
        Method::Put => put(run),
        Method::Delete => delete(run),
    }
}
```

`operation_method` must be captured before `operation` moves into the closure — bind `let operation_method = operation.method;` as the first line of `handler_for`.

`RawPathParams` rather than `Path<HashMap<_, _>>`: the latter rejects a request on a route with no path params at all, which is most of them.

- [ ] **Step 4: Run the tests and watch them pass**

Run: `cargo test -p horsie-server --lib control::http::`
Expected: PASS, 5 tests. Task 5 provides `control::agents::operations()`; until then the first test will not compile — write Task 5's `operations()` first if you are executing strictly in order, or temporarily point that test at the inline `ghosts` operation.

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/control/http.rs
git commit -m "feat(control): fold the router out of the operation table"
```

---

## Task 5: The agents resource

**Files:**
- Create: `crates/server/src/control/agents.rs`
- Modify: `crates/server/src/http/agents.rs`, `crates/server/src/http/mod.rs`, `crates/server/src/control/mod.rs`

**Interfaces:**
- Consumes: `op`, `Operation`, `Method`, `Expose`, `NoInput`, `NameRef`, `ControlError`.
- Produces: `pub fn operations() -> Vec<Operation>` in `control::agents`; `pub fn operations() -> Vec<Operation>` in `control` returning every resource's concatenated.

The input type for `create` and `replace` is `AgentPresetInput` itself — it already carries `name` (`agents.fl`), and `AgentService::replace` checks it against the path. No wrapper type and no `serde(flatten)` is needed.

`invoke_agent`'s 80-line body moves here from `http/agents.rs:102` **verbatim**, with `Scope(state)` becoming a parameter, `Json(req)` becoming the typed input, `Api::unprocessable` becoming `ControlError::Invalid`, and `Api::not_found` becoming `ControlError::NotFound`. Do not otherwise rewrite it.

- [ ] **Step 1: Write the failing test**

In `crates/server/src/control/agents.rs`:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::wildcard_enum_match_arm)]
mod tests {
    use super::*;

    fn find(action: &str) -> Operation {
        operations().into_iter().find(|o| o.action == action).unwrap()
    }

    #[test]
    fn every_action_is_declared_once() {
        let mut actions: Vec<&str> = operations().iter().map(|o| o.action).collect();
        actions.sort_unstable();
        assert_eq!(actions, ["create", "delete", "get", "invoke", "list", "replace"]);
        assert!(operations().iter().all(|o| o.resource == "agents"));
    }

    #[tokio::test]
    async fn create_then_list_round_trips_through_the_operation() {
        let state = crate::testing::TestStateBuilder::default().build().await;
        let services = state.services().await;

        find("create")
            .run(services.clone(), serde_json::json!({"name": "deploy", "model": "test-model"}))
            .await
            .unwrap();

        let listed = find("list").run(services, serde_json::json!({})).await.unwrap();
        assert_eq!(listed[0]["name"], "deploy");
    }

    #[tokio::test]
    async fn delete_is_refused_while_a_routine_uses_the_preset() {
        // The check lives in the operation now, not in the axum handler, so it
        // must still fire when reached from a tool.
        let state = crate::testing::TestStateBuilder::default().build().await;
        let services = state.services().await;
        find("create")
            .run(services.clone(), serde_json::json!({"name": "deploy", "model": "test-model"}))
            .await
            .unwrap();
        services
            .routines
            .create(routine_input_naming("deploy"), horsie_models::now_ms())
            .await
            .unwrap();

        let err = find("delete")
            .run(services, serde_json::json!({"name": "deploy"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ControlError::Conflict { ref code, .. } if code == "agent_in_use"));
    }
}
```

`test-model` must be a model alias the test state configures. Check what `TestStateBuilder` seeds (`crates/server/src/testing.rs:121`) and use that alias; if it seeds none, call `state.insert_provider(...)` and add a model the way the existing `agents::service` tests do — copy their setup rather than inventing one. Likewise write `routine_input_naming` by copying the `RoutineInput` construction from the existing routine service tests.

- [ ] **Step 2: Run the tests and watch them fail**

Run: `cargo test -p horsie-server --lib control::agents::`
Expected: FAIL — module does not exist.

- [ ] **Step 3: Write the implementation**

```rust
//! The agents resource: agent presets, and invoking one into a session.

use crate::control::{ControlError, Expose, Method, NameRef, NoInput, Operation, op};
use crate::users::UserServices;
use horsie_models::agents::{AgentInvokeRequest, AgentInvokeResponse, AgentPresetInput, AgentView};
use std::sync::Arc;

/// `POST /api/agents/{name}/invoke` takes its slug from the path and the rest
/// from the body; the merge in `control::http` supplies `name` for the route,
/// and a tool passes it directly.
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct InvokeAgent {
    /// Slug of the preset to invoke.
    pub name: String,
    #[serde(flatten)]
    pub request: AgentInvokeRequest,
}

pub fn operations() -> Vec<Operation> {
    vec![
        op("agents", "list", Method::Get, "/api/agents",
           "Every saved agent preset.", Expose::ApiAndTool,
           |s: Arc<UserServices>, _i: NoInput| async move {
               Ok::<Vec<AgentView>, ControlError>(s.agents.list().await?)
           }),
        op("agents", "get", Method::Get, "/api/agents/{name}",
           "One agent preset by slug.", Expose::ApiAndTool,
           |s: Arc<UserServices>, i: NameRef| async move {
               Ok::<AgentView, ControlError>(s.agents.get(&i.name).await?)
           }),
        op("agents", "create", Method::Post, "/api/agents",
           "Save a new agent preset. `model` must be a configured alias — list \
            the models first if you are unsure.", Expose::ApiAndTool,
           |s: Arc<UserServices>, i: AgentPresetInput| async move {
               Ok::<AgentView, ControlError>(s.agents.create(i).await?)
           }),
        op("agents", "replace", Method::Put, "/api/agents/{name}",
           "Replace a preset wholesale. Omitted fields are reset, not kept. The \
            name is immutable.", Expose::ApiAndTool,
           |s: Arc<UserServices>, i: AgentPresetInput| async move {
               let name = i.name.clone();
               Ok::<AgentView, ControlError>(s.agents.replace(&name, i).await?)
           }),
        op("agents", "delete", Method::Delete, "/api/agents/{name}",
           "Delete a preset. Refused while a routine still names it.", Expose::ApiAndTool,
           |s: Arc<UserServices>, i: NameRef| async move { delete(&s, &i.name).await }),
        op("agents", "invoke", Method::Post, "/api/agents/{name}/invoke",
           "Create a session from a preset and queue its first message. Returns \
            as soon as both are accepted; the turn runs in the background.",
           Expose::ApiAndTool,
           |s: Arc<UserServices>, i: InvokeAgent| async move { invoke(&s, i).await }),
    ]
}

/// The routine check was in the axum handler; both surfaces need it.
async fn delete(services: &UserServices, name: &str) -> Result<(), ControlError> {
    let used_by = services
        .routines
        .using_agent(name)
        .await
        .map_err(|e| ControlError::Internal(e.to_string()))?;
    if !used_by.is_empty() {
        return Err(ControlError::Conflict {
            code: "agent_in_use".to_string(),
            message: format!("routines still use this agent: {}", used_by.join(", ")),
        });
    }
    services.agents.delete(name).await?;
    Ok(())
}

async fn invoke(
    services: &UserServices,
    input: InvokeAgent,
) -> Result<AgentInvokeResponse, ControlError> {
    // The body of http/agents.rs:102, moved. Fill this in during this step —
    // do not leave a `todo!()`, which the workspace's `panic` lint rejects.
}
```

Cut the real `invoke_agent` body out of `http/agents.rs` and paste it here in this same step. It is roughly 80 lines: the preset lookup, the empty-message check, the re-validation of the model against the config view, the `WireAgentSettings` literal, `build_session_spec`, the connected-vendor check, and the two `handlers::ask` calls. Change only these mechanical things — `state` becomes `services`, `req` becomes `input.request`, `name` becomes `input.name`, `Api::unprocessable(m)` becomes `ControlError::Invalid(m)`, `Api::not_found(m)` becomes `ControlError::NotFound(m)`, and `Api::conflict(c, m)` becomes `ControlError::Conflict { code: c.to_string(), message: m }`. `handlers::ask` takes `&UserServices` already (`http/handlers.rs:46`), so it needs no change. Rewriting the logic while moving it is how a verbatim move turns into a regression nobody reviews. `SpecError` currently converts into `Api` (`http/error.rs:46`); add the matching `impl From<crate::sessions::builder::SpecError> for ControlError` alongside it in `control/mod.rs`, mapping `Invalid` to `Invalid` and `Internal` to `Internal`.

Then in `control/mod.rs`:

```rust
pub mod agents;

/// The whole control plane. A new resource is one line.
pub fn operations() -> Vec<Operation> {
    agents::operations()
}
```

And in `crates/server/src/http/mod.rs`, delete the five agent routes and merge the fold:

```rust
.merge(crate::control::http::router(&crate::control::operations()))
```

`http/agents.rs` is left holding only what the fold does not cover; for agents that is nothing, so delete the file and its `mod agents;` line.

- [ ] **Step 4: Run the tests and watch them pass**

Run: `cargo test -p horsie-server --lib control::` then `cargo test -p horsie-server`
Expected: PASS. The existing HTTP-level agent tests must pass unchanged — they are the proof the fold serves the same routes.

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/control/ crates/server/src/http/
git commit -m "feat(control): agents as operations"
```

---

## Task 6: The workflows resource

**Files:**
- Create: `crates/server/src/control/workflows.rs`
- Modify: `crates/server/src/http/workflows.rs`, `crates/server/src/http/mod.rs`, `crates/server/src/control/mod.rs`

**Interfaces:**
- Produces: `control::workflows::operations()` with actions `list`, `get`, `create`, `replace`, `delete`, `run`, `retry-step`.

`WorkflowService::create` and `replace` take a `now_secs: u64` second argument; the operation supplies it exactly as `http/workflows.rs:32`'s `now_secs()` helper does today — move that helper into `control/workflows.rs`.

`get_run_graph` (`http/workflows.rs:267`) and its projection helpers `project_run` and `step_run_view` stay in `http/workflows.rs` for now: the graph projection is a read shaped for the web UI, and PR 3 decides whether it becomes an operation. Its route stays hand-mounted and is one of the entries PR 3's classification test will account for.

- [ ] **Step 1: Write the failing test**

In `crates/server/src/control/workflows.rs`, mirroring Task 5's shape:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::wildcard_enum_match_arm)]
mod tests {
    use super::*;

    fn find(action: &str) -> Operation {
        operations().into_iter().find(|o| o.action == action).unwrap()
    }

    #[test]
    fn every_action_is_declared_once() {
        let mut actions: Vec<&str> = operations().iter().map(|o| o.action).collect();
        actions.sort_unstable();
        assert_eq!(
            actions,
            ["create", "delete", "get", "list", "replace", "retry-step", "run"]
        );
    }

    #[tokio::test]
    async fn replace_rejects_a_renamed_body() {
        // The path is the id of record; merge_params must not have papered
        // over the mismatch.
        let state = crate::testing::TestStateBuilder::default().build().await;
        let services = state.services().await;
        find("create").run(services.clone(), workflow_json("nightly")).await.unwrap();

        let mut renamed = workflow_json("nightly");
        renamed["name"] = serde_json::json!("renamed");
        let err = find("replace").run(services, renamed).await.unwrap_err();
        assert!(matches!(err, ControlError::Invalid(_)));
    }
}
```

Write `workflow_json` by copying a valid `WorkflowInput` from the existing `workflows::service` tests and serialising it — do not invent the step-graph shape.

- [ ] **Step 2: Run the tests and watch them fail**

Run: `cargo test -p horsie-server --lib control::workflows::`
Expected: FAIL — module does not exist.

- [ ] **Step 3: Write the implementation**

Follow Task 5's structure exactly. `run` and `retry-step` take `WorkflowRunRequest` and `WorkflowRetryRequest` wrapped with the path slug the same way `InvokeAgent` wraps `AgentInvokeRequest`. Move `start_run`'s body (`http/workflows.rs:112-266`) verbatim into a private `async fn run_workflow(services: &UserServices, input: RunWorkflow) -> Result<WorkflowRunResponse, ControlError>`, converting `Api::*` constructors to `ControlError::*` and nothing else.

Register in `control/mod.rs`:

```rust
pub mod workflows;

pub fn operations() -> Vec<Operation> {
    [agents::operations(), workflows::operations()].concat()
}
```

Remove the migrated routes from `http/mod.rs`, keeping `/api/sessions/{id}/workflow`.

- [ ] **Step 4: Run the tests and watch them pass**

Run: `cargo test -p horsie-server`
Expected: PASS, existing workflow HTTP tests unchanged.

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/control/ crates/server/src/http/
git commit -m "feat(control): workflows as operations"
```

---

## Task 7: The routines resource

**Files:**
- Create: `crates/server/src/control/routines.rs`
- Modify: `crates/server/src/http/routines.rs`, `crates/server/src/http/mod.rs`, `crates/server/src/control/mod.rs`

**Interfaces:**
- Produces: `control::routines::operations()` with `list`, `get`, `create`, `replace`, `delete`, `run`.

`RoutineService::create`/`replace` take `now_ms: u64` — supply `horsie_models::now_ms()`, as `http/routines.rs` does.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::wildcard_enum_match_arm)]
mod tests {
    use super::*;

    #[test]
    fn every_action_is_declared_once() {
        let mut actions: Vec<&str> = operations().iter().map(|o| o.action).collect();
        actions.sort_unstable();
        assert_eq!(actions, ["create", "delete", "get", "list", "replace", "run"]);
        assert!(operations().iter().all(|o| o.resource == "routines"));
    }

    #[tokio::test]
    async fn create_rejects_an_unknown_agent() {
        let state = crate::testing::TestStateBuilder::default().build().await;
        let operation = operations().into_iter().find(|o| o.action == "create").unwrap();
        let err = operation
            .run(state.services().await, routine_json("nightly", "no-such-preset"))
            .await
            .unwrap_err();
        assert!(matches!(err, ControlError::Invalid(_)));
    }
}
```

Copy `routine_json` from the existing routine service tests.

- [ ] **Step 2: Run the tests and watch them fail**

Run: `cargo test -p horsie-server --lib control::routines::`
Expected: FAIL — module does not exist.

- [ ] **Step 3: Write the implementation**

Same structure as Task 5. `run_routine`'s handler body (`http/routines.rs:105`) moves in verbatim.

- [ ] **Step 4: Run the tests and watch them pass**

Run: `cargo test -p horsie-server`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/control/ crates/server/src/http/
git commit -m "feat(control): routines as operations"
```

---

## Task 8: The environments resource, and PR 1 verification

**Files:**
- Create: `crates/server/src/control/environments.rs`
- Modify: `crates/server/src/http/environments.rs`, `crates/server/src/http/mod.rs`, `crates/server/src/control/mod.rs`

**Interfaces:**
- Produces: `control::environments::operations()` with `list`, `get`, `create`, `replace`, `delete`. `EnvironmentService::create`/`replace` take no timestamp.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::wildcard_enum_match_arm)]
mod tests {
    use super::*;

    #[test]
    fn every_action_is_declared_once() {
        let mut actions: Vec<&str> = operations().iter().map(|o| o.action).collect();
        actions.sort_unstable();
        assert_eq!(actions, ["create", "delete", "get", "list", "replace"]);
        assert!(operations().iter().all(|o| o.resource == "environments"));
    }
}
```

- [ ] **Step 2: Run the test and watch it fail**

Run: `cargo test -p horsie-server --lib control::environments::`
Expected: FAIL — module does not exist.

- [ ] **Step 3: Write the implementation**

Same structure as Task 5, and finish `control::operations()`:

```rust
pub fn operations() -> Vec<Operation> {
    [
        agents::operations(),
        workflows::operations(),
        routines::operations(),
        environments::operations(),
    ]
    .concat()
}
```

- [ ] **Step 4: Verify the whole workspace**

Run each and fix anything red before continuing:

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace
```

The e2e suite in `crates/tests` exercises these routes; `-p horsie-server` passing is not sufficient evidence. On macOS the Playwright-adjacent setup needs `TMPDIR=/tmp` — prefix the command if the web e2e tests are in the run.

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/
git commit -m "feat(control): environments as operations"
```

PR 1 is complete at this point and can be pushed and opened while PR 2 proceeds.

---

## Task 9: The `control_plane` flag through the stack

**Files:**
- Create: `crates/server/migrations/sqlite/0037_agent_control_plane.sql`, `crates/server/migrations/postgres/0037_agent_control_plane.sql`
- Modify: `crates/models/fluorite/agents.fl`, `crates/models/fluorite/session.fl`, `crates/server/src/agents/store.rs`, `crates/server/src/agents/service.rs`, `crates/server/src/control/agents.rs`, `crates/server/src/workflows/service.rs:329`, `crates/server/src/runtime_manager.rs:531`

**Interfaces:**
- Produces: `AgentView.control_plane: bool`, `AgentPresetInput.control_plane: Option<bool>`, `AgentSettings.control_plane: Option<bool>`. Absent means off — unlike `auto_compact`, whose absence means on.

- [ ] **Step 1: Write the failing test**

In `crates/server/src/agents/service.rs`'s existing test module:

```rust
#[tokio::test]
async fn control_plane_defaults_off_and_round_trips() {
    let (service, _dir) = service().await;
    let view = service.create(preset_input("plain")).await.unwrap();
    assert!(!view.control_plane, "a preset must not gain control-plane access by omission");

    let mut input = preset_input("ops");
    input.control_plane = Some(true);
    let view = service.create(input).await.unwrap();
    assert!(view.control_plane);
    assert!(service.get("ops").await.unwrap().control_plane, "must survive the store");
}
```

Reuse whatever `service()` and `preset_input()` helpers that module already defines.

- [ ] **Step 2: Run the test and watch it fail**

Run: `cargo test -p horsie-server --lib agents::service::`
Expected: FAIL — no field `control_plane`.

- [ ] **Step 3: Write the implementation**

Both migration files, identical:

```sql
-- Whether sessions from this preset may manage the horsie server itself.
-- NULL means no: control-plane access is granted, never inherited, so every
-- preset that predates the feature stays without it.
--
-- INTEGER, not BOOLEAN: the `sqlx::Any` driver cannot decode SQLite's BOOLEAN,
-- and every other flag in this schema is stored the same way for the same
-- reason.
ALTER TABLE agents ADD COLUMN control_plane INTEGER;
```

In `agents.fl`, on `AgentView`:

```
    /// Whether this preset's sessions may manage the horsie server itself —
    /// its agents, workflows, routines, environments and runtimes.
    control_plane: Bool,
```

and on `AgentPresetInput`, `control_plane: Option<Bool>` with a comment saying absent means off. Add the same `Option<Bool>` to `AgentSettings` in `session.fl`.

Run `make types`.

Add `control_plane` to the column list at `agents/store.rs:10` and to every row mapping; map it in `agents/service.rs:177` (`input.control_plane.unwrap_or(false)`) and `:193`. Set `control_plane: agent.control_plane` in the `WireAgentSettings` literal now living in `control/agents.rs`, and `control_plane: None` in the two other `AgentSettings` literals that will fail to compile (`workflows/service.rs:329`, `runtime_manager.rs:531`).

- [ ] **Step 4: Run the tests and watch them pass**

Run: `cargo test -p horsie-server --lib agents::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/server/migrations/ crates/models/fluorite/ crates/server/src/
git commit -m "feat(agents): a preset can carry control-plane access"
```

---

## Task 10: `ControlToolbox` specs

**Files:**
- Create: `crates/server/src/control/toolbox.rs`
- Modify: `crates/server/src/control/mod.rs`

**Interfaces:**
- Consumes: `Operation`, `Expose`.
- Produces: `pub struct ControlToolbox` with `pub fn new(inner: Arc<dyn Toolbox>, services: Arc<UserServices>, operations: Vec<Operation>) -> Self`, and `pub fn command_index(&self) -> String`.

One tool per resource, named `horsie_<resource>`. `Expose::Api` operations are excluded.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::wildcard_enum_match_arm)]
mod tests {
    use super::*;
    use horsie_agentcore::EmptyToolbox;

    async fn toolbox() -> (ControlToolbox, crate::testing::TestState) {
        let state = crate::testing::TestStateBuilder::default().build().await;
        let services = state.services().await;
        (
            ControlToolbox::new(Arc::new(EmptyToolbox), services, crate::control::operations()),
            state,
        )
    }

    #[tokio::test]
    async fn one_tool_per_resource_with_every_action() {
        let (tb, _state) = toolbox().await;
        let specs = tb.specs();
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"horsie_agents"));
        assert!(names.contains(&"horsie_workflows"));

        let agents = specs.iter().find(|s| s.name == "horsie_agents").unwrap();
        let actions = agents.input_schema["properties"]["action"]["enum"]
            .as_array()
            .unwrap();
        assert_eq!(actions.len(), 6);
        assert_eq!(agents.input_schema["required"][0], "action");
        let branches = agents.input_schema["oneOf"].as_array().unwrap();
        assert_eq!(branches.len(), 6);
        assert!(
            branches.iter().any(|b| b["properties"]["action"]["const"] == "create"),
            "each branch pins its action"
        );
    }

    #[tokio::test]
    async fn the_inner_toolbox_is_not_shadowed() {
        let (tb, _state) = toolbox().await;
        // EmptyToolbox contributes nothing, so every spec is ours; the point is
        // that specs() extends rather than replaces.
        assert!(tb.specs().len() >= 4);
    }

    #[tokio::test]
    async fn the_command_index_names_every_resource_and_action() {
        let (tb, _state) = toolbox().await;
        let index = tb.command_index();
        assert!(index.contains("agents {"));
        assert!(index.contains("invoke"));
    }
}
```

- [ ] **Step 2: Run the tests and watch them fail**

Run: `cargo test -p horsie-server --lib control::toolbox::`
Expected: FAIL — module does not exist.

- [ ] **Step 3: Write the implementation**

```rust
//! The agent-facing control plane. Executes in the server process against the
//! same services the routes use — the sandboxed runtime is never involved.
//!
//! Wraps an inner toolbox rather than composing into one, so control tools sit
//! outside `FilteredToolbox` and a session that sets `allowed_tools` does not
//! silently lose them. The preset's checkbox is the only gate.

use crate::control::{Expose, Operation};
use crate::users::UserServices;
use async_trait::async_trait;
use horsie_agentcore::{ToolCallError, ToolSpec, Toolbox};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

const PREFIX: &str = "horsie_";

pub struct ControlToolbox {
    inner: Arc<dyn Toolbox>,
    services: Arc<UserServices>,
    /// resource -> action -> operation, built once at spawn. Specs must not
    /// touch the database: `CompositeToolbox::execute` calls `specs()` on every
    /// box for every tool call.
    by_resource: BTreeMap<&'static str, BTreeMap<&'static str, Operation>>,
}

impl ControlToolbox {
    pub fn new(
        inner: Arc<dyn Toolbox>,
        services: Arc<UserServices>,
        operations: Vec<Operation>,
    ) -> Self {
        let mut by_resource: BTreeMap<&'static str, BTreeMap<&'static str, Operation>> =
            BTreeMap::new();
        for operation in operations
            .into_iter()
            .filter(|o| o.expose != Expose::Api)
        {
            by_resource
                .entry(operation.resource)
                .or_default()
                .insert(operation.action, operation);
        }
        Self { inner, services, by_resource }
    }

    /// One line per resource for the system prompt, so the model's first call
    /// is a real one rather than a guess.
    pub fn command_index(&self) -> String {
        self.by_resource
            .iter()
            .map(|(resource, actions)| {
                format!(
                    "{resource} {{{}}}",
                    actions.keys().copied().collect::<Vec<_>>().join(",")
                )
            })
            .collect::<Vec<_>>()
            .join(" · ")
    }

    fn spec(resource: &str, actions: &BTreeMap<&'static str, Operation>) -> ToolSpec {
        ToolSpec {
            name: format!("{PREFIX}{resource}"),
            description: format!(
                "Manage {resource} on this horsie server.\n\nActions:\n{}",
                actions
                    .values()
                    .map(|o| format!("- {}: {}", o.action, o.summary))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
            input_schema: json!({
                "type": "object",
                "required": ["action"],
                "properties": {
                    "action": { "enum": actions.keys().copied().collect::<Vec<_>>() }
                },
                "oneOf": actions.values().map(|o| json!({
                    "properties": { "action": { "const": o.action } },
                    "allOf": [o.schema],
                })).collect::<Vec<_>>(),
            }),
        }
    }
}
```

Register `pub mod toolbox;` in `control/mod.rs`.

- [ ] **Step 4: Run the tests and watch them pass**

Run: `cargo test -p horsie-server --lib control::toolbox::`
Expected: PASS, 3 tests.

The tests call `specs()`, so the `Toolbox` impl must exist now. Write it with the real `specs()` (extending `self.inner.specs()` with one `Self::spec` per resource) and an `execute` that only forwards to the inner box:

```rust
    async fn execute(&self, name: &str, input: Value, tool_call_id: &str)
        -> Result<Value, ToolCallError> {
        self.inner.execute(name, input, tool_call_id).await
    }
```

Not `unimplemented!()` — the workspace denies `panic` in production code, so a placeholder that panics will not compile past clippy. Task 11 replaces this body.

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/control/
git commit -m "feat(control): per-resource tool specs from the table"
```

---

## Task 11: `ControlToolbox` dispatch

**Files:**
- Modify: `crates/server/src/control/toolbox.rs`

**Interfaces:**
- Produces: `impl Toolbox for ControlToolbox`.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn a_tool_call_reaches_the_service() {
    let (tb, state) = toolbox().await;
    tb.execute(
        "horsie_agents",
        json!({"action": "create", "name": "deploy", "model": "test-model"}),
        "tc1",
    )
    .await
    .unwrap();
    assert_eq!(state.services().await.agents.get("deploy").await.unwrap().name, "deploy");
}

#[tokio::test]
async fn an_unknown_action_says_so_without_reaching_a_service() {
    let (tb, _state) = toolbox().await;
    let err = tb
        .execute("horsie_agents", json!({"action": "explode"}), "tc1")
        .await
        .unwrap_err();
    assert!(matches!(err, ToolCallError::InvalidInput(ref m) if m.contains("explode")));
}

#[tokio::test]
async fn a_missing_action_says_so() {
    let (tb, _state) = toolbox().await;
    let err = tb.execute("horsie_agents", json!({}), "tc1").await.unwrap_err();
    assert!(matches!(err, ToolCallError::InvalidInput(_)));
}

#[tokio::test]
async fn an_unrelated_tool_falls_through_to_the_inner_box() {
    let (tb, _state) = toolbox().await;
    // EmptyToolbox answers everything with InvalidInput("no tool named …"),
    // which is how we know the call was forwarded rather than swallowed.
    let err = tb.execute("read_file", json!({}), "tc1").await.unwrap_err();
    assert!(matches!(err, ToolCallError::InvalidInput(ref m) if m.contains("read_file")));
}
```

- [ ] **Step 2: Run the tests and watch them fail**

Run: `cargo test -p horsie-server --lib control::toolbox::`
Expected: FAIL — `execute` unimplemented.

- [ ] **Step 3: Write the implementation**

```rust
#[async_trait]
impl Toolbox for ControlToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        specs.extend(
            self.by_resource
                .iter()
                .map(|(resource, actions)| Self::spec(resource, actions)),
        );
        specs
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<Value, ToolCallError> {
        let Some(actions) = name
            .strip_prefix(PREFIX)
            .and_then(|resource| self.by_resource.get(resource))
        else {
            return self.inner.execute(name, input, tool_call_id).await;
        };
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolCallError::InvalidInput("'action' is required".to_string()))?;
        let operation = actions.get(action).ok_or_else(|| {
            ToolCallError::InvalidInput(format!(
                "no action '{action}'; available: {}",
                actions.keys().copied().collect::<Vec<_>>().join(", ")
            ))
        })?;
        operation
            .run(self.services.clone(), input.clone())
            .await
            .map_err(Into::into)
    }
}
```

The `action` key stays in the input passed to `run`. Every operation's input type is a fluorite struct or a hand-written one with `deny_unknown_fields` unset, so an extra key deserialises away harmlessly. If any input type ever sets `deny_unknown_fields`, strip `action` here instead.

- [ ] **Step 4: Run the tests and watch them pass**

Run: `cargo test -p horsie-server --lib control::toolbox::`
Expected: PASS, 7 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/control/toolbox.rs
git commit -m "feat(control): dispatch a tool call to its operation"
```

---

## Task 12: Wire the toolbox into the session

**Files:**
- Modify: `crates/server/src/sessions/session_actor/context.rs`

**Interfaces:**
- Consumes: `ControlToolbox`, `AgentSettings.control_plane`.

The layer goes beside `build_memory_layer` (`context.rs:106-127`) and applies **to the main agent only**, following `SessionAgentKind`'s existing rule that session-metadata tools are main-only (`context.rs:129`).

- [ ] **Step 1: Write the failing test**

In `context.rs`'s test module, or a new one following its conventions:

```rust
#[tokio::test]
async fn control_tools_reach_the_main_agent_only_when_the_preset_says_so() {
    let (base, services) = harness().await;

    let mut settings = settings_with(false);
    let (toolbox, _) = build_control_layer(base.clone(), &services, &settings, SessionAgentKind::Main);
    assert!(!toolbox.specs().iter().any(|s| s.name.starts_with("horsie_")));

    settings.control_plane = Some(true);
    let (toolbox, index) =
        build_control_layer(base.clone(), &services, &settings, SessionAgentKind::Main);
    assert!(toolbox.specs().iter().any(|s| s.name == "horsie_agents"));
    assert!(index.contains("agents {"));

    // A subagent does not inherit it.
    let (toolbox, _) = build_control_layer(
        base, &services, &settings, SessionAgentKind::Sub(uuid::Uuid::new_v4()),
    );
    assert!(!toolbox.specs().iter().any(|s| s.name.starts_with("horsie_")));
}
```

Copy `harness()` and `settings_with()` from whatever the neighbouring memory-layer tests use; if `context.rs` has no test module yet, build the two helpers from `build_memory_layer`'s call site.

- [ ] **Step 2: Run the test and watch it fail**

Run: `cargo test -p horsie-server --lib sessions::session_actor::context::`
Expected: FAIL — `cannot find function 'build_control_layer'`.

- [ ] **Step 3: Write the implementation**

```rust
/// Wrap `base` with the control-plane tools, and render the command index.
///
/// Main-agent only: a subagent or a workflow step inherits the session's
/// settings but not its authority over the server, following the same rule that
/// keeps session-metadata tools off them.
fn build_control_layer(
    base: Arc<dyn Toolbox>,
    services: &Arc<UserServices>,
    settings: &AgentSettings,
    kind: SessionAgentKind,
) -> (Arc<dyn Toolbox>, String) {
    if !matches!(kind, SessionAgentKind::Main) || settings.control_plane != Some(true) {
        return (base, String::new());
    }
    let toolbox = crate::control::toolbox::ControlToolbox::new(
        base,
        services.clone(),
        crate::control::operations(),
    );
    let index = toolbox.command_index();
    (Arc::new(toolbox), index)
}
```

Call it where `build_memory_layer`'s result is consumed, and add the index to the system prompt as its own section, following exactly how the memory index is inserted. The section text:

```
## Managing this horsie server

You can manage this server through the horsie_* tools. Available:
{index}

Call the tool for a resource with an `action` to act. Changes take effect
immediately and are not confirmed with the user first.
```

- [ ] **Step 4: Run the test and watch it pass**

Run: `cargo test -p horsie-server --lib sessions::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/sessions/session_actor/context.rs
git commit -m "feat(sessions): a control-plane preset gets the horsie tools"
```

---

## Task 13: End-to-end through a real turn

**Files:**
- Create or modify: the session e2e test file in `crates/server/tests/` alongside `session_server_e2e.rs`

**Interfaces:**
- Consumes: everything above.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn a_control_plane_session_creates_an_agent_preset() {
    let harness = /* the existing e2e harness with the mock LLM */;

    // A preset that may manage the server.
    harness.create_preset(json!({
        "name": "ops",
        "model": "mock",
        "control_plane": true
    })).await;

    // The mock LLM answers the first turn with a tool call and then a reply.
    harness.mock_llm.expect_tool_call(
        "horsie_agents",
        json!({"action": "create", "name": "made-by-agent", "model": "mock"}),
    );

    let session = harness.invoke("ops", "make me a preset").await;

    // Poll for the reply text, never for `Idle`: a session reports Idle twice —
    // once when provisioning finishes and again when the turn ends — so waiting
    // on status can read an empty transcript on a fast machine.
    harness.wait_for_reply(&session).await;

    assert_eq!(harness.get_preset("made-by-agent").await.name, "made-by-agent");
}
```

Adapt to the harness that exists: read `crates/server/tests/session_server_e2e.rs` and reuse `wait_for_reply` and the mock-LLM turn shape rather than inventing them. A fake runtime daemon must answer `ScanWorkspace` (and `SessionStart` when `use_plugins` resolves true) or session provisioning hangs with no output.

- [ ] **Step 2: Run the test and watch it fail**

Run: `TMPDIR=/tmp cargo test -p horsie-server --test session_server_e2e control_plane`
Expected: FAIL — the preset is not created.

- [ ] **Step 3: Fix whatever it reveals**

No new production code is expected; this test exists to prove the wiring. If it fails for a real reason, fix the cause rather than the test.

- [ ] **Step 4: Run the test and watch it pass**

Run: `TMPDIR=/tmp cargo test -p horsie-server --test session_server_e2e control_plane`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/server/tests/
git commit -m "test(control): an agent creates a preset end to end"
```

---

## Task 14: The web UI checkbox

**Files:**
- Modify: the agent preset form under `clients/web/src/`, and its TypeScript types if they are not regenerated by `make types`

**Interfaces:**
- Consumes: `AgentView.control_plane`, `AgentPresetInput.control_plane`.

- [ ] **Step 1: Find the form and its existing boolean**

`auto_compact` is already a boolean on this form. Locate it and copy its control, label placement and save wiring exactly.

Run: `rg -l "auto_compact" clients/web/src`

- [ ] **Step 2: Add the checkbox**

Label: **Control plane access**. Help text: "Let sessions from this preset manage this horsie server — its agents, workflows, routines, environments and runtimes. Changes are applied immediately, without confirmation."

Settled configuration belongs behind an info affordance rather than in a header; put it with the other preset toggles.

- [ ] **Step 3: Install and run the web UI**

```bash
cd clients/web && bun install --frozen-lockfile && bun run dev
```

`npm ci` fails in a fresh worktree; CI uses bun.

- [ ] **Step 4: See it**

Open the agent preset form, screenshot it with the checkbox visible, toggle it on, save, reload, and confirm it persists. `tsc --noEmit` passing is not evidence the element was drawn.

- [ ] **Step 5: Commit**

```bash
git add clients/web/
git commit -m "feat(web): a control-plane checkbox on agent presets"
```

---

## Task 15: PR 2 verification

- [ ] **Step 1: Format and lint**

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

- [ ] **Step 2: Full workspace suite**

```bash
TMPDIR=/tmp cargo test --workspace
```

- [ ] **Step 3: Verify the schema against real providers**

The spec calls for this and no test can substitute. For each configured provider kind available — anthropic, openai, the chatgpt backend, deepseek, kimi — start a control-plane session and ask it to create an agent preset. Record which providers were actually exercised and which were skipped for lack of credentials; a provider silently degrading the `oneOf` is invisible from our side and this is the only way to see it.

- [ ] **Step 4: Confirm the regression tests fail without the fix**

For the two behavioural guards — `a_param_never_overwrites_the_body` and `control_plane_defaults_off_and_round_trips` — revert the production change locally, watch the test fail, then restore. A regression test that passes against the broken code is not a regression test.

- [ ] **Step 5: Commit anything the verification changed**

```bash
git commit -am "chore: verification fixes"
```

---

## Self-review notes

Checked against the spec:

- Operation descriptor, `run` closure, schema from fluorite `JsonSchema` — Task 2.
- Router folded out of the table, `path -> MethodRouter` to survive shared paths — Task 4.
- `Expose` with all three variants — Task 2; `ToolOnly` is defined and skipped by the fold but unused until PR 3, which is stated in the plan's scope section rather than left implicit.
- Fill-missing-only param merge protecting the name-immutability checks — Task 3, with the reason recorded in the test name.
- `ControlError` converting to both surfaces — Task 1.
- Per-resource tools, `oneOf` branches, no hand-written schema — Task 10.
- System-prompt command index — Tasks 10 and 12.
- `control_plane` gate, main-agent only, defaulting off — Tasks 9 and 12.
- Web UI checkbox seen, not merely compiled — Task 14.
- Provider verification of `oneOf` before shipping — Task 15, with skipped providers recorded rather than silently dropped.

**Not in this plan, by design:** the classification test and `NON_OPERATIONS` (they only become meaningful once every resource is migrated — PR 3), `sessions.read` and signature stripping (PR 3), the `PerOperation` fallback renderer (built only if Task 15 finds a provider that needs it), and CLI generation (PR 4).
