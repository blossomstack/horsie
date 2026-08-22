//! The agents resource: named agent presets, and invoking one into a session.

use crate::control::{
    ControlError, Expose, Method, NameRef, NoInput, Operation, Resource, ask, op,
};
use crate::http::handlers;
use crate::projects::ProjectServices;
use crate::sessions::builder::build_session_spec;
use crate::sessions::spec::{SessionOrigin, SessionStatus};
use crate::sessions::supervisor::{SessionRecord, SessionSupervisorCommand};
use horsie_models::agents::{AgentInvokeRequest, AgentInvokeResponse, AgentPresetInput, AgentView};
use horsie_models::now_ms;
use horsie_models::session::AgentSettings as WireAgentSettings;
use std::collections::BTreeMap;
use std::sync::Arc;

/// `invoke` takes its slug from the path and the rest from the body. The merge
/// in [`crate::control::http`] supplies `name` for the route; a tool passes it
/// alongside the other fields, which is why both live in one type.
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct InvokeAgent {
    /// Slug of the preset to invoke.
    pub name: String,
    #[serde(flatten)]
    pub request: AgentInvokeRequest,
}

/// Named agent presets, and invoking one into a session.
pub struct Agents;

impl Resource for Agents {
    fn name(&self) -> &'static str {
        "agents"
    }

    fn operations(&self) -> Vec<Operation> {
        vec![
            op(
                "list",
                Method::Get,
                "/agents",
                "Every saved agent preset.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, _i: NoInput| async move {
                    Ok::<Vec<AgentView>, ControlError>(s.agents.list().await?)
                },
            ),
            op(
                "get",
                Method::Get,
                "/agents/{name}",
                "One agent preset by slug.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, i: NameRef| async move {
                    Ok::<AgentView, ControlError>(s.agents.get(&i.name).await?)
                },
            ),
            op(
                "create",
                Method::Post,
                "/agents",
                "Save a new agent preset. `model` must be a configured alias — list \
             the models first if you are unsure.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, i: AgentPresetInput| async move {
                    Ok::<AgentView, ControlError>(s.agents.create(i).await?)
                },
            )
            .created(),
            op(
                "replace",
                Method::Put,
                "/agents/{name}",
                "Replace a preset wholesale. Omitted fields are reset, not kept. The \
             name is immutable — it is the id of record.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, i: AgentPresetInput| async move {
                    let name = i.name.clone();
                    Ok::<AgentView, ControlError>(s.agents.replace(&name, i).await?)
                },
            ),
            op(
                "delete",
                Method::Delete,
                "/agents/{name}",
                "Delete a preset. Refused while a routine still names it.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, i: NameRef| async move { delete(&s, &i.name).await },
            )
            .no_content(),
            op(
                "invoke",
                Method::Post,
                "/agents/{name}/invoke",
                "Create a session from a preset and queue its first message. Returns \
             as soon as both are accepted; the turn runs in the background.",
                Expose::ApiAndTool,
                |s: Arc<ProjectServices>, i: InvokeAgent| async move { invoke(&s, i).await },
            )
            .created(),
        ]
    }
}

/// Refused while a routine names this preset: a routine's whole configuration
/// is the agent it points at, so deleting one out from under it turns a
/// scheduled job into a timer that fails every firing.
async fn delete(services: &ProjectServices, name: &str) -> Result<(), ControlError> {
    let used_by = services
        .routines
        .using_agent(name)
        .await
        .map_err(|e| ControlError::Internal(e.to_string()))?;
    if !used_by.is_empty() {
        return Err(ControlError::Conflict {
            code: "agent_in_use".to_string(),
            message: format!("routines still use this agent: {}", used_by.join(", ")),
        });
    }
    services.agents.delete(name).await?;
    Ok(())
}

async fn invoke(
    services: &ProjectServices,
    input: InvokeAgent,
) -> Result<AgentInvokeResponse, ControlError> {
    let InvokeAgent { name, request } = input;
    let agent = services.agents.get(&name).await?;
    if request.message.trim().is_empty() {
        return Err(ControlError::Invalid(
            "message must not be empty".to_string(),
        ));
    }
    // The preset validated its model at save, but models are editable
    // settings — re-check so a stale preset fails here, not as a turn error.
    let view = services
        .config_store
        .view()
        .await
        .map_err(ControlError::Internal)?;
    if !view.models.iter().any(|m| m.alias == agent.model) {
        return Err(ControlError::Invalid(format!(
            "model '{}' is no longer configured",
            agent.model
        )));
    }
    let wire = WireAgentSettings {
        model: agent.model.clone(),
        allowed_tools: agent.allowed_tools.clone(),
        use_plugins: None,
        max_iterations: None,
        max_retries: None,
        mcp_servers: Some(agent.mcp_servers.clone()),
        memory_spaces: Some(agent.memory_spaces.clone()),
        thinking_effort: agent.thinking_effort.clone(),
        max_concurrent_subagents: None,
        // What the preset says about *behaviour*, as opposed to what it gates.
        instructions: agent.instructions.clone(),
        auto_compact: agent.auto_compact,
    };
    let spec = build_session_spec(
        &services.config_store,
        &services.environments,
        request.name,
        wire,
        request.environment,
        Some(agent.plugins.clone()),
        SessionOrigin::User,
    )
    .await?;
    // Checked on the *resolved* vendor, which only exists once the environment
    // has been read: a named environment carries its own.
    if !services
        .connected_vendors
        .connected_names()
        .contains(&spec.vendor)
    {
        return Err(ControlError::Invalid(format!(
            "runtime vendor '{}' is not connected",
            spec.vendor
        )));
    }
    let created_at = now_ms();
    // One ask: the create carries the first message, so the two cannot be
    // addressed separately and land on different nodes.
    let id = ask(services, |reply| SessionSupervisorCommand::Create {
        spec: spec.clone(),
        created_at,
        message: Some(request.message),
        reply,
    })
    .await?
    .map_err(super::create_failed)?
    .id;
    let rec = SessionRecord {
        spec,
        created_at,
        annotations: BTreeMap::new(),
        status: SessionStatus::Idle,
        sub_sessions: Vec::new(),
    };
    Ok(AgentInvokeResponse {
        session: handlers::summary(&id, &rec),
    })
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

    fn operations() -> Vec<Operation> {
        Agents.operations()
    }

    fn find(action: &str) -> Operation {
        operations()
            .into_iter()
            .find(|o| o.action == action)
            .unwrap()
    }

    #[test]
    fn every_action_is_declared_once_on_one_resource() {
        let mut actions: Vec<&str> = operations().iter().map(|o| o.action).collect();
        actions.sort_unstable();
        assert_eq!(
            actions,
            ["create", "delete", "get", "invoke", "list", "replace"]
        );
        assert_eq!(Agents.name(), "agents");
    }

    /// An account with one provider and one model alias, so a preset can be
    /// saved: `AgentService::create` validates its model against the config.
    async fn account() -> (Arc<ProjectServices>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::testing::state(dir.path()).build().await;
        let services = state.services().await;
        services
            .config_store
            .upsert_provider(horsie_models::settings::ProviderInput {
                name: "p".into(),
                kind: "anthropic".into(),
                base_url: Some("http://localhost:1".into()),
                api_key: Some("sk-x".into()),
                keep_thinking_signature: None,
            })
            .await
            .unwrap();
        services
            .config_store
            .upsert_model(horsie_models::settings::ModelInput {
                alias: "sonnet".into(),
                provider: "p".into(),
                model_id: "claude-sonnet-4-6".into(),
                max_tokens: None,
                context_window: None,
                thinking_efforts: None,
                thinking_effort: None,
                thinking_dialect: None,
                forced_tools_disable_thinking: None,
            })
            .await
            .unwrap();
        (services, dir)
    }

    #[tokio::test]
    async fn create_then_get_reaches_the_service() {
        let (services, _dir) = account().await;

        find("create")
            .run(
                services.clone(),
                serde_json::json!({"name": "deploy", "model": "sonnet"}),
            )
            .await
            .unwrap();

        let listed = find("list")
            .run(services.clone(), serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(listed[0]["name"], "deploy");

        let one = find("get")
            .run(services, serde_json::json!({"name": "deploy"}))
            .await
            .unwrap();
        assert_eq!(one["model"], "sonnet");
    }

    #[tokio::test]
    async fn replace_still_refuses_a_renamed_body() {
        // The path is the id of record. `merge_params` fills `name` only when
        // absent, so a caller supplying a different one still gets rejected.
        let (services, _dir) = account().await;
        find("create")
            .run(
                services.clone(),
                serde_json::json!({"name": "deploy", "model": "sonnet"}),
            )
            .await
            .unwrap();

        // What the route produces for `PUT /api/agents/deploy` with a body
        // naming something else: the body's name survives the merge.
        let err = find("replace")
            .run(
                services,
                serde_json::json!({"name": "renamed", "model": "sonnet"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ControlError::NotFound(_) | ControlError::Invalid(_)
        ));
    }

    #[tokio::test]
    async fn create_rejects_a_model_that_is_not_configured() {
        let (services, _dir) = account().await;
        let err = find("create")
            .run(
                services,
                serde_json::json!({"name": "deploy", "model": "no-such-model"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ControlError::Invalid(_)));
    }

    #[test]
    fn every_path_param_is_a_field_of_its_input() {
        // The merge in `control::http` can only fill a param the input type
        // declares; a mismatch would 422 every call to that route.
        for operation in operations() {
            for param in operation
                .path
                .split('/')
                .filter_map(|s| s.strip_prefix('{').and_then(|s| s.strip_suffix('}')))
            {
                assert!(
                    operation.schema["properties"].get(param).is_some(),
                    "{}.{} takes {{{}}} in its path but not in its input",
                    operation.resource,
                    operation.action,
                    param
                );
            }
        }
    }
}
