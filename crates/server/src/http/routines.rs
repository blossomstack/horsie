//! HTTP surface for routines: CRUD, `POST /api/routines/:name/run` (the manual
//! and API trigger, sharing the runner with the scheduler's timer), and
//! `GET /api/routines/:name/sessions` — the run list, which is the only place a
//! routine's sessions appear.

use super::Scope;
use super::error::Api;
use super::handlers;
use crate::routines::RoutineError;
use crate::sessions::supervisor::SessionSupervisorCommand;
use axum::Json;
use axum::extract::Path;
use axum::http::StatusCode;
use horsie_models::now_ms;
use horsie_models::routines::{
    RoutineInput, RoutineRunResponse, RoutineSessionsResponse, RoutineView,
};
use horsie_models::session::SessionSummary;

/// Map the typed service error onto the envelope without string matching.
fn api_err(e: RoutineError) -> Api {
    match e {
        RoutineError::NotFound(m) => Api::not_found(m),
        RoutineError::Conflict(m) => Api::conflict("conflict", m),
        RoutineError::Invalid(m) => Api::unprocessable(m),
        RoutineError::Internal(m) => Api::internal(m),
    }
}

/// GET /api/routines
pub async fn list_routines(Scope(state): Scope) -> Result<Json<Vec<RoutineView>>, Api> {
    state.routines.list().await.map(Json).map_err(api_err)
}

/// POST /api/routines
pub async fn create_routine(
    Scope(state): Scope,
    Json(input): Json<RoutineInput>,
) -> Result<(StatusCode, Json<RoutineView>), Api> {
    state
        .routines
        .create(input, now_ms())
        .await
        .map(|v| (StatusCode::CREATED, Json(v)))
        .map_err(api_err)
}

/// GET /api/routines/:name
pub async fn get_routine(
    Scope(state): Scope,
    Path(name): Path<String>,
) -> Result<Json<RoutineView>, Api> {
    state.routines.get(&name).await.map(Json).map_err(api_err)
}

/// PUT /api/routines/:name — full replace; the path is the id of record.
pub async fn replace_routine(
    Scope(state): Scope,
    Path(name): Path<String>,
    Json(input): Json<RoutineInput>,
) -> Result<Json<RoutineView>, Api> {
    state
        .routines
        .replace(&name, input, now_ms())
        .await
        .map(Json)
        .map_err(api_err)
}

/// DELETE /api/routines/:name — and every session it created.
///
/// The routine's page is the only place its runs are listed, so keeping them
/// would leave sessions that no view reaches and whose runtimes nothing
/// releases. The UI says how many will go before asking.
pub async fn delete_routine(
    Scope(state): Scope,
    Path(name): Path<String>,
) -> Result<StatusCode, Api> {
    state.routines.get(&name).await.map_err(api_err)?;
    for (id, _) in routine_sessions(&state, &name).await? {
        // Best effort per session: a routine whose delete half-failed is worse
        // than one whose runs outlive it by a restart.
        if let Err(e) = handlers::ask(&state, |reply| SessionSupervisorCommand::Delete {
            id: id.clone(),
            reply,
        })
        .await?
        {
            tracing::warn!(routine = %name, session = %id, error = %e, "deleting a routine run failed");
        }
    }
    state
        .routines
        .delete(&name)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(api_err)
}

/// POST /api/routines/:name/run — trigger now, whatever the schedule says.
///
/// Deliberately independent of `enabled`, which pauses the timer only: pausing
/// a routine should not take away the button that lets you try it.
pub async fn run_routine(
    Scope(state): Scope,
    Path(name): Path<String>,
) -> Result<(StatusCode, Json<RoutineRunResponse>), Api> {
    state
        .routine_runner
        .run(&name, now_ms())
        .await
        .map(|session| (StatusCode::CREATED, Json(RoutineRunResponse { session })))
        .map_err(api_err)
}

/// GET /api/routines/:name/sessions — the routine's runs, newest first.
pub async fn get_routine_sessions(
    Scope(state): Scope,
    Path(name): Path<String>,
) -> Result<Json<RoutineSessionsResponse>, Api> {
    state.routines.get(&name).await.map_err(api_err)?;
    let mut sessions: Vec<SessionSummary> = routine_sessions(&state, &name)
        .await?
        .into_iter()
        .map(|(_, summary)| summary)
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.created_at));
    Ok(Json(RoutineSessionsResponse { sessions }))
}

/// Every session created by `name`, as (id, summary).
async fn routine_sessions(
    state: &crate::users::UserServices,
    name: &str,
) -> Result<Vec<(String, SessionSummary)>, Api> {
    let sessions = handlers::ask(state, |reply| SessionSupervisorCommand::List { reply }).await?;
    Ok(sessions
        .iter()
        .filter(|(_, rec)| rec.spec.routine() == Some(name))
        .map(|(id, rec)| (id.clone(), handlers::summary(id, rec)))
        .collect())
}
