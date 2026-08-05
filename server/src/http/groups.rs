//! Session groups and session annotations. Both are supervisor-journal state,
//! so every handler is a thin ask-and-map over `SessionSupervisorCommand`.

use crate::http::Scope;
use crate::http::error::Api;
use crate::http::handlers::ask;
use crate::sessions::supervisor::{GroupError, SessionSupervisorCommand};
use axum::Json;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use horsie_models::now_ms;
use horsie_models::session_api::{
    Ack, CreateGroupRequest, CreateGroupResponse, ListGroupsResponse, RenameGroupRequest,
    SessionGroupView, SetAnnotationsRequest,
};
use std::collections::BTreeMap;

fn group_error(e: GroupError) -> Api {
    match e {
        GroupError::NotFound(m) => Api::not_found(m),
        GroupError::NameTaken(m) => Api::conflict("name_taken", m),
        GroupError::Invalid(m) => Api::unprocessable(m),
    }
}

pub async fn list_groups(Scope(state): Scope) -> Result<impl IntoResponse, Api> {
    let groups = ask(&state, |reply| SessionSupervisorCommand::ListGroups {
        reply,
    })
    .await?;
    let groups = groups
        .into_iter()
        .map(|(name, _)| SessionGroupView { name })
        .collect();
    Ok(Json(ListGroupsResponse { groups }))
}

pub async fn create_group(
    Scope(state): Scope,
    Json(req): Json<CreateGroupRequest>,
) -> Result<impl IntoResponse, Api> {
    ask(&state, |reply| SessionSupervisorCommand::CreateGroup {
        name: req.name.clone(),
        created_at: now_ms(),
        reply,
    })
    .await?
    .map_err(group_error)?;
    Ok((
        StatusCode::CREATED,
        Json(CreateGroupResponse {
            group: SessionGroupView { name: req.name },
        }),
    ))
}

pub async fn rename_group(
    Scope(state): Scope,
    Path(name): Path<String>,
    Json(req): Json<RenameGroupRequest>,
) -> Result<impl IntoResponse, Api> {
    ask(&state, |reply| SessionSupervisorCommand::RenameGroup {
        old: name,
        new: req.name,
        reply,
    })
    .await?
    .map_err(group_error)?;
    Ok(Json(Ack {}))
}

pub async fn delete_group(
    Scope(state): Scope,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, Api> {
    ask(&state, |reply| SessionSupervisorCommand::DeleteGroup {
        name,
        reply,
    })
    .await?
    .map_err(group_error)?;
    Ok(Json(Ack {}))
}

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
    Path(id): Path<String>,
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
