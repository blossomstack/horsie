//! `/api/projects` — the one resource that is an account's rather than a
//! project's.
//!
//! Outside the `/api/p/{project}` prefix by necessity: this is how a client
//! learns what may go in that segment. It takes [`Account`] rather than
//! [`Scope`] for the same reason.
//!
//! Deliberately *not* a control-plane operation. An operation runs inside a
//! project, against a resolved [`Scope`]; an agent that could create and delete
//! projects would be reaching outside the one it is running in, which is the
//! boundary everything else here exists to hold.

use super::error::Api;
use super::{Account, AppState};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use horsie_models::projects::{ProjectInput, ProjectView};

use crate::projects::{ProjectError, ProjectId, ProjectRow};

/// GET /api/projects
pub async fn list_projects(
    State(state): State<AppState>,
    Account(user): Account,
) -> Result<Json<Vec<ProjectView>>, Api> {
    // Through `default_project` first: an account that has never been seen has
    // no rows yet, and an empty list would send a fresh browser to a project
    // switcher with nothing in it. This is the resolution-time creation the
    // three account-creation paths all share.
    state
        .shared
        .project_service
        .default_project(&user)
        .await
        .map_err(api_error)?;
    let projects = state
        .shared
        .project_service
        .list(&user)
        .await
        .map_err(api_error)?;
    Ok(Json(projects.iter().map(view).collect()))
}

/// POST /api/projects
pub async fn create_project(
    State(state): State<AppState>,
    Account(user): Account,
    Json(input): Json<ProjectInput>,
) -> Result<(StatusCode, Json<ProjectView>), Api> {
    let row = state
        .shared
        .project_service
        .create(&user, &input.name)
        .await
        .map_err(api_error)?;
    Ok((StatusCode::CREATED, Json(view(&row))))
}

/// PUT /api/projects/:id
pub async fn rename_project(
    State(state): State<AppState>,
    Account(user): Account,
    Path(id): Path<String>,
    Json(input): Json<ProjectInput>,
) -> Result<Json<ProjectView>, Api> {
    let row = state
        .shared
        .project_service
        .rename(&user, &ProjectId::new(id), &input.name)
        .await
        .map_err(api_error)?;
    Ok(Json(view(&row)))
}

/// DELETE /api/projects/:id
///
/// Resolves the project's bundle first, because deleting it means deleting its
/// sessions, and the session actor is what knows how to tell a vendor to
/// destroy a machine. Resolving is safe here in a way it is not in [`Scope`]:
/// ownership has already been established by `delete` itself, which reads the
/// row.
pub async fn delete_project(
    State(state): State<AppState>,
    Account(user): Account,
    Path(id): Path<String>,
) -> Result<StatusCode, Api> {
    let id = ProjectId::new(id);
    // Ownership before anything is built, exactly as `Scope` does it: a
    // stranger's id must not materialise a supervisor.
    match state.shared.project_service.store().get(&id).await {
        Ok(Some(row)) if row.user_id == user => {}
        Ok(_) => return Err(Api::not_found("no such project")),
        Err(e) => {
            tracing::error!(project = %id, error = %e, "reading a project failed");
            return Err(Api::internal("could not resolve the project"));
        }
    }
    let services = state
        .projects
        .get(&id)
        .await
        .map_err(|e| Api::internal(format!("could not resolve the project: {e}")))?;
    state
        .shared
        .project_service
        .delete(&user, &id, &services.supervisor)
        .await
        .map_err(api_error)?;
    Ok(StatusCode::NO_CONTENT)
}

fn view(row: &ProjectRow) -> ProjectView {
    ProjectView {
        id: row.id.as_str().to_string(),
        name: row.name.clone(),
        is_default: row.is_default,
        created_at: row.created_at.clone(),
        updated_at: row.updated_at.clone(),
    }
}

fn api_error(e: ProjectError) -> Api {
    match e {
        ProjectError::NotFound(m) => Api::not_found(m),
        ProjectError::Conflict(m) => Api::conflict("project_exists", m),
        ProjectError::Invalid(m) => Api::unprocessable(m),
        ProjectError::Internal(m) => {
            tracing::error!(error = %m, "a project operation failed");
            Api::internal(m)
        }
    }
}
