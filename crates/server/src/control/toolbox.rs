//! The agent-facing control plane. Executes in the server process against the
//! same services the routes use — the sandboxed runtime is never involved, like
//! `MemoryToolbox`.
//!
//! Wraps an inner toolbox rather than composing into one, so control tools sit
//! outside `FilteredToolbox` and a session that sets `allowed_tools` does not
//! silently lose them. The preset's checkbox is the only gate.
//!
//! Specs are static: `CompositeToolbox::execute` calls `specs()` on every box
//! for every tool call, so nothing here may touch the database.

use crate::control::{Expose, Operation};
use crate::users::UserServices;
use async_trait::async_trait;
use horsie_agentcore::{ToolCallError, ToolOutcome, ToolSpec, Toolbox};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Namespaced so a control tool cannot collide with a runtime tool, an MCP tool
/// or a plugin's — they all share one name space on the wire.
const PREFIX: &str = "horsie_";

pub struct ControlToolbox {
    inner: Arc<dyn Toolbox>,
    services: Arc<UserServices>,
    /// resource -> action -> operation, built once at spawn.
    by_resource: BTreeMap<&'static str, BTreeMap<&'static str, Operation>>,
}

impl ControlToolbox {
    pub fn new(
        inner: Arc<dyn Toolbox>,
        services: Arc<UserServices>,
        operations: Vec<Operation>,
    ) -> Self {
        let mut by_resource: BTreeMap<&'static str, BTreeMap<&'static str, Operation>> =
            BTreeMap::new();
        for operation in operations.into_iter().filter(|o| o.expose != Expose::Api) {
            by_resource
                .entry(operation.resource)
                .or_default()
                .insert(operation.action, operation);
        }
        Self {
            inner,
            services,
            by_resource,
        }
    }

    /// One line per resource for the system prompt, so the model's first call is
    /// a real one rather than a guess. A few hundred tokens for the whole
    /// control plane, against re-sending every schema in every request.
    pub fn command_index(&self) -> String {
        self.by_resource
            .iter()
            .map(|(resource, actions)| {
                format!(
                    "{resource} {{{}}}",
                    actions.keys().copied().collect::<Vec<_>>().join(",")
                )
            })
            .collect::<Vec<_>>()
            .join(" · ")
    }

    /// One tool per resource: `action` picks the operation, and each `oneOf`
    /// branch pins that action to a const and carries the operation's own
    /// derived schema. Nothing here is hand-written.
    fn spec(resource: &str, actions: &BTreeMap<&'static str, Operation>) -> ToolSpec {
        ToolSpec {
            name: format!("{PREFIX}{resource}"),
            description: format!(
                "Manage {resource} on this horsie server. Changes take effect \
                 immediately.\n\nActions:\n{}",
                actions
                    .values()
                    .map(|o| format!("- {}: {}", o.action, o.summary))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
            input_schema: json!({
                "type": "object",
                "required": ["action"],
                "properties": {
                    "action": {
                        "description": "Which operation to run.",
                        "enum": actions.keys().copied().collect::<Vec<_>>(),
                    }
                },
                "oneOf": actions
                    .values()
                    .map(|o| json!({
                        "properties": { "action": { "const": o.action } },
                        "allOf": [o.schema],
                    }))
                    .collect::<Vec<_>>(),
            }),
        }
    }
}

#[async_trait]
impl Toolbox for ControlToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        specs.extend(
            self.by_resource
                .iter()
                .map(|(resource, actions)| Self::spec(resource, actions)),
        );
        specs
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<ToolOutcome, ToolCallError> {
        let Some(actions) = name
            .strip_prefix(PREFIX)
            .and_then(|resource| self.by_resource.get(resource))
        else {
            return self.inner.execute(name, input, tool_call_id).await;
        };
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ToolCallError::InvalidInput(format!(
                    "'action' is required; available: {}",
                    actions.keys().copied().collect::<Vec<_>>().join(", ")
                ))
            })?
            .to_string();
        let operation = actions.get(action.as_str()).ok_or_else(|| {
            ToolCallError::InvalidInput(format!(
                "no action '{action}'; available: {}",
                actions.keys().copied().collect::<Vec<_>>().join(", ")
            ))
        })?;
        // `action` stays in the input: every operation's input type ignores
        // unknown fields, and stripping it would mean copying the object for
        // every call to no end.
        //
        // Always `Result`, never `StopRun`: managing the server is something an
        // agent does *during* a turn, never the thing that ends one.
        operation
            .run(self.services.clone(), input)
            .await
            .map(ToolOutcome::Result)
            .map_err(Into::into)
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
    use horsie_agentcore::EmptyToolbox;

    async fn toolbox() -> (ControlToolbox, Arc<UserServices>, tempfile::TempDir) {
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
        let tb = ControlToolbox::new(
            Arc::new(EmptyToolbox),
            services.clone(),
            crate::control::operations(),
        );
        (tb, services, dir)
    }

    #[tokio::test]
    async fn one_tool_per_resource_carrying_every_action() {
        let (tb, _services, _dir) = toolbox().await;
        let specs = tb.specs();
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"horsie_agents"), "{names:?}");
        assert!(names.contains(&"horsie_routines"), "{names:?}");
        assert!(names.contains(&"horsie_environments"), "{names:?}");

        let agents = specs.iter().find(|s| s.name == "horsie_agents").unwrap();
        let actions = agents.input_schema["properties"]["action"]["enum"]
            .as_array()
            .unwrap();
        assert_eq!(actions.len(), 6);
        assert_eq!(agents.input_schema["required"][0], "action");

        let branches = agents.input_schema["oneOf"].as_array().unwrap();
        assert_eq!(branches.len(), 6);
        assert!(
            branches
                .iter()
                .any(|b| b["properties"]["action"]["const"] == "create"),
            "each branch pins its own action"
        );
        assert!(
            branches
                .iter()
                .any(|b| b["allOf"][0]["properties"].get("model").is_some()),
            "a branch carries its operation's derived schema"
        );
    }

    #[tokio::test]
    async fn specs_extend_the_inner_box_rather_than_replacing_it() {
        // A session that sets `allowed_tools` must not lose these, which is why
        // this wraps instead of composing.
        //
        // Counted against the table rather than a literal: a new resource is
        // meant to appear here, and a magic number would fail every time one
        // does while saying nothing about what actually broke.
        let (tb, _services, _dir) = toolbox().await;
        let resources: std::collections::BTreeSet<&str> = crate::control::operations()
            .iter()
            .filter(|o| o.expose != Expose::Api)
            .map(|o| o.resource)
            .collect();
        assert_eq!(
            tb.specs().len(),
            resources.len(),
            "one tool per resource, and EmptyToolbox contributes nothing"
        );
    }

    #[tokio::test]
    async fn a_tool_call_reaches_the_service() {
        let (tb, services, _dir) = toolbox().await;
        tb.execute(
            "horsie_agents",
            json!({"action": "create", "name": "deploy", "model": "sonnet"}),
            "tc1",
        )
        .await
        .unwrap()
        .expect_value();
        assert_eq!(services.agents.get("deploy").await.unwrap().name, "deploy");
    }

    #[tokio::test]
    async fn an_unknown_action_names_the_ones_that_exist() {
        let (tb, _services, _dir) = toolbox().await;
        let err = tb
            .execute("horsie_agents", json!({"action": "explode"}), "tc1")
            .await
            .unwrap_err();
        let ToolCallError::InvalidInput(message) = err else {
            panic!("expected invalid input");
        };
        assert!(message.contains("explode"), "{message}");
        assert!(message.contains("invoke"), "{message}");
    }

    #[tokio::test]
    async fn a_missing_action_says_so() {
        let (tb, _services, _dir) = toolbox().await;
        let err = tb
            .execute("horsie_agents", json!({}), "tc1")
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn an_unrelated_tool_falls_through_to_the_inner_box() {
        let (tb, _services, _dir) = toolbox().await;
        // EmptyToolbox answers everything with "no tool named …", which is how
        // we know the call was forwarded rather than swallowed here.
        let err = tb.execute("read_file", json!({}), "tc1").await.unwrap_err();
        let ToolCallError::InvalidInput(message) = err else {
            panic!("expected invalid input");
        };
        assert!(message.contains("read_file"), "{message}");
    }

    #[tokio::test]
    async fn a_service_rejection_comes_back_as_something_the_model_can_act_on() {
        let (tb, _services, _dir) = toolbox().await;
        let err = tb
            .execute(
                "horsie_agents",
                json!({"action": "create", "name": "deploy", "model": "no-such-model"}),
                "tc1",
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn the_command_index_names_every_resource_and_action() {
        let (tb, _services, _dir) = toolbox().await;
        let index = tb.command_index();
        assert!(index.contains("agents {"), "{index}");
        assert!(index.contains("invoke"), "{index}");
        assert!(index.contains("routines {"), "{index}");
    }
}
