//! REST handlers over the `SessionSupervisor`. Bodies are fluorite wire types;
//! errors are the uniform `ApiError` envelope.

use crate::http::AppState;
use crate::http::error::Api;
use crate::sessions::UserMessageError;
use crate::sessions::events::fold_session_state;
use crate::sessions::spec::{
    AgentSettings, ProvisionStepSpec, SessionSpec, SessionStatus, WorkspaceDef, status_kind,
    status_reason,
};
use crate::sessions::supervisor::{SessionRecord, SessionSupervisorCommand};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use horsie_models::session::{
    AgentSettings as WireAgentSettings, SessionDetail, SessionStatusKind, SessionSummary, TaskItem,
    TaskStatus as WireTaskStatus, UsageView,
};
use horsie_models::session_api::{
    CreateSessionRequest, CreateSessionResponse, GetSessionResponse, HistoryPage,
    ListSessionsResponse, SendMessageRequest, SessionAck,
};
use horsie_workflow::{AgentHistoryPage, HistoryQuery, TaskStatus as AgentTaskStatus};
use serde::Deserialize;
use uuid::Uuid;

/// Default and maximum messages returned by one `/history` page.
const HISTORY_DEFAULT_LIMIT: usize = 50;
const HISTORY_MAX_LIMIT: usize = 200;

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
        allowed_tools: w.allowed_tools,
        use_plugins: w.use_plugins,
        max_iterations: w.max_iterations,
        max_retries: w.max_retries.unwrap_or(0),
        mcp_servers: w.mcp_servers.unwrap_or_default(),
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
    // The workspace is always vendor-allocated; `repos` (when the vendor
    // supports provisioning) become git-checkout provision steps that clone
    // into it. The UI only sends repos to a provisioning-capable vendor; a
    // vendor that can't provision rejects them at `create()`.
    let repos = req.repos.unwrap_or_default();
    let provision: Vec<ProvisionStepSpec> = horsie_models::provision_from_repos(&repos)
        .map_err(|e| Api::unprocessable(format!("invalid repos: {e}")))?
        .into_iter()
        .map(|s| ProvisionStepSpec {
            name: s.name,
            uses: s.uses,
            with: s.with.into_iter().map(|p| (p.key, p.value)).collect(),
        })
        .collect();
    let workspaces = vec![WorkspaceDef {
        name: "main".into(),
    }];
    // Repo provisioning clones inside the sandbox, so the default capability
    // spec (which may block the network) gets a network-allow override; an
    // explicit request-supplied spec always wins untouched.
    let caps = match req.capabilities {
        Some(c) => (state.caps_finalize)(c),
        None if !provision.is_empty() => {
            let mut c = state.default_caps.clone();
            c.network = horsie_models::capabilities::NetworkPolicy::Allow(
                horsie_models::capabilities::AllowNetwork {},
            );
            c
        }
        None => state.default_caps.clone(),
    };
    // Selected bundle names (empty → the provisioner falls back to the
    // default-enabled set). Selecting bundles implies plugins are surfaced, so
    // force the agent's opt-in when any are chosen.
    let plugins = req.plugins.unwrap_or_default();
    let mut agent = settings_from_wire(req.agent);
    if !plugins.is_empty() {
        agent.use_plugins = Some(true);
    }
    let spec = SessionSpec {
        name: req.name,
        agent,
        workspaces,
        provision,
        capabilities: caps,
        vendor: req
            .vendor
            .unwrap_or_else(|| state.config_store.default_vendor()),
        plugins_dir: state.plugins_dir.clone(),
        hook_path: state.hook_path.clone(),
        plugins,
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
        vendor: rec.spec.vendor.clone(),
        repos: rec
            .spec
            .provision
            .iter()
            .filter(|s| s.uses == "git_checkout")
            .filter_map(|s| {
                s.with
                    .iter()
                    .find(|(k, _)| k == "url")
                    .map(|(_, v)| v.clone())
            })
            .collect(),
    };
    Ok(Json(GetSessionResponse { session: detail }))
}

/// Query params for `GET /api/sessions/:id/history`.
#[derive(Deserialize)]
pub struct HistoryParams {
    /// Return the page of messages immediately before this message id; absent
    /// requests the latest (tail) page.
    before: Option<String>,
    /// Max messages; defaults to [`HISTORY_DEFAULT_LIMIT`], capped at
    /// [`HISTORY_MAX_LIMIT`].
    limit: Option<usize>,
}

fn wire_task_status(status: AgentTaskStatus) -> WireTaskStatus {
    match status {
        AgentTaskStatus::Pending => WireTaskStatus::Pending,
        AgentTaskStatus::InProgress => WireTaskStatus::InProgress,
        AgentTaskStatus::Completed => WireTaskStatus::Completed,
    }
}

fn to_wire_history(page: AgentHistoryPage) -> HistoryPage {
    HistoryPage {
        messages: page.messages,
        has_more: page.has_more,
        tasks: page.tasks.map(|tasks| {
            tasks
                .into_iter()
                .map(|t| TaskItem {
                    id: t.id,
                    content: t.content,
                    status: wire_task_status(t.status),
                })
                .collect()
        }),
        usage: page.usage.map(|u| UsageView {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
        }),
    }
}

/// A window of a session's conversation history, from the agent's in-memory
/// state (no journal replay in the server). The tail page (no `before`) also
/// carries the current task list and cumulative usage.
pub async fn get_history(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HistoryParams>,
) -> Result<impl IntoResponse, Api> {
    let limit = params
        .limit
        .unwrap_or(HISTORY_DEFAULT_LIMIT)
        .clamp(1, HISTORY_MAX_LIMIT);
    let query = HistoryQuery {
        before: params.before,
        limit,
    };
    let page = ask(&state, |reply| SessionSupervisorCommand::History {
        id: id.clone(),
        query,
        reply,
    })
    .await?
    .ok_or_else(|| Api::not_found(format!("no such session: {id}")))?;
    Ok(Json(to_wire_history(page)))
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
