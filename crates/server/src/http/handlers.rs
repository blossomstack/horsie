//! REST handlers over the `SessionSupervisor`. Bodies are fluorite wire types;
//! errors are the uniform `ApiError` envelope.

use crate::http::error::Api;
use crate::http::{Scope, Scoped};
use crate::sessions::UserMessageError;
use crate::sessions::builder::{AgentChoice, build_session_spec};
use crate::sessions::session_actor::{AgentEntry, AskAnswer};
use crate::sessions::spec::{SessionOrigin, SessionStatus, status_kind, status_reason};
use crate::sessions::supervisor::{SessionRecord, SessionSupervisorCommand};
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use horsie_models::now_ms;
use horsie_models::session::{
    AgentStats, AnnotationEntry, AnswerAsksRequest, SessionDetail, SessionSummary, SubAgentView,
    SubSessionView, UsageView,
};
use horsie_models::session_api::{
    Ack, AgentDocument, CreateSessionRequest, CreateSessionResponse, GetAgentResponse,
    SendMessageRequest, SessionAck,
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

/// Every built-in tool this server offers, grouped for selection.
///
/// Static per build — the catalogue is a table, not a discovery pass — so this
/// takes no state and touches nothing. MCP tools are deliberately absent: they
/// are chosen by selecting the server, and their names do not exist until
/// something has been connected to. See [`crate::tools`].
pub async fn tool_catalog() -> impl IntoResponse {
    Json(crate::tools::catalog())
}

/// Liveness, and — on a clustered node — readiness.
///
/// A node that has stood down reports 503 here so a load balancer drains it,
/// rather than discovering it one request at a time. Unclustered, this is
/// always 200: a single node never stands down.
pub async fn health(
    axum::extract::State(state): axum::extract::State<crate::http::AppState>,
) -> impl IntoResponse {
    let ready = state.shared.serving.as_ref().is_none_or(|rx| *rx.borrow());
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(serde_json::json!({ "ok": ready })))
}

/// Ask the supervisor a question, mapping a closed mailbox to a 500.
///
/// The HTTP rendering of [`crate::control::ask`], which both surfaces share —
/// including its 503 for a node that has stood down.
pub(crate) async fn ask<T, F>(state: &crate::projects::ProjectServices, make: F) -> Result<T, Api>
where
    F: FnOnce(horsie_actor::ReplyTo<T>) -> SessionSupervisorCommand,
    T: Send + 'static,
{
    crate::control::ask(state, make).await.map_err(Api::from)
}

pub(crate) fn summary(id: &str, rec: &SessionRecord) -> SessionSummary {
    SessionSummary {
        id: id.to_string(),
        name: rec.name.clone(),
        status: status_kind(&rec.status),
        created_at: rec.created_at,
        last_error: status_reason(&rec.status),
        workflow: rec.spec.workflow_name().map(str::to_string),
        annotations: wire_annotations(&rec.annotations),
        sub_sessions: rec
            .sub_sessions
            .iter()
            .map(crate::sessions::supervisor::SubSessionRow::to_view)
            .collect(),
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
    let name = req.name;
    let spec = build_session_spec(
        &state.config_store,
        &state.environments,
        // The sessions API takes settings inline; nothing named a preset, so
        // claiming one would be an invention.
        AgentChoice::ad_hoc(req.agent),
        req.environment,
        req.plugins,
        SessionOrigin::User,
    )
    .await?;
    let created_at = now_ms();
    // One ask, not two. Queued, not run: the runtime is still provisioning
    // behind this call, and the agent's own queue is what holds the message
    // until it is there. The agent is not ready yet, so this returns once the
    // message is durable and the create's completion is what releases it.
    let created = ask(&state, |reply| SessionSupervisorCommand::Create {
        spec: spec.clone(),
        name: name.clone(),
        created_at,
        message: Some(req.message),
        reply,
    })
    .await?
    .map_err(Api::from)?;
    let id = created.id;
    let rec = SessionRecord {
        spec,
        name,
        created_at,
        annotations: BTreeMap::new(),
        status: SessionStatus::Provisioning,
        sub_sessions: Vec::new(),
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
    windows: &ContextWindows,
) -> SessionDetail {
    // The actor's own status when it answered, the registry's copy otherwise —
    // and the registry always has one, so this document never has to say it
    // does not know. They agree except in the window where the actor has folded
    // a transition it has not reported yet, and there the actor is right.
    let status = snapshot.map_or_else(|| rec.status.clone(), |s| s.status.clone());
    SessionDetail {
        id: id.to_string(),
        name: rec.name.clone(),
        status: status_kind(&status),
        created_at: rec.created_at,
        last_error: status_reason(&status),
        annotations: wire_annotations(&rec.annotations),
        environment: rec.spec.environment().map(str::to_string),
        // A session that runs without a sandbox names no vendor. The wire field
        // stays required and reads empty, which is what every client already
        // renders as "nothing to show".
        vendor: rec.spec.vendor().unwrap_or_default().to_string(),
        repos: rec
            .spec
            .runtime
            .iter()
            .flat_map(|r| r.provision.iter())
            .filter(|s| s.uses == "git_checkout")
            .filter_map(|s| {
                s.with
                    .iter()
                    .find(|(k, _)| k == "url")
                    .map(|(_, v)| v.clone())
            })
            .collect(),
        plugins: rec.spec.plugins.clone(),
        // Session-scoped current values, so they belong on this document rather
        // than on a history page or a separate endpoint. Both come off the same
        // snapshot as `status` — the session's actor is the only thing that
        // knows any of it, and it answers all of it at once.
        usage_total: to_wire_usage(snapshot.as_ref().map(|s| s.usage_total).unwrap_or_default()),
        agents: snapshot
            .map(|s| s.agents.iter().map(|a| to_wire_agent(a, windows)).collect())
            .unwrap_or_default(),
        // From the actor when there is one, because only the actor can answer
        // for a sub session's numbers and its brief. The supervisor's rows are
        // the fallback: they are kept current whether or not the session is
        // resident, so a session nobody has loaded still lists what it hosts —
        // just without the figures nothing has been asked for.
        sub_sessions: match snapshot {
            Some(s) => s
                .sub_sessions
                .iter()
                .map(|sub| to_wire_sub_session(sub, windows))
                .collect(),
            None => rec
                .sub_sessions
                .iter()
                .map(crate::sessions::supervisor::SubSessionRow::to_view)
                .collect(),
        },
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
    Scoped(id): Scoped<String>,
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
fn to_wire_agent(agent: &AgentEntry, windows: &ContextWindows) -> SubAgentView {
    SubAgentView {
        id: agent.id.clone(),
        parent: agent.parent.map(|id| id.to_string()),
        title: agent.title.clone(),
        kind: wire_kind(agent.kind).to_string(),
        input: agent.input.clone(),
        output: agent.output.clone(),
        stats: to_wire_stats(&agent.stats, agent.model.as_deref(), windows),
        depth: agent.depth,
        agent_type: agent.agent_type.clone(),
        preset: agent.preset.clone(),
        status: agent.status.as_wire().to_string(),
        error: agent.error.clone(),
        spawned_at_ms: agent.started_at_ms,
        ended_at_ms: agent.ended_at_ms,
    }
}

/// One sub session, as its session's own document reports it — the registry
/// row plus what only the session actor holds.
fn to_wire_sub_session(
    sub: &crate::sessions::session_actor::SubSessionEntry,
    windows: &ContextWindows,
) -> SubSessionView {
    SubSessionView {
        id: sub.id.to_string(),
        parent: sub.parent.map(|p| p.to_string()),
        title: sub.title.clone(),
        status: sub.status.as_wire().to_string(),
        created_at_ms: sub.created_at_ms,
        last_activity_ms: sub.last_activity_ms,
        input: Some(sub.input.clone()),
        stats: Some(to_wire_stats(&sub.stats, sub.model.as_deref(), windows)),
    }
}

/// Model alias → the context window that model allows. What the session actor
/// cannot answer, because which models are configured is not its business.
pub(crate) type ContextWindows = std::collections::HashMap<String, u32>;

/// Every configured model's context window, by alias.
///
/// Read once per document rather than once per agent: a session with thirty
/// agents would otherwise read the settings thirty times to answer the same
/// question. A model with no window configured is simply absent, and the
/// agents running it report no denominator.
pub(crate) async fn context_windows(
    config: &std::sync::Arc<dyn crate::config::ConfigStore>,
) -> Result<ContextWindows, String> {
    Ok(config
        .view()
        .await?
        .models
        .into_iter()
        .filter_map(|m| m.context_window.map(|w| (m.alias, w)))
        .collect())
}

fn wire_kind(kind: crate::sessions::session_actor::AgentKind) -> &'static str {
    use crate::sessions::session_actor::AgentKind as K;
    match kind {
        K::Main => "main",
        K::Sub => "subagent",
        K::Step => "step",
        K::SubSession => "sub_session",
    }
}

fn to_wire_stats(
    stats: &crate::sessions::session_actor::AgentStats,
    model: Option<&str>,
    windows: &ContextWindows,
) -> AgentStats {
    AgentStats {
        usage: to_wire_usage(stats.usage),
        subtree_usage: to_wire_usage(stats.subtree_usage),
        context_tokens: stats.context_tokens,
        context_window: model.and_then(|m| windows.get(m).copied()),
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
    // Two of this route's own, so the project is written out: `Path`
    // deserializes a flat sequence and `Scoped` only drops one segment.
    Path((_project, id, agent_id)): Path<(String, String, String)>,
) -> Result<impl IntoResponse, Api> {
    let detail = ask(&state, |reply| SessionSupervisorCommand::AgentDetail {
        id: id.clone(),
        agent_id: Some(agent_id.clone()),
        reply,
    })
    .await?
    .ok_or_else(|| Api::not_found(format!("no such agent: {agent_id}")))?;

    // The agent's own configuration — model, MCP, memory, plugins, thinking —
    // rides on its document, because it is per-agent: a workflow step's is its
    // own preset's, never the session's. `context_window` is the one field the
    // actor cannot answer (it does not know which models are configured), so
    // the HTTP layer looks up the window for the model the document reports.
    let settings = state.config_store.view().await.map_err(Api::internal)?;
    let context_window = settings
        .models
        .iter()
        .find(|m| m.alias == detail.settings.model)
        .and_then(|m| m.context_window);

    let agent = AgentDocument {
        id: agent_id,
        parent: detail.entry.parent.map(|id| id.to_string()),
        title: detail.entry.title.clone(),
        task: detail.task,
        depth: detail.entry.depth,
        status: detail.entry.status.as_wire().to_string(),
        output: detail.output,
        error: detail.entry.error,
        model: detail.settings.model,
        mcp_servers: detail.settings.mcp_servers,
        memory_spaces: detail.settings.memory_spaces,
        allowed_tools: detail.settings.allowed_tools,
        use_plugins: detail.settings.use_plugins.unwrap_or(false),
        thinking_effort: detail.settings.thinking_effort,
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
    Scoped(id): Scoped<String>,
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
                sub_session: accepted.sub_session,
            }),
        )),
        Err(UserMessageError::NotFound) => Err(Api::not_found("no such session")),
        Err(UserMessageError::Unrecoverable(reason)) => Err(Api::conflict("unrecoverable", reason)),
        Err(UserMessageError::Rejected(why)) => Err(Api::conflict("not-a-session", why)),
    }
}

/// `DELETE /api/sessions/:id/agents/:agent_id` — remove one agent a session
/// hosts, and everything below it.
///
/// A subagent's run or a sub session. Not the main agent, which *is* the
/// session — deleting that is `DELETE /sessions/:id` — and not a workflow
/// step, which belongs to its run's log rather than to whoever is reading it.
/// The session decides which of those an id names; this layer only carries the
/// answer back.
pub async fn delete_agent(
    Scope(state): Scope,
    // Two of this route's own, so the project is written out: `Path`
    // deserializes a flat sequence and `Scoped` only drops one segment.
    Path((_project, id, agent_id)): Path<(String, String, String)>,
) -> Result<impl IntoResponse, Api> {
    let agent = uuid::Uuid::parse_str(&agent_id).map_err(|_| Api::not_found("no such agent"))?;
    let result = ask(&state, |reply| SessionSupervisorCommand::DeleteAgent {
        id,
        agent,
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
    use crate::sessions::session_actor::AgentStatus;
    use uuid::Uuid;

    /// A registry row for a session that has never been branched. The two
    /// documents below are projections of this and nothing else.
    fn record_with_no_sub_sessions() -> SessionRecord {
        SessionRecord {
            spec: crate::sessions::spec::SessionSpec::for_vendor("mock"),
            name: None,
            created_at: 1_699_000_000_000,
            annotations: BTreeMap::new(),
            status: SessionStatus::Idle,
            sub_sessions: Vec::new(),
        }
    }

    /// This layer's whole job with an agent: one projection, no derivation. The
    /// roster it projects, and what each entry's status means, are tested where
    /// they are decided — [`crate::sessions::session_actor`].
    #[test]
    fn an_agent_crosses_the_wire_verbatim() {
        let parent = Uuid::new_v4();
        let id = Uuid::new_v4();
        let view = to_wire_agent(
            &AgentEntry {
                id: id.to_string(),
                parent: Some(parent),
                title: Some("research".into()),
                depth: 2,
                agent_type: Some("auditor".into()),
                status: AgentStatus::Failed,
                error: Some("boom".into()),
                kind: crate::sessions::session_actor::AgentKind::Sub,
                input: Some("audit the deps".into()),
                output: None,
                stats: crate::sessions::session_actor::AgentStats {
                    context_tokens: 4_000,
                    ..Default::default()
                },
                model: Some("m".into()),
                preset: Some("reviewer".into()),
                started_at_ms: 100,
                ended_at_ms: 400,
            },
            &ContextWindows::from([("m".to_string(), 200_000)]),
        );
        assert_eq!(view.id, id.to_string());
        assert_eq!(view.parent, Some(parent.to_string()));
        assert_eq!(view.title.as_deref(), Some("research"));
        assert_eq!(view.kind, "subagent");
        assert_eq!(view.input.as_deref(), Some("audit the deps"));
        // The window is the HTTP layer's own contribution: the actor names the
        // model, and only this layer knows what that model allows.
        assert_eq!(view.stats.context_tokens, 4_000);
        assert_eq!(view.stats.context_window, Some(200_000));
        assert_eq!(view.agent_type.as_deref(), Some("auditor"));
        // The two are different questions and are carried separately: a typed
        // subagent has an `agent_type` from a plugin and a `preset` it
        // inherited, and collapsing them would file it under the wrong one.
        assert_eq!(view.preset.as_deref(), Some("reviewer"));
        assert_eq!(view.depth, 2);
        assert_eq!(view.status, "failed");
        assert_eq!(view.error.as_deref(), Some("boom"));
        assert_eq!((view.spawned_at_ms, view.ended_at_ms), (100, 400));
    }

    /// A session's sub sessions belong on its own document, not only on the
    /// list row.
    ///
    /// A client reading one session — a deep link straight to it, which is the
    /// normal case — otherwise has to fetch the entire session list to find out
    /// what branched off the thing it is already looking at.
    #[test]
    fn a_sessions_sub_sessions_are_on_its_detail_document() {
        let sub_session = Uuid::new_v4();
        let mut rec = record_with_no_sub_sessions();
        rec.sub_sessions = vec![crate::sessions::supervisor::SubSessionRow {
            id: sub_session,
            parent: None,
            title: "Other migration".into(),
            status: AgentStatus::Idle,
            created_at_ms: 1_700_000_000_000,
            last_activity_ms: 1_700_000_000_000,
        }];

        let view = detail("s1", &rec, None, &ContextWindows::new());

        assert_eq!(view.sub_sessions.len(), 1);
        assert_eq!(view.sub_sessions[0].id, sub_session.to_string());
        assert_eq!(view.sub_sessions[0].title, "Other migration");
        assert_eq!(view.sub_sessions[0].status, "idle");
        assert_eq!(view.sub_sessions[0].created_at_ms, 1_700_000_000_000);
    }

    /// The two documents that carry a session's sub sessions must not drift: a
    /// list row and a detail read are the same fact, and a client that sees
    /// one nesting in the sidebar and another on the page has no way to tell
    /// which is wrong.
    #[test]
    fn the_list_row_and_the_detail_agree_about_sub_sessions() {
        let child = Uuid::new_v4();
        let parent = Uuid::new_v4();
        let mut rec = record_with_no_sub_sessions();
        rec.sub_sessions = vec![
            crate::sessions::supervisor::SubSessionRow {
                id: parent,
                parent: None,
                title: "the first branch".into(),
                status: AgentStatus::Running,
                created_at_ms: 10,
                last_activity_ms: 10,
            },
            crate::sessions::supervisor::SubSessionRow {
                id: child,
                parent: Some(parent),
                title: "deeper".into(),
                status: AgentStatus::Idle,
                created_at_ms: 20,
                last_activity_ms: 20,
            },
        ];

        assert_eq!(
            detail("s1", &rec, None, &ContextWindows::new()).sub_sessions,
            summary("s1", &rec).sub_sessions
        );
    }

    /// The session document makes no session-wide model claim any more: the
    /// model and per-agent configuration live on the agent document, because a
    /// workflow run's steps each carry their own. Asserted on the JSON shape,
    /// which is the contract a client reads.
    #[test]
    fn the_detail_document_carries_no_session_wide_agent_configuration() {
        let view = detail(
            "s1",
            &record_with_no_sub_sessions(),
            None,
            &ContextWindows::new(),
        );
        let json = serde_json::to_value(&view).unwrap();
        for key in [
            "model",
            "mcp_servers",
            "memory_spaces",
            "use_plugins",
            "thinking_effort",
        ] {
            assert!(
                json.get(key).is_none(),
                "the session document must not claim a session-wide {key}"
            );
        }
    }
}
