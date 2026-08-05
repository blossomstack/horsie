//! REST handlers over the `SessionSupervisor`. Bodies are fluorite wire types;
//! errors are the uniform `ApiError` envelope.

use crate::http::AppState;
use crate::http::error::Api;
use crate::sessions::UserMessageError;
use crate::sessions::builder::build_session_spec;
use crate::sessions::session_actor::{AskAnswer, InboxMessage};
use crate::sessions::spec::{PendingAsk, SessionOrigin, SessionStatus, status_kind, status_reason};
use crate::sessions::subagents::{SubAgentParent, SubAgentRecord, SubAgentStatus};
use crate::sessions::supervisor::{SessionRecord, SessionSupervisorCommand};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use horsie_models::now_ms;
use horsie_models::session::{
    AnnotationEntry, AnswerAsksRequest, PendingAskView, QueuedMessage, SessionDetail,
    SessionStatusKind, SessionSummary, SubAgentView, UsageView,
};
use horsie_models::session_api::{
    Ack, AgentDocument, CreateSessionRequest, CreateSessionResponse, GetAgentResponse,
    GetSessionResponse, HistoryPage, ListSessionsResponse, SendMessageRequest, SessionAck,
};
use horsie_workflow::{AgentHistoryPage, HistoryQuery};
use serde::Deserialize;
use std::collections::BTreeMap;
use uuid::Uuid;

/// The path segment naming a session's primary agent, as opposed to a
/// subagent's uuid. One spelling, shared by every agent-scoped route.
pub const MAIN_AGENT: &str = "main";

/// Default and maximum messages returned by one `/history` page.
const HISTORY_DEFAULT_LIMIT: usize = 50;
const HISTORY_MAX_LIMIT: usize = 200;

/// The wire shape of one queued message. Shared with the SSE layer so the
/// detail endpoint and `InboxChanged` can never disagree about the queue.
pub fn wire_queued_message(m: InboxMessage) -> QueuedMessage {
    QueuedMessage {
        id: m.id,
        text: m.text,
        at_ms: m.at_ms,
    }
}

/// The wire shape of a session's annotations: sorted key-value pairs.
pub(crate) fn wire_annotations(annotations: &BTreeMap<String, String>) -> Vec<AnnotationEntry> {
    annotations
        .iter()
        .map(|(key, value)| AnnotationEntry {
            key: key.clone(),
            value: value.clone(),
        })
        .collect()
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
        workflow: rec.spec.workflow_name().map(str::to_string),
        annotations: wire_annotations(&rec.annotations),
    }
}

pub async fn create_session(
    State(state): State<AppState>,
    Json(req): Json<CreateSessionRequest>,
) -> Result<impl IntoResponse, Api> {
    let spec = build_session_spec(
        &state.config_store,
        req.name,
        req.agent,
        req.vendor,
        req.repos.unwrap_or_default(),
        req.plugins,
        SessionOrigin::User,
    )
    .await?;
    let created_at = now_ms();
    let id = ask(&state, |reply| SessionSupervisorCommand::Create {
        spec: spec.clone(),
        created_at,
        reply,
    })
    .await?;
    let rec = SessionRecord {
        spec,
        created_at,
        annotations: BTreeMap::new(),
    };
    Ok((
        StatusCode::CREATED,
        Json(CreateSessionResponse {
            // A freshly created session is loaded and idle; its runtime is
            // being provisioned in the background.
            session: summary(&id, &rec, Some(&SessionStatus::Idle)),
        }),
    ))
}

/// Every session a person started. A routine's runs are deliberately absent:
/// they are listed on the routine's own page, and a routine on a timer would
/// otherwise bury the sessions somebody is actually having.
pub async fn list_sessions(State(state): State<AppState>) -> Result<impl IntoResponse, Api> {
    let sessions = ask(&state, |reply| SessionSupervisorCommand::List { reply }).await?;
    let sessions = sessions
        .iter()
        .filter(|(_, rec, _)| rec.spec.routine().is_none())
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
    // Both are session-scoped current values, so they belong on this document
    // rather than on a history page or a separate endpoint.
    let usage_total = ask(&state, |reply| SessionSupervisorCommand::UsageStats {
        id: id.clone(),
        reply,
    })
    .await?
    .map(|stats| stats.session_total)
    .unwrap_or_default();
    let tree = ask(&state, |reply| SessionSupervisorCommand::SubAgents {
        id: id.clone(),
        reply,
    })
    .await?
    .unwrap_or_default();
    let agents = agent_roster(&tree);
    let detail = SessionDetail {
        id: id.clone(),
        name: rec.spec.name.clone(),
        status: status.as_ref().map(status_kind),
        created_at: rec.created_at,
        last_error: status.as_ref().and_then(status_reason),
        annotations: wire_annotations(&rec.annotations),
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
        usage_total: to_wire_usage(usage_total),
        agents,
        progression: None,
        workflow: rec.spec.workflow_name().map(str::to_string),
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

/// Query params for `GET /api/sessions/:id/agents/:agent_id/history`.
///
/// `before` and `after` are the same cursor space — a message id — read in
/// opposite directions; `after` wins if both are given.
#[derive(Deserialize)]
pub struct HistoryParams {
    /// Return the page of messages immediately before this message id; absent
    /// requests the latest (tail) page.
    before: Option<String>,
    /// Return the page of messages immediately after this message id — the
    /// forward page a reconnecting stream backfills with.
    after: Option<String>,
    /// Max messages; defaults to [`HISTORY_DEFAULT_LIMIT`], capped at
    /// [`HISTORY_MAX_LIMIT`].
    limit: Option<usize>,
}

fn to_wire_history(page: AgentHistoryPage) -> HistoryPage {
    let mut entries = page.entries;
    crate::wire_redact::strip_entry_signatures(&mut entries);
    HistoryPage {
        entries,
        has_more_before: page.has_more_before,
        has_more_after: page.has_more_after,
    }
}

/// The session's agent roster: the main agent first, then its subagent tree.
/// The main agent is listed so every agent — not just spawned ones — is
/// reachable at the same `/agents/:agent_id` shape.
fn agent_roster(tree: &[(Uuid, SubAgentRecord)]) -> Vec<SubAgentView> {
    let mut agents = vec![SubAgentView {
        id: MAIN_AGENT.to_string(),
        parent: None,
        label: None,
        depth: 0,
        agent_type: None,
        status: "running".to_string(),
        error: None,
        spawned_at_ms: 0,
        ended_at_ms: 0,
    }];
    agents.extend(tree.iter().map(|(id, rec)| to_wire_subagent(*id, rec)));
    agents
}

fn to_wire_usage(u: horsie_workflow::UsageTotal) -> UsageView {
    UsageView {
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        cache_creation_tokens: u.cache_creation_tokens,
        cache_read_tokens: u.cache_read_tokens,
    }
}

/// A window of one agent's transcript, from its in-memory state — no journal
/// replay anywhere in the server. Messages only: current values live on the
/// agent document, so a page means the same thing whichever cursor produced it.
pub async fn get_history(
    State(state): State<AppState>,
    Path((id, agent_id)): Path<(String, String)>,
    Query(params): Query<HistoryParams>,
) -> Result<impl IntoResponse, Api> {
    let limit = params
        .limit
        .unwrap_or(HISTORY_DEFAULT_LIMIT)
        .clamp(1, HISTORY_MAX_LIMIT);
    let query = HistoryQuery {
        before: params.before,
        after: params.after,
        limit,
    };
    let page = ask(&state, |reply| SessionSupervisorCommand::History {
        id: id.clone(),
        agent_id: Some(agent_id),
        query,
        reply,
    })
    .await?
    .ok_or_else(|| Api::not_found(format!("no such session: {id}")))?;
    Ok(Json(to_wire_history(page)))
}

/// One agent's current values: its task list, its usage, and — for a subagent —
/// its spawn metadata and terminal result. Everything here is a value the
/// client re-reads rather than a log it accumulates; the log is `/history`.
pub async fn get_agent(
    State(state): State<AppState>,
    Path((id, agent_id)): Path<(String, String)>,
) -> Result<impl IntoResponse, Api> {
    let view = ask(&state, |reply| SessionSupervisorCommand::AgentState {
        id: id.clone(),
        agent_id: Some(agent_id.clone()),
        reply,
    })
    .await?
    .ok_or_else(|| Api::not_found(format!("no such agent: {agent_id}")))?;

    // `context_window` is the one field here that is not agent state — an agent
    // does not know which models are configured, so the HTTP layer attaches it.
    let (rec, _) = ask(&state, |reply| SessionSupervisorCommand::Get {
        id: id.clone(),
        reply,
    })
    .await?
    .ok_or_else(|| Api::not_found(format!("no such session: {id}")))?;
    let settings = state.config_store.view().await.map_err(Api::internal)?;
    let context_window = settings
        .models
        .iter()
        .find(|m| m.alias == rec.spec.agent.model)
        .and_then(|m| m.context_window);

    // Spawn metadata comes from the session's tree, which is where a subagent's
    // lifecycle is recorded; the main agent has none of it.
    let node = if agent_id == MAIN_AGENT {
        None
    } else {
        let tree = ask(&state, |reply| SessionSupervisorCommand::SubAgents {
            id: id.clone(),
            reply,
        })
        .await?
        .unwrap_or_default();
        Uuid::parse_str(&agent_id)
            .ok()
            .and_then(|uid| tree.into_iter().find(|(nid, _)| *nid == uid))
    };

    let agent = AgentDocument {
        id: agent_id,
        parent: node.as_ref().and_then(|(_, rec)| match rec.parent {
            SubAgentParent::Main => None,
            SubAgentParent::SubAgent(pid) => Some(pid.to_string()),
        }),
        label: node.as_ref().map(|(_, rec)| rec.label.clone()),
        task: node.as_ref().map(|(_, rec)| rec.task.clone()),
        depth: node.as_ref().map_or(0, |(_, rec)| rec.depth),
        status: node.as_ref().map_or_else(
            || "running".to_string(),
            |(_, rec)| {
                match rec.status {
                    SubAgentStatus::Running => "running",
                    SubAgentStatus::Completed => "completed",
                    SubAgentStatus::Failed => "failed",
                }
                .to_string()
            },
        ),
        output: node.as_ref().and_then(|(_, rec)| rec.output.clone()),
        error: node.as_ref().and_then(|(_, rec)| rec.error.clone()),
        tasks: view
            .tasks
            .iter()
            .map(crate::sessions::events::wire_task)
            .collect(),
        usage: to_wire_usage(view.usage_total),
        last_turn_usage: view.last_turn_usage,
        context_tokens: view.context_tokens,
        context_window,
    };
    Ok(Json(GetAgentResponse { agent }))
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
        Err(UserMessageError::Rejected(why)) => Err(Api::conflict("not-a-conversation", why)),
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
        label: Some(rec.label.clone()),
        depth: rec.depth,
        agent_type: rec.agent_type.clone(),
        status: match rec.status {
            SubAgentStatus::Running => "running",
            SubAgentStatus::Completed => "completed",
            SubAgentStatus::Failed => "failed",
        }
        .to_string(),
        error: rec.error.clone(),
        spawned_at_ms: rec.spawned_at_ms,
        ended_at_ms: rec.ended_at_ms,
    }
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
            agent_type: None,
            status,
            output: Some("answer".into()),
            error: None,
            notified: true,
            spawned_at_ms: 100,
            ended_at_ms: 400,
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
        assert_eq!(view.label.as_deref(), Some("research"));
        assert_eq!(view.depth, 2);
        assert_eq!(view.status, "failed");
        // No `output` field exists on the wire type at all — transcripts are
        // read via the history endpoint, never the tree.

        let root = to_wire_subagent(id, &record(SubAgentParent::Main, SubAgentStatus::Running));
        assert_eq!(root.parent, None);
        assert_eq!(root.status, "running");
    }
}
