//! REST handlers over the `SessionSupervisor`. Bodies are fluorite wire types;
//! errors are the uniform `ApiError` envelope.

use crate::http::Scope;
use crate::http::error::Api;
use crate::sessions::UserMessageError;
use crate::sessions::builder::build_session_spec;
use crate::sessions::session_actor::{AgentEntry, AgentStatus, AskAnswer};
use crate::sessions::spec::{SessionOrigin, SessionStatus, status_kind, status_reason};
use crate::sessions::supervisor::{RenameSessionError, SessionRecord, SessionSupervisorCommand};
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
    GetSessionResponse, ListSessionsResponse, RenameSessionRequest, SendMessageRequest, SessionAck,
};
use std::collections::BTreeMap;

/// The path segment naming a session's primary agent, as opposed to a
/// subagent's uuid. Re-exported rather than spelled again: the session actor is
/// what resolves it, so this layer and that one cannot drift apart.
pub use crate::sessions::session_actor::MAIN_AGENT_ID as MAIN_AGENT;

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
    F: FnOnce(horsie_actor::ReplyTo<T>) -> SessionSupervisorCommand,
    T: Send + 'static,
{
    state
        .supervisor
        .ask(make)
        .await
        .map_err(|_| Api::internal("session supervisor unavailable"))
}

pub(crate) fn summary(id: &str, rec: &SessionRecord) -> SessionSummary {
    SessionSummary {
        id: id.to_string(),
        name: rec.spec.name.clone(),
        status: status_kind(&rec.status),
        created_at: rec.created_at,
        last_error: status_reason(&rec.status),
        workflow: rec.spec.workflow_name().map(str::to_string),
        annotations: wire_annotations(&rec.annotations),
        forks: rec.forks.iter().map(wire_fork).collect(),
    }
}

/// One fork row, for the session list.
fn wire_fork(row: &crate::sessions::supervisor::ForkRow) -> horsie_models::session::ForkView {
    horsie_models::session::ForkView {
        id: row.id.to_string(),
        parent: row.parent.map(|p| p.to_string()),
        title: row.title.clone(),
        status: wire_agent_status(row.status).to_string(),
        created_at_ms: row.created_at_ms,
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
        &state.environments,
        req.name,
        req.agent,
        req.environment,
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
        status: SessionStatus::Provisioning,
        forks: Vec::new(),
    };
    Ok((
        StatusCode::CREATED,
        Json(CreateSessionResponse {
            // A freshly created session is loaded and building its runtime,
            // with the message above already queued behind it.
            session: summary(&id, &rec),
        }),
    ))
}

/// Which sessions to list.
///
/// A run of a workflow or a routine is an ordinary session, so it is listed here
/// rather than from a second endpoint that would re-derive the same row from the
/// same registry read. Naming one is what scopes the list to it.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSessionsQuery {
    workflow: Option<String>,
    routine: Option<String>,
}

/// Every session a person started, newest first.
///
/// With neither filter a routine's runs are deliberately absent: they are listed
/// from `?routine=`, and a routine on a timer would otherwise bury the sessions
/// somebody is actually having. Workflow runs are present — they are started by
/// hand, and the row says which workflow it came from.
pub async fn list_sessions(
    Scope(state): Scope,
    Query(q): Query<ListSessionsQuery>,
) -> Result<impl IntoResponse, Api> {
    let all = ask(&state, |reply| SessionSupervisorCommand::List { reply }).await?;
    let mut sessions: Vec<_> = all
        .iter()
        .filter(|(_, rec)| match (&q.workflow, &q.routine) {
            (Some(w), _) => rec.spec.workflow_name() == Some(w.as_str()),
            (_, Some(r)) => rec.spec.routine() == Some(r.as_str()),
            _ => rec.spec.routine().is_none(),
        })
        .map(|(id, rec)| summary(id, rec))
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.created_at));
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
    Ok(Json(GetSessionResponse {
        session: detail(&id, &rec, snapshot.as_ref()),
    }))
}

/// Project one session onto its detail document.
///
/// Pure, and beside [`summary`] rather than inline in the handler, for the
/// reason that module's tests give: this layer's job is one projection and no
/// derivation, and a projection that cannot be called without a running
/// supervisor cannot be tested at the level it is written.
pub(crate) fn detail(
    id: &str,
    rec: &SessionRecord,
    snapshot: Option<&crate::sessions::session_actor::SessionSnapshot>,
) -> SessionDetail {
    // The actor's own status when it answered, the registry's copy otherwise —
    // and the registry always has one, so this document never has to say it
    // does not know. They agree except in the window where the actor has folded
    // a transition it has not reported yet, and there the actor is right.
    let status = snapshot.map_or_else(|| rec.status.clone(), |s| s.status.clone());
    SessionDetail {
        id: id.to_string(),
        name: rec.spec.name.clone(),
        status: status_kind(&status),
        created_at: rec.created_at,
        last_error: status_reason(&status),
        annotations: wire_annotations(&rec.annotations),
        model: rec.spec.agent.model.clone(),
        environment: rec.spec.environment.clone(),
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
        // Session-scoped current values, so they belong on this document rather
        // than on a history page or a separate endpoint. Both come off the same
        // snapshot as `status` — the session's actor is the only thing that
        // knows any of it, and it answers all of it at once.
        usage_total: to_wire_usage(snapshot.as_ref().map(|s| s.usage_total).unwrap_or_default()),
        agents: snapshot
            .map(|s| s.agents.iter().map(to_wire_agent).collect())
            .unwrap_or_default(),
        // The same rows a list entry carries, through the same helper. The
        // supervisor keeps them current whether or not the session actor is
        // loaded, so this costs no extra read — and reading them from `rec`
        // rather than the snapshot is what makes them available on a session
        // that is not resident.
        forks: rec.forks.iter().map(wire_fork).collect(),
        workflow: rec.spec.workflow_name().map(str::to_string),
    }
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

/// Project one of the session's agents onto its wire shape.
///
/// The whole of this layer's business with an agent. What the roster contains,
/// and what each entry's status *is*, is the session actor's — it is the only
/// thing holding the session's status, its run log and its subagent tree
/// together, and deriving any of it here meant deriving it differently in each
/// place that asked.
fn to_wire_agent(agent: &AgentEntry) -> SubAgentView {
    SubAgentView {
        id: agent.id.clone(),
        parent: agent.parent.map(|id| id.to_string()),
        label: agent.label.clone(),
        depth: agent.depth,
        agent_type: agent.agent_type.clone(),
        status: wire_agent_status(agent.status).to_string(),
        error: agent.error.clone(),
        spawned_at_ms: agent.started_at_ms,
        ended_at_ms: agent.ended_at_ms,
    }
}

/// The one spelling of an agent's status on the wire.
fn wire_agent_status(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Provisioning => "provisioning",
        AgentStatus::Running => "running",
        AgentStatus::Idle => "idle",
        AgentStatus::AwaitingInput => "awaiting_input",
        AgentStatus::Completed => "completed",
        AgentStatus::Failed => "failed",
        AgentStatus::Cancelled => "cancelled",
    }
}

fn to_wire_usage(u: crate::agent_loop::UsageTotal) -> UsageView {
    UsageView {
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        cache_creation_tokens: u.cache_creation_tokens,
        cache_read_tokens: u.cache_read_tokens,
    }
}

/// One agent's current values: what it is, what became of it, its task list and
/// its usage. Everything here is a value the client re-reads rather than a log
/// it accumulates; the log is `/history`.
pub async fn get_agent(
    Scope(state): Scope,
    Path((id, agent_id)): Path<(String, String)>,
) -> Result<impl IntoResponse, Api> {
    let detail = ask(&state, |reply| SessionSupervisorCommand::AgentDetail {
        id: id.clone(),
        agent_id: Some(agent_id.clone()),
        reply,
    })
    .await?
    .ok_or_else(|| Api::not_found(format!("no such agent: {agent_id}")))?;

    // `context_window` is the one field on this document that is not the
    // session's to answer: an agent does not know which models are configured,
    // so the HTTP layer looks up the window for the model it reports running.
    let settings = state.config_store.view().await.map_err(Api::internal)?;
    let context_window = settings
        .models
        .iter()
        .find(|m| m.alias == detail.model)
        .and_then(|m| m.context_window);

    let agent = AgentDocument {
        id: agent_id,
        parent: detail.entry.parent.map(|id| id.to_string()),
        label: detail.entry.label.clone(),
        task: detail.task,
        depth: detail.entry.depth,
        status: wire_agent_status(detail.entry.status).to_string(),
        output: detail.output,
        error: detail.entry.error,
        tasks: detail
            .state
            .tasks
            .iter()
            .map(crate::sessions::events::wire_task)
            .collect(),
        usage: to_wire_usage(detail.state.usage_total),
        last_turn_usage: detail.state.last_turn_usage,
        context_tokens: detail.state.context_tokens,
        context_window,
        as_of_seq: detail.state.as_of_seq,
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
        Ok(accepted) => Ok((
            StatusCode::ACCEPTED,
            Json(SessionAck {
                message_id: accepted.message_id,
                forked_agent: accepted.forked_agent,
            }),
        )),
        Err(UserMessageError::NotFound) => Err(Api::not_found("no such session")),
        Err(UserMessageError::Unrecoverable(reason)) => Err(Api::conflict("unrecoverable", reason)),
        Err(UserMessageError::Rejected(why)) => Err(Api::conflict("not-a-conversation", why)),
    }
}

/// `PUT /api/sessions/:id/name` — rename a session.
///
/// The agent's title tool was the only writer of a session name, so a session
/// the model never titled kept its first message as its name for good. This is
/// the other writer.
pub async fn rename_session(
    Scope(state): Scope,
    Path(id): Path<String>,
    Json(req): Json<RenameSessionRequest>,
) -> Result<impl IntoResponse, Api> {
    ask(&state, |reply| SessionSupervisorCommand::SetSessionTitle {
        id,
        name: req.name,
        reply,
    })
    .await?
    .map_err(|e| match e {
        RenameSessionError::NotFound(m) => Api::not_found(m),
        RenameSessionError::Invalid(m) => Api::unprocessable(m),
    })?;
    Ok(Json(Ack {}))
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

/// `DELETE /api/sessions/:id/agents/:agent_id` — remove one fork.
///
/// Only a fork. A subagent and a workflow step are part of what their session
/// did, and deleting one would leave a transcript referring to work with no
/// record. A fork is a conversation somebody started and can be done with.
pub async fn delete_fork(
    Scope(state): Scope,
    Path((id, agent_id)): Path<(String, String)>,
) -> Result<impl IntoResponse, Api> {
    let fork = uuid::Uuid::parse_str(&agent_id).map_err(|_| Api::not_found("no such fork"))?;
    let result = ask(&state, |reply| SessionSupervisorCommand::DeleteFork {
        id,
        fork,
        reply,
    })
    .await?;
    match result {
        Ok(()) => Ok(Json(Ack {})),
        Err(msg) => Err(Api::not_found(msg)),
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
    use uuid::Uuid;

    /// A registry row for a session that has never been forked. The two
    /// documents below are projections of this and nothing else.
    fn record_with_no_forks() -> SessionRecord {
        SessionRecord {
            spec: crate::sessions::spec::SessionSpec::for_vendor("mock"),
            created_at: 1_699_000_000_000,
            annotations: BTreeMap::new(),
            status: SessionStatus::Idle,
            forks: Vec::new(),
        }
    }

    /// This layer's whole job with an agent: one projection, no derivation. The
    /// roster it projects, and what each entry's status means, are tested where
    /// they are decided — [`crate::sessions::session_actor`].
    #[test]
    fn an_agent_crosses_the_wire_verbatim() {
        let parent = Uuid::new_v4();
        let id = Uuid::new_v4();
        let view = to_wire_agent(&AgentEntry {
            id: id.to_string(),
            parent: Some(parent),
            label: Some("research".into()),
            depth: 2,
            agent_type: Some("auditor".into()),
            status: AgentStatus::Failed,
            error: Some("boom".into()),
            started_at_ms: 100,
            ended_at_ms: 400,
        });
        assert_eq!(view.id, id.to_string());
        assert_eq!(view.parent, Some(parent.to_string()));
        assert_eq!(view.label.as_deref(), Some("research"));
        assert_eq!(view.agent_type.as_deref(), Some("auditor"));
        assert_eq!(view.depth, 2);
        assert_eq!(view.status, "failed");
        assert_eq!(view.error.as_deref(), Some("boom"));
        assert_eq!((view.spawned_at_ms, view.ended_at_ms), (100, 400));
    }

    /// A session's forks belong on its own document, not only on the list row.
    ///
    /// A client reading one session — a deep link straight to it, which is the
    /// normal case — otherwise has to fetch the entire session list to find out
    /// what branched off the thing it is already looking at.
    #[test]
    fn a_sessions_forks_are_on_its_detail_document() {
        let fork = Uuid::new_v4();
        let mut rec = record_with_no_forks();
        rec.forks = vec![crate::sessions::supervisor::ForkRow {
            id: fork,
            parent: None,
            title: Some("Other migration".into()),
            status: AgentStatus::Idle,
            created_at_ms: 1_700_000_000_000,
        }];

        let view = detail("s1", &rec, None);

        assert_eq!(view.forks.len(), 1);
        assert_eq!(view.forks[0].id, fork.to_string());
        assert_eq!(view.forks[0].title.as_deref(), Some("Other migration"));
        assert_eq!(view.forks[0].status, "idle");
        assert_eq!(view.forks[0].created_at_ms, 1_700_000_000_000);
    }

    /// The two documents that carry a session's forks must not drift: a list
    /// row and a detail read are the same fact, and a client that sees one
    /// nesting in the sidebar and another on the page has no way to tell which
    /// is wrong.
    #[test]
    fn the_list_row_and_the_detail_agree_about_forks() {
        let child = Uuid::new_v4();
        let parent = Uuid::new_v4();
        let mut rec = record_with_no_forks();
        rec.forks = vec![
            crate::sessions::supervisor::ForkRow {
                id: parent,
                parent: None,
                title: None,
                status: AgentStatus::Running,
                created_at_ms: 10,
            },
            crate::sessions::supervisor::ForkRow {
                id: child,
                parent: Some(parent),
                title: Some("deeper".into()),
                status: AgentStatus::Idle,
                created_at_ms: 20,
            },
        ];

        assert_eq!(detail("s1", &rec, None).forks, summary("s1", &rec).forks);
    }

    /// Every state has a spelling, and one spelling: a `_ =>` arm here is how
    /// the two documents that carry a status came to disagree about what a
    /// failed provision looks like.
    #[test]
    fn every_agent_status_has_one_spelling() {
        for (status, expected) in [
            (AgentStatus::Provisioning, "provisioning"),
            (AgentStatus::Running, "running"),
            (AgentStatus::Idle, "idle"),
            (AgentStatus::AwaitingInput, "awaiting_input"),
            (AgentStatus::Completed, "completed"),
            (AgentStatus::Failed, "failed"),
            (AgentStatus::Cancelled, "cancelled"),
        ] {
            assert_eq!(wire_agent_status(status), expected);
        }
    }
}
