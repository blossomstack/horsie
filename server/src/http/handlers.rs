//! REST handlers over the `SessionSupervisor`. Bodies are fluorite wire types;
//! errors are the uniform `ApiError` envelope.

use crate::http::AppState;
use crate::http::error::Api;
use crate::sessions::UserMessageError;
use crate::sessions::events::fold_session_state;
use crate::sessions::spec::{
    AgentSettings, SessionSpec, SessionStatus, status_kind, status_reason,
};
use crate::sessions::supervisor::{SessionRecord, SessionSupervisorCommand};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use horsie_models::session::{
    AgentSettings as WireAgentSettings, SessionDetail, SessionStatusKind, SessionSummary,
};
use horsie_models::session_api::{
    CreateSessionRequest, CreateSessionResponse, GetSessionResponse, ListSessionsResponse,
    SendMessageRequest, SessionAck,
};
use uuid::Uuid;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

pub async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "ok": true }))
}

/// Ask the supervisor a question, mapping a closed mailbox to a 500.
async fn ask<T, F>(state: &AppState, make: F) -> Result<T, Api>
where
    F: FnOnce(tokio::sync::oneshot::Sender<T>) -> SessionSupervisorCommand,
    T: Send + 'static,
{
    state
        .supervisor
        .ask(make)
        .await
        .map_err(|_| Api::internal("session supervisor unavailable"))
}

/// Storage `AgentSettings` from the wire request, applying defaults.
fn settings_from_wire(w: WireAgentSettings) -> AgentSettings {
    AgentSettings {
        model: w.model,
        system_prompt: w.system_prompt,
        allowed_tools: w.allowed_tools,
        allow_ask_user: w.allow_ask_user.unwrap_or(false),
        use_plugins: w.use_plugins,
        max_iterations: w.max_iterations,
        max_retries: w.max_retries.unwrap_or(0),
    }
}

fn summary(id: &str, rec: &SessionRecord) -> SessionSummary {
    SessionSummary {
        id: id.to_string(),
        name: rec.spec.name.clone(),
        status: status_kind(&rec.status),
        created_at: rec.created_at,
        last_error: status_reason(&rec.status),
    }
}

pub async fn create_session(
    State(state): State<AppState>,
    Json(req): Json<CreateSessionRequest>,
) -> Result<impl IntoResponse, Api> {
    if req.workdirs.is_empty() {
        return Err(Api::unprocessable("at least one workdir is required"));
    }
    let paths: Vec<std::path::PathBuf> =
        req.workdirs.iter().map(std::path::PathBuf::from).collect();
    let workspaces = horsie_models::derive_workspaces(&paths)
        .map_err(|e| Api::unprocessable(format!("invalid workdirs: {e}")))?;
    let caps = (state.caps_finalize)(
        req.capabilities
            .unwrap_or_else(|| state.default_caps.clone()),
    );
    let spec = SessionSpec {
        name: req.name,
        agent: settings_from_wire(req.agent),
        workspaces,
        capabilities: caps,
        vendor: req.vendor.unwrap_or_else(|| "local".to_string()),
        plugins_dir: state.plugins_dir.clone(),
        hook_path: state.hook_path.clone(),
    };
    let created_at = now_ms();
    let id = ask(&state, |reply| SessionSupervisorCommand::Create {
        spec: spec.clone(),
        created_at,
        reply,
    })
    .await?;
    let rec = SessionRecord {
        spec,
        status: SessionStatus::Provisioning,
        created_at,
    };
    Ok((
        StatusCode::CREATED,
        Json(CreateSessionResponse {
            session: summary(&id, &rec),
        }),
    ))
}

pub async fn list_sessions(State(state): State<AppState>) -> Result<impl IntoResponse, Api> {
    let sessions = ask(&state, |reply| SessionSupervisorCommand::List { reply }).await?;
    let sessions = sessions.iter().map(|(id, rec)| summary(id, rec)).collect();
    Ok(Json(ListSessionsResponse { sessions }))
}

pub async fn get_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, Api> {
    let rec = ask(&state, |reply| SessionSupervisorCommand::Get {
        id: id.clone(),
        reply,
    })
    .await?
    .ok_or_else(|| Api::not_found(format!("no such session: {id}")))?;
    // pending_question / last_error are durable truth in the session journal.
    let pending_question = match Uuid::parse_str(&id) {
        Ok(uuid) => {
            fold_session_state(&state.journal, uuid)
                .await
                .pending_question
        }
        Err(_) => None,
    };
    let detail = SessionDetail {
        id: id.clone(),
        name: rec.spec.name.clone(),
        status: status_kind(&rec.status),
        created_at: rec.created_at,
        last_error: status_reason(&rec.status),
        pending_question,
        model: rec.spec.agent.model.clone(),
        workdirs: rec
            .spec
            .workspaces
            .iter()
            .map(|w| w.path.to_string_lossy().into_owned())
            .collect(),
        vendor: rec.spec.vendor.clone(),
    };
    Ok(Json(GetSessionResponse { session: detail }))
}

pub async fn send_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SendMessageRequest>,
) -> Result<impl IntoResponse, Api> {
    let result = ask(&state, |reply| SessionSupervisorCommand::UserMessage {
        id,
        text: req.text,
        reply,
    })
    .await?;
    match result {
        Ok(()) => Ok((StatusCode::ACCEPTED, Json(SessionAck {}))),
        Err(UserMessageError::NotFound) => Err(Api::not_found("no such session")),
        Err(UserMessageError::Provisioning) => Err(Api::conflict(
            "provisioning",
            "session is still provisioning",
        )),
        Err(UserMessageError::TurnInFlight) => Err(Api::conflict(
            "turn_in_flight",
            "a turn is already in flight",
        )),
        Err(UserMessageError::RecoveryFailed(msg)) => Err(Api::bad_gateway("recovery_failed", msg)),
    }
}

pub async fn stop_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, Api> {
    let result = ask(&state, |reply| SessionSupervisorCommand::Stop { id, reply }).await?;
    match result {
        Ok(()) => Ok(Json(SessionAck {})),
        Err(msg) => Err(Api::not_found(msg)),
    }
}

pub async fn delete_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, Api> {
    let result = ask(&state, |reply| SessionSupervisorCommand::Delete {
        id,
        reply,
    })
    .await?;
    match result {
        Ok(()) => Ok(Json(SessionAck {})),
        Err(msg) => Err(Api::not_found(msg)),
    }
}

/// Map a storage status to its wire kind (re-exported for the SSE layer).
pub(crate) fn wire_status_kind(s: &SessionStatus) -> SessionStatusKind {
    status_kind(s)
}
