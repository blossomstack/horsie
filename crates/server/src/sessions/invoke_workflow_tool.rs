//! Server-owned tools for invoking workflows mid-session: `invoke_workflow`
//! starts a run of a saved workflow and `workflow_status` inspects one the
//! caller invoked. Both route through the owning session's mailbox — the
//! session is the one place that enforces limits, persists the run, and owns
//! the step agents.
//!
//! Layered onto every agent in a session — main, subagents, steps and forks
//! alike — carrying the *calling* agent's identity, so a run is attributed to
//! whatever invoked it and its report is delivered back there. That is the
//! whole of nesting: a workflow's step can invoke a workflow whose steps spawn
//! subagents, and each edge is the same edge.
//!
//! Resolution happens here, on the agent's own task, through the same
//! [`resolve_run_spec`] the HTTP `run` operation uses — so a run created
//! mid-session is exactly the run a request would have created, and the
//! session's mailbox never waits on a store.

use crate::sessions::addressing::SessionRef;
use crate::sessions::session_actor::{RunCommand, SessionCommand};
use crate::sessions::workflow::resolve_run_spec;
use crate::users::UserServices;
use async_trait::async_trait;
use horsie_agentcore::{ToolCallError, ToolOutcome, ToolSpec, Toolbox};
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

/// Name of the built-in workflow-invoking tool.
pub const INVOKE_WORKFLOW_TOOL: &str = "invoke_workflow";
/// Name of the built-in run-inspection tool.
pub const WORKFLOW_STATUS_TOOL: &str = "workflow_status";

fn invoke_workflow_spec() -> ToolSpec {
    ToolSpec {
        name: INVOKE_WORKFLOW_TOOL.to_string(),
        description: "Start a run of a saved workflow — a predefined graph of agent steps — \
            inside this session, sharing its workspace. Returns immediately with the run's id; \
            the run's final result or failure is automatically delivered back to you as a \
            message when it ends. Continue with independent work, or wait if none remains; do \
            not poll workflow_status. Invoking fails when the workflow does not exist or the \
            session's delegation limits (depth or live runs) are reached."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["workflow", "input"],
            "properties": {
                "workflow": {
                    "type": "string",
                    "description": "Slug of the saved workflow to run."
                },
                "input": {
                    "type": "string",
                    "description": "What the workflow's first step is handed. Complete and \
                        self-contained — the run inherits your workspace but not your \
                        conversation."
                }
            }
        }),
    }
}

fn workflow_status_spec() -> ToolSpec {
    ToolSpec {
        name: WORKFLOW_STATUS_TOOL.to_string(),
        description: "Inspect a workflow run you invoked, only for a user-requested progress \
            update or to diagnose a suspected problem. Do not poll or call this repeatedly: \
            a run's result or failure is automatically delivered to you as a message. Returns \
            the run's phase and its step log."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["run"],
            "properties": {
                "run": {
                    "type": "string",
                    "description": "A run id returned by invoke_workflow."
                }
            }
        }),
    }
}

/// Wraps an agent's toolbox, adding `invoke_workflow` and `workflow_status`.
pub struct InvokeWorkflowToolbox {
    inner: Arc<dyn Toolbox>,
    session: SessionRef,
    /// Which agent this toolbox belongs to — the invoker a run reports to.
    caller: Uuid,
    /// Where workflow definitions and presets are resolved from. Held here
    /// because resolution runs on the agent's task; the session actor never
    /// reads a store, it journals the resolved snapshot.
    services: Arc<UserServices>,
}

impl InvokeWorkflowToolbox {
    pub fn new(
        inner: Arc<dyn Toolbox>,
        session: SessionRef,
        caller: Uuid,
        services: Arc<UserServices>,
    ) -> Self {
        Self {
            inner,
            session,
            caller,
            services,
        }
    }
}

#[async_trait]
impl Toolbox for InvokeWorkflowToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        specs.push(invoke_workflow_spec());
        specs.push(workflow_status_spec());
        specs
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<ToolOutcome, ToolCallError> {
        if name == INVOKE_WORKFLOW_TOOL {
            let workflow = input
                .get("workflow")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolCallError::InvalidInput("missing 'workflow'".to_string()))?;
            let run_input = input
                .get("input")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolCallError::InvalidInput("missing 'input'".to_string()))?;
            // Resolved here, on the agent's task: every step's preset is
            // flattened into the snapshot, so a preset edited mid-run cannot
            // change a step that has not started yet — and the session's
            // mailbox is never held across a store read.
            let resolved = resolve_run_spec(&self.services, workflow, run_input)
                .await
                .map_err(|e| ToolCallError::InvalidInput(e.to_string()))?;
            let id = self
                .session
                .ask(|reply| {
                    SessionCommand::Run(RunCommand::Create {
                        parent: self.caller,
                        graph: resolved.run,
                        reply,
                    })
                })
                .await
                .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?
                .map_err(ToolCallError::ExecutionFailed)?;
            return Ok(ToolOutcome::Result(Value::String(format!(
                "Workflow run started: {id}"
            ))));
        }
        if name == WORKFLOW_STATUS_TOOL {
            let run = input
                .get("run")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolCallError::InvalidInput("missing 'run'".to_string()))?;
            let run = Uuid::parse_str(run).map_err(|_| {
                ToolCallError::InvalidInput(format!("'{run}' is not a workflow run id"))
            })?;
            let rendered = self
                .session
                .ask(|reply| {
                    SessionCommand::Run(RunCommand::Status {
                        caller: self.caller,
                        run,
                        reply,
                    })
                })
                .await
                .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?
                .map_err(ToolCallError::ExecutionFailed)?;
            return Ok(ToolOutcome::Result(Value::String(rendered)));
        }
        self.inner.execute(name, input, tool_call_id).await
    }
}
