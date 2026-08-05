//! HTTP surface for agent presets: CRUD for the web UI and CLI, plus
//! `POST /api/agents/:name/invoke` — create a session from the preset and
//! queue the first message in one call, returning the session immediately.

use super::Scope;
use super::error::Api;
use super::handlers;
use crate::agents::AgentError;
use crate::sessions::UserMessageError;
use crate::sessions::builder::build_session_spec;
use crate::sessions::spec::{SessionOrigin, SessionStatus};
use crate::sessions::supervisor::{SessionRecord, SessionSupervisorCommand};
use axum::Json;
use axum::extract::Path;
use axum::http::StatusCode;
use horsie_models::agents::{AgentInvokeRequest, AgentInvokeResponse, AgentPresetInput, AgentView};
use horsie_models::now_ms;
use horsie_models::session::AgentSettings as WireAgentSettings;
use std::collections::BTreeMap;

/// Map the typed service error onto the envelope without string matching.
fn api_err(e: AgentError) -> Api {
    match e {
        AgentError::NotFound(m) => Api::not_found(m),
        AgentError::Conflict(m) => Api::conflict("duplicate", m),
        AgentError::Invalid(m) => Api::unprocessable(m),
        AgentError::Internal(m) => Api::internal(m),
    }
}

/// GET /api/agents
pub async fn list_agents(Scope(state): Scope) -> Result<Json<Vec<AgentView>>, Api> {
    state.agents.list().await.map(Json).map_err(api_err)
}

/// POST /api/agents
pub async fn create_agent(
    Scope(state): Scope,
    Json(input): Json<AgentPresetInput>,
) -> Result<(StatusCode, Json<AgentView>), Api> {
    state
        .agents
        .create(input)
        .await
        .map(|v| (StatusCode::CREATED, Json(v)))
        .map_err(api_err)
}

/// GET /api/agents/:name
pub async fn get_agent(
    Scope(state): Scope,
    Path(name): Path<String>,
) -> Result<Json<AgentView>, Api> {
    state.agents.get(&name).await.map(Json).map_err(api_err)
}

/// PUT /api/agents/:name — full replace; the path is the id of record.
pub async fn replace_agent(
    Scope(state): Scope,
    Path(name): Path<String>,
    Json(input): Json<AgentPresetInput>,
) -> Result<Json<AgentView>, Api> {
    state
        .agents
        .replace(&name, input)
        .await
        .map(Json)
        .map_err(api_err)
}

/// DELETE /api/agents/:name
///
/// Refused while a routine names this preset: a routine's whole configuration
/// is the agent it points at, so deleting one out from under it turns a
/// scheduled job into a timer that fails every firing.
pub async fn delete_agent(
    Scope(state): Scope,
    Path(name): Path<String>,
) -> Result<StatusCode, Api> {
    let used_by = state
        .routines
        .using_agent(&name)
        .await
        .map_err(|e| Api::internal(e.to_string()))?;
    if !used_by.is_empty() {
        return Err(Api::conflict(
            "agent_in_use",
            format!("routines still use this agent: {}", used_by.join(", ")),
        ));
    }
    state
        .agents
        .delete(&name)
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(api_err)
}

/// POST /api/agents/:name/invoke — create a session from the preset and queue
/// the message; returns as soon as both are accepted (the turn runs in the
/// background).
pub async fn invoke_agent(
    Scope(state): Scope,
    Path(name): Path<String>,
    Json(req): Json<AgentInvokeRequest>,
) -> Result<(StatusCode, Json<AgentInvokeResponse>), Api> {
    let agent = state.agents.get(&name).await.map_err(api_err)?;
    if req.message.trim().is_empty() {
        return Err(Api::unprocessable("message must not be empty"));
    }
    // A preset names no vendor: where the work runs belongs to the invocation.
    let vendor = state.config_store.default_vendor();
    if !state.vendor_agents.connected_names().contains(&vendor) {
        return Err(Api::unprocessable(format!(
            "runtime vendor '{vendor}' is not connected"
        )));
    }
    // The preset validated its model at save, but models are editable
    // settings — re-check so a stale preset fails here, not as a turn error.
    let view = state.config_store.view().await.map_err(Api::internal)?;
    if !view.models.iter().any(|m| m.alias == agent.model) {
        return Err(Api::unprocessable(format!(
            "model '{}' is no longer configured",
            agent.model
        )));
    }
    let wire = WireAgentSettings {
        model: agent.model.clone(),
        allowed_tools: None,
        use_plugins: None,
        max_iterations: None,
        max_retries: None,
        mcp_servers: Some(agent.mcp_servers.clone()),
        memory_spaces: Some(agent.memory_spaces.clone()),
        thinking_effort: agent.thinking_effort.clone(),
        max_concurrent_subagents: None,
    };
    let spec = build_session_spec(
        &state.config_store,
        req.name,
        wire,
        Some(vendor),
        agent.repos.clone(),
        Some(agent.plugins.clone()),
        SessionOrigin::User,
    )
    .await?;
    let created_at = now_ms();
    let id = handlers::ask(&state, |reply| SessionSupervisorCommand::Create {
        spec: spec.clone(),
        created_at,
        reply,
    })
    .await?;
    handlers::ask(&state, |reply| SessionSupervisorCommand::UserMessage {
        id: id.clone(),
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
        Json(AgentInvokeResponse {
            session: handlers::summary(&id, &rec, Some(&SessionStatus::Idle)),
        }),
    ))
}
