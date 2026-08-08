//! REST handlers over the `SessionSupervisor`. Bodies are fluorite wire types;
//! errors are the uniform `ApiError` envelope.

use crate::http::Scope;
use crate::http::error::Api;
use crate::sessions::UserMessageError;
use crate::sessions::builder::build_session_spec;
use crate::sessions::session_actor::AskAnswer;
use crate::sessions::spec::{SessionOrigin, SessionStatus, status_kind, status_reason};
use crate::sessions::subagents::{SubAgentParent, SubAgentRecord, SubAgentStatus};
use crate::sessions::supervisor::{SessionRecord, SessionSupervisorCommand};
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use horsie_models::now_ms;
use horsie_models::session::{
    AnnotationEntry, AnswerAsksRequest, SessionDetail, SessionSummary, SubAgentView, UsageView,
};
use horsie_models::session_api::{
    Ack, AgentDocument, CreateSessionRequest, CreateSessionResponse, GetAgentResponse,
    GetSessionResponse, ListSessionsResponse, SendMessageRequest, SessionAck,
};
use std::collections::BTreeMap;
use uuid::Uuid;

/// The path segment naming a session's primary agent, as opposed to a
/// subagent's uuid. One spelling, shared by every agent-scoped route.
pub const MAIN_AGENT: &str = "main";

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
pub(crate) async fn ask<T, F>(state: &crate::users::UserServices, make: F) -> Result<T, Api>
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

/// Create a session and queue its first message, in that order and in one
/// call.
///
/// The message is not optional. A create-only route provisioned a runtime for
/// a session nobody had said anything to, and nothing reclaimed it: the idle
/// sweep only walks *loaded* session actors, and a session that never received
/// a message never loads. Every other way a session comes into being — an agent
/// preset, a workflow run, a routine — already creates and messages together;
/// this makes that the only shape.
pub async fn create_session(
    Scope(state): Scope,
    Json(req): Json<CreateSessionRequest>,
) -> Result<impl IntoResponse, Api> {
    if req.message.trim().is_empty() {
        return Err(Api::unprocessable("message must not be empty"));
    }
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
    // Queued, not run: the runtime is still provisioning behind this call, and
    // the agent's own queue is what holds the message until it is there. The
    // agent is not ready yet, so this returns once the message is durable and
    // the create's completion is what releases it.
    ask(&state, |reply| SessionSupervisorCommand::UserMessage {
        id: id.clone(),
        agent_id: None,
        text: req.message,
        reply,
    })
    .await?
    .map_err(|e| match e {
        UserMessageError::NotFound => Api::not_found("no such session"),
        UserMessageError::Unrecoverable(reason) => Api::conflict("unrecoverable", reason),
        UserMessageError::Rejected(why) => Api::conflict("not-a-conversation", why),
    })?;
    let rec = SessionRecord {
        spec,
        created_at,
        annotations: BTreeMap::new(),
    };
    Ok((
        StatusCode::CREATED,
        Json(CreateSessionResponse {
            // A freshly created session is loaded and building its runtime,
            // with the message above already queued behind it.
            session: summary(&id, &rec, Some(&SessionStatus::Provisioning)),
        }),
    ))
}

/// Every session a person started. A routine's runs are deliberately absent:
/// they are listed on the routine's own page, and a routine on a timer would
/// otherwise bury the sessions somebody is actually having.
pub async fn list_sessions(Scope(state): Scope) -> Result<impl IntoResponse, Api> {
    let sessions = ask(&state, |reply| SessionSupervisorCommand::List { reply }).await?;
    let sessions = sessions
        .iter()
        .filter(|(_, rec, _)| rec.spec.routine().is_none())
        .map(|(id, rec, status)| summary(id, rec, status.as_ref()))
        .collect();
    Ok(Json(ListSessionsResponse { sessions }))
}

pub async fn get_session(
    Scope(state): Scope,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, Api> {
    let (rec, snapshot) = ask(&state, |reply| SessionSupervisorCommand::Get {
        id: id.clone(),
        reply,
    })
    .await?
    .ok_or_else(|| Api::not_found(format!("no such session: {id}")))?;
    let status = snapshot.as_ref().map(|s| s.status.clone());
    // Session-scoped current values, so they belong on this document rather
    // than on a history page or a separate endpoint.
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
        usage_total: to_wire_usage(usage_total),
        agents,
        progression: None,
        workflow: rec.spec.workflow_name().map(str::to_string),
    };
    Ok(Json(GetSessionResponse { session: detail }))
}

/// Which agent a write is addressed to. Absent or `"main"` for the session's
/// primary agent, else a subagent or workflow-step agent id — the same
/// vocabulary the read path uses.
#[derive(serde::Deserialize)]
pub struct AgentParam {
    aid: Option<String>,
}

/// `POST /api/sessions/:id/answers?aid=` — answer every question one agent is
/// parked on, at once.
///
/// All or nothing: a set that does not cover that agent's questions exactly is
/// a 400 and changes nothing. A partially answered park could not resume
/// anyway, and would leave a `tool_use` on the wire with no result.
pub async fn answer_asks(
    Scope(state): Scope,
    Path(id): Path<String>,
    Query(agent): Query<AgentParam>,
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
        agent_id: agent.aid,
        answers,
        reply,
    })
    .await?
    .map_err(|e| Api::unprocessable(e.to_string()))?;
    Ok((StatusCode::ACCEPTED, Json(Ack {})))
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

/// One agent's current values: its task list, its usage, and — for a subagent —
/// its spawn metadata and terminal result. Everything here is a value the
/// client re-reads rather than a log it accumulates; the log is `/history`.
pub async fn get_agent(
    Scope(state): Scope,
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
        as_of_seq: view.as_of_seq,
    };
    Ok(Json(GetAgentResponse { agent }))
}

/// `POST /api/sessions/:id/messages?aid=` — send a message to one agent.
///
/// Returns only once the message is durably in that agent's queue, so a client
/// holding a `202` holds a promise that survives a crash.
pub async fn send_message(
    Scope(state): Scope,
    Path(id): Path<String>,
    Query(agent): Query<AgentParam>,
    Json(req): Json<SendMessageRequest>,
) -> Result<impl IntoResponse, Api> {
    let result = ask(&state, |reply| SessionSupervisorCommand::UserMessage {
        id,
        agent_id: agent.aid,
        text: req.text,
        reply,
    })
    .await?;
    match result {
        // Always accepted, never 409: a turn in flight queues the message and
        // the agent answers it at its next turn boundary.
        Ok(message_id) => Ok((StatusCode::ACCEPTED, Json(SessionAck { message_id }))),
        Err(UserMessageError::NotFound) => Err(Api::not_found("no such session")),
        Err(UserMessageError::Unrecoverable(reason)) => Err(Api::conflict("unrecoverable", reason)),
        Err(UserMessageError::Rejected(why)) => Err(Api::conflict("not-a-conversation", why)),
    }
}

pub async fn stop_session(
    Scope(state): Scope,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, Api> {
    let result = ask(&state, |reply| SessionSupervisorCommand::Stop { id, reply }).await?;
    match result {
        Ok(()) => Ok(Json(Ack {})),
        Err(msg) => Err(Api::not_found(msg)),
    }
}

pub async fn delete_session(
    Scope(state): Scope,
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
