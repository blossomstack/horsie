//! REST handlers over the `SessionSupervisor`. Bodies are fluorite wire types;
//! errors are the uniform `ApiError` envelope.

use crate::http::AppState;
use crate::http::error::Api;
use crate::sessions::UserMessageError;
use crate::sessions::session_actor::{AskAnswer, InboxMessage, SessionUsageStats};
use crate::sessions::spec::{
    AgentSettings, PendingAsk, ProvisionStepSpec, SessionSpec, SessionStatus, WorkspaceDef,
    status_kind, status_reason,
};
use crate::sessions::subagents::{SubAgentParent, SubAgentRecord, SubAgentStatus};
use crate::sessions::supervisor::{SessionRecord, SessionSupervisorCommand};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use horsie_models::session::{
    AgentSettings as WireAgentSettings, AgentUsageView, AnswerAsksRequest, PendingAskView,
    QueuedMessage, SessionDetail, SessionStatusKind, SessionSummary,
    SessionUsageStats as WireSessionUsageStats, TaskItem, TaskStatus as WireTaskStatus, UsageView,
};
use horsie_models::session_api::{
    Ack, CreateSessionRequest, CreateSessionResponse, GetSessionResponse,
    GetSessionSubAgentsResponse, GetSessionUsageResponse, HistoryPage, ListSessionsResponse,
    RepoConfig, SendMessageRequest, SessionAck, SubAgentView,
};
use horsie_workflow::{AgentHistoryPage, HistoryQuery, TaskStatus as AgentTaskStatus};
use serde::Deserialize;
use uuid::Uuid;

/// Default and maximum messages returned by one `/history` page.
const HISTORY_DEFAULT_LIMIT: usize = 50;
const HISTORY_MAX_LIMIT: usize = 200;

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// The wire shape of one queued message. Shared with the SSE layer so the
/// detail endpoint and `InboxChanged` can never disagree about the queue.
pub fn wire_queued_message(m: InboxMessage) -> QueuedMessage {
    QueuedMessage {
        id: m.id,
        text: m.text,
        at_ms: m.at_ms,
    }
}

pub async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "ok": true }))
}

/// Ask the supervisor a question, mapping a closed mailbox to a 500.
pub(crate) async fn ask<T, F>(state: &AppState, make: F) -> Result<T, Api>
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
        memory_spaces: w.memory_spaces.unwrap_or_default(),
        thinking_effort: w.thinking_effort,
        max_concurrent_subagents: w.max_concurrent_subagents,
    }
}

pub(crate) fn summary(
    id: &str,
    rec: &SessionRecord,
    status: Option<&SessionStatus>,
) -> SessionSummary {
    SessionSummary {
        id: id.to_string(),
        name: rec.spec.name.clone(),
        status: status.map(status_kind),
        created_at: rec.created_at,
        last_error: status.and_then(status_reason),
    }
}

/// Assemble a [`SessionSpec`] from the pieces every creation path supplies.
/// Shared by `create_session` and the agent-preset invoke endpoint so the two
/// can never drift on provisioning, plugin, or thinking-effort semantics.
pub(crate) async fn build_session_spec(
    state: &AppState,
    name: Option<String>,
    agent: WireAgentSettings,
    vendor: Option<String>,
    repos: Vec<RepoConfig>,
    plugins: Option<Vec<String>>,
) -> Result<SessionSpec, Api> {
    // The workspace is always vendor-allocated; `repos` (when the vendor
    // supports provisioning) become git-checkout provision steps that clone
    // into it. The UI only sends repos to a provisioning-capable vendor; a
    // vendor that can't provision rejects them at `create()`.
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
    // Selected bundle names (empty → the provisioner falls back to the
    // default-enabled set). Selecting bundles implies plugins are surfaced, so
    // force the agent's opt-in when any are chosen.
    let plugins = plugins.unwrap_or_default();
    let mut agent = settings_from_wire(agent);
    if !plugins.is_empty() {
        agent.use_plugins = Some(true);
    }
    // Resolve the effective thinking effort once, here: session choice wins,
    // else the model's configured default, else nothing. Effort is fixed for a
    // session's lifetime (changing it mid-conversation invalidates the prompt
    // cache), so freezing it at creation is deliberate. A requested value must
    // be canonical AND offered by the model — otherwise it reaches the provider
    // as an opaque 400.
    {
        let model_row = state
            .config_store
            .view()
            .await
            .map_err(Api::internal)?
            .models
            .into_iter()
            .find(|m| m.alias == agent.model);
        match agent.thinking_effort.as_deref() {
            Some(requested) => {
                let effort =
                    horsie_agentcore::ThinkingEffort::parse(requested).ok_or_else(|| {
                        Api::unprocessable(format!("unknown thinking effort '{requested}'"))
                    })?;
                let offered = model_row
                    .as_ref()
                    .and_then(|m| m.thinking_efforts.clone())
                    .unwrap_or_default();
                if !offered.iter().any(|e| e == effort.as_str()) {
                    return Err(Api::unprocessable(format!(
                        "model '{}' does not offer thinking effort '{requested}'",
                        agent.model
                    )));
                }
            }
            None => {
                agent.thinking_effort = model_row.and_then(|m| m.thinking_effort);
            }
        }
    }
    Ok(SessionSpec {
        name,
        agent,
        workspaces,
        provision,
        vendor: vendor.unwrap_or_else(|| state.config_store.default_vendor()),
        plugins,
    })
}

pub async fn create_session(
    State(state): State<AppState>,
    Json(req): Json<CreateSessionRequest>,
) -> Result<impl IntoResponse, Api> {
    let spec = build_session_spec(
        &state,
        req.name,
        req.agent,
        req.vendor,
        req.repos.unwrap_or_default(),
        req.plugins,
    )
    .await?;
    let created_at = now_ms();
    let id = ask(&state, |reply| SessionSupervisorCommand::Create {
        spec: spec.clone(),
        created_at,
        reply,
    })
    .await?;
    let rec = SessionRecord { spec, created_at };
    Ok((
        StatusCode::CREATED,
        Json(CreateSessionResponse {
            // A freshly created session is loaded and idle; its runtime is
            // being provisioned in the background.
            session: summary(&id, &rec, Some(&SessionStatus::Idle)),
        }),
    ))
}

pub async fn list_sessions(State(state): State<AppState>) -> Result<impl IntoResponse, Api> {
    let sessions = ask(&state, |reply| SessionSupervisorCommand::List { reply }).await?;
    let sessions = sessions
        .iter()
        .map(|(id, rec, status)| summary(id, rec, status.as_ref()))
        .collect();
    Ok(Json(ListSessionsResponse { sessions }))
}

pub async fn get_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, Api> {
    let (rec, snapshot) = ask(&state, |reply| SessionSupervisorCommand::Get {
        id: id.clone(),
        reply,
    })
    .await?
    .ok_or_else(|| Api::not_found(format!("no such session: {id}")))?;
    let status = snapshot.as_ref().map(|s| s.status.clone());
    let pending_asks = status.as_ref().map(wire_pending_asks).unwrap_or_default();
    let detail = SessionDetail {
        id: id.clone(),
        name: rec.spec.name.clone(),
        status: status.as_ref().map(status_kind),
        created_at: rec.created_at,
        last_error: status.as_ref().and_then(status_reason),
        pending_question: pending_asks.first().map(|a| a.question.clone()),
        pending_asks,
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
        plugins: rec.spec.plugins.clone(),
        mcp_servers: rec.spec.agent.mcp_servers.clone(),
        memory_spaces: rec.spec.agent.memory_spaces.clone(),
        use_plugins: rec.spec.agent.use_plugins.unwrap_or(false),
        thinking_effort: rec.spec.agent.thinking_effort.clone(),
        inbox: snapshot
            .map(|s| s.inbox.into_iter().map(wire_queued_message).collect())
            .unwrap_or_default(),
    };
    Ok(Json(GetSessionResponse { session: detail }))
}

/// `POST /api/sessions/:id/answers` — answer every pending ask at once.
///
/// All or nothing: a set that does not cover the pending asks exactly is a 400
/// and changes nothing. A partially answered park could not resume anyway, and
/// would leave a `tool_use` on the wire with no result.
pub async fn answer_asks(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<AnswerAsksRequest>,
) -> Result<impl IntoResponse, Api> {
    let answers: Vec<AskAnswer> = req
        .answers
        .into_iter()
        .map(|a| AskAnswer {
            tool_call_id: a.tool_call_id,
            text: a.text,
        })
        .collect();
    ask(&state, |reply| SessionSupervisorCommand::Answer {
        id: id.clone(),
        answers,
        reply,
    })
    .await?
    .map_err(|e| Api::unprocessable(e.to_string()))?;
    Ok((StatusCode::ACCEPTED, Json(Ack {})))
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
    /// Which agent's transcript to read: absent or `main` for the session's
    /// primary agent, otherwise a subagent id.
    agent_id: Option<String>,
}

fn wire_task_status(status: AgentTaskStatus) -> WireTaskStatus {
    match status {
        AgentTaskStatus::Pending => WireTaskStatus::Pending,
        AgentTaskStatus::InProgress => WireTaskStatus::InProgress,
        AgentTaskStatus::Completed => WireTaskStatus::Completed,
    }
}

fn to_wire_history(page: AgentHistoryPage) -> HistoryPage {
    let mut messages = page.messages;
    crate::wire_redact::strip_thinking_signatures(&mut messages);
    HistoryPage {
        messages,
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
        usage: page.usage.map(to_wire_usage),
    }
}

fn to_wire_usage(u: horsie_workflow::UsageTotal) -> UsageView {
    UsageView {
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        cache_creation_tokens: u.cache_creation_tokens,
        cache_read_tokens: u.cache_read_tokens,
    }
}

/// Maps the session actor's aggregated usage onto its wire shape, attaching
/// `context_window` from the model config — the one piece that isn't agent
/// state, since an agent doesn't know about configured models.
fn to_wire_usage_stats(
    stats: SessionUsageStats,
    context_window: Option<u32>,
) -> WireSessionUsageStats {
    WireSessionUsageStats {
        session_total: to_wire_usage(stats.session_total),
        main_agent: AgentUsageView {
            model: stats.main_agent.model,
            usage_total: to_wire_usage(stats.main_agent.snapshot.usage_total),
            last_turn_usage: stats.main_agent.snapshot.last_turn_usage,
            context_tokens: stats.main_agent.snapshot.context_tokens,
            context_window,
        },
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
        agent_id: params.agent_id,
        query,
        reply,
    })
    .await?
    .ok_or_else(|| Api::not_found(format!("no such session: {id}")))?;
    Ok(Json(to_wire_history(page)))
}

/// A session's aggregated usage (summed across every agent it hosts — today
/// just the one) plus the primary agent's own usage and context-size
/// snapshot, for the header's context-window display.
pub async fn get_session_usage(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, Api> {
    let stats = ask(&state, |reply| SessionSupervisorCommand::UsageStats {
        id: id.clone(),
        reply,
    })
    .await?
    .ok_or_else(|| Api::not_found(format!("no such session: {id}")))?;
    let view = state.config_store.view().await.map_err(Api::internal)?;
    let context_window = view
        .models
        .iter()
        .find(|m| m.alias == stats.main_agent.model)
        .and_then(|m| m.context_window);
    Ok(Json(GetSessionUsageResponse {
        usage: to_wire_usage_stats(stats, context_window),
    }))
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
        // Always accepted, never 409: a turn in flight queues the message and
        // answers it at the next turn boundary.
        Ok(message_id) => Ok((StatusCode::ACCEPTED, Json(SessionAck { message_id }))),
        Err(UserMessageError::NotFound) => Err(Api::not_found("no such session")),
        Err(UserMessageError::Unrecoverable(reason)) => Err(Api::conflict("unrecoverable", reason)),
    }
}

pub async fn stop_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, Api> {
    let result = ask(&state, |reply| SessionSupervisorCommand::Stop { id, reply }).await?;
    match result {
        Ok(()) => Ok(Json(Ack {})),
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
        Ok(()) => Ok(Json(Ack {})),
        Err(msg) => Err(Api::not_found(msg)),
    }
}

/// Map a storage status to its wire kind (re-exported for the SSE layer).
pub(crate) fn wire_status_kind(s: &SessionStatus) -> SessionStatusKind {
    status_kind(s)
}

/// Map one pending ask onto the wire.
pub(crate) fn wire_pending_ask(ask: &PendingAsk) -> PendingAskView {
    PendingAskView {
        tool_call_id: ask.tool_call_id.clone(),
        question: ask.question.clone(),
    }
}

/// The pending asks a status carries, or empty when it is not a park.
pub(crate) fn wire_pending_asks(status: &SessionStatus) -> Vec<PendingAskView> {
    match status {
        SessionStatus::AwaitingInput { asks } => asks.iter().map(wire_pending_ask).collect(),
        SessionStatus::Idle
        | SessionStatus::Running
        | SessionStatus::Failed { .. }
        | SessionStatus::Unrecoverable { .. } => Vec::new(),
    }
}

/// Project one tree node onto its wire shape. `output` never crosses here —
/// transcripts are read through the history endpoint with an `agent_id`.
fn to_wire_subagent(id: Uuid, rec: &SubAgentRecord) -> SubAgentView {
    SubAgentView {
        id: id.to_string(),
        parent: match rec.parent {
            SubAgentParent::Main => None,
            SubAgentParent::SubAgent(pid) => Some(pid.to_string()),
        },
        label: rec.label.clone(),
        depth: rec.depth,
        status: match rec.status {
            SubAgentStatus::Running => "running",
            SubAgentStatus::Completed => "completed",
            SubAgentStatus::Failed => "failed",
        }
        .to_string(),
        error: rec.error.clone(),
    }
}

/// A session's subagent tree, for client tree rendering. Loading the session
/// to answer is deliberate and free of sandbox cost — the tree folds from the
/// session journal, and no agent is asked.
pub async fn get_subagents(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, Api> {
    let tree = ask(&state, |reply| SessionSupervisorCommand::SubAgents {
        id: id.clone(),
        reply,
    })
    .await?
    .ok_or_else(|| Api::not_found(format!("no such session: {id}")))?;
    let subagents = tree
        .into_iter()
        .map(|(id, rec)| to_wire_subagent(id, &rec))
        .collect();
    Ok(Json(GetSessionSubAgentsResponse { subagents }))
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
    use crate::sessions::subagents::{SubAgentParent, SubAgentRecord, SubAgentStatus};

    fn record(parent: SubAgentParent, status: SubAgentStatus) -> SubAgentRecord {
        SubAgentRecord {
            parent,
            label: "research".into(),
            task: "dig".into(),
            depth: 2,
            status,
            output: Some("answer".into()),
            error: None,
            notified: true,
        }
    }

    #[test]
    fn wire_subagent_projects_the_record_without_output() {
        let parent = Uuid::new_v4();
        let id = Uuid::new_v4();
        let view = to_wire_subagent(
            id,
            &record(SubAgentParent::SubAgent(parent), SubAgentStatus::Failed),
        );
        assert_eq!(view.id, id.to_string());
        assert_eq!(view.parent, Some(parent.to_string()));
        assert_eq!(view.label, "research");
        assert_eq!(view.depth, 2);
        assert_eq!(view.status, "failed");
        // No `output` field exists on the wire type at all — transcripts are
        // read via the history endpoint, never the tree.

        let root = to_wire_subagent(id, &record(SubAgentParent::Main, SubAgentStatus::Running));
        assert_eq!(root.parent, None);
        assert_eq!(root.status, "running");
    }
}
