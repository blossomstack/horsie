//! Session annotations: user-set key-value metadata, and the mechanism tags
//! ride on. Supervisor-journal state, so the handler is a thin ask-and-map
//! over `SessionSupervisorCommand`.

use crate::http::error::Api;
use crate::http::handlers::ask;
use crate::http::{Scope, Scoped};
use crate::sessions::supervisor::SessionSupervisorCommand;
use axum::Json;
use axum::response::IntoResponse;
use horsie_models::session_api::{Ack, SetAnnotationsRequest};
use std::collections::BTreeMap;

/// Annotation keys are machine-facing: lowercase slug characters only.
fn valid_annotation_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 128
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

pub async fn set_annotations(
    Scope(state): Scope,
    Scoped(id): Scoped<String>,
    Json(req): Json<SetAnnotationsRequest>,
) -> Result<impl IntoResponse, Api> {
    if req.set.iter().any(|e| !valid_annotation_key(&e.key))
        || req.remove.iter().any(|k| !valid_annotation_key(k))
    {
        return Err(Api::unprocessable(
            "annotation keys must be 1-128 chars of [a-z0-9._-]",
        ));
    }
    let set: BTreeMap<String, String> = req.set.into_iter().map(|e| (e.key, e.value)).collect();
    ask(&state, |reply| {
        SessionSupervisorCommand::SetSessionAnnotations {
            id,
            set,
            remove: req.remove,
            reply,
        }
    })
    .await?
    .map_err(Api::not_found)?;
    Ok(Json(Ack {}))
}
