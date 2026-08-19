//! Server-owned tools for running workflows from inside a session:
//! `invoke_workflow` starts a run and `workflow_status` inspects the caller's
//! runs.
//!
//! Layered onto every agent in a session, main and sub alike — nesting is the
//! point — carrying the *calling* agent's identity so runs are attributed to
//! the right parent and their results delivered back to it.
//!
//! Resolution happens here, on the agent's own task: turning a workflow name
//! into a run snapshot reads stores, and store I/O must never run on the
//! session's mailbox. The session receives a resolved graph and journals it;
//! a name that resolves to nothing is an ordinary tool error that never
//! touches the session.

use crate::sessions::addressing::SessionRef;
use crate::sessions::session_actor::{AgentId, RunCommand, SessionCommand};
use crate::sessions::workflow::{ResolveError, resolve_run_spec};
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
        description: "Run a saved workflow by name, inside this session's own workspace. \
            Returns immediately with the run's id; the run proceeds one step at a time in \
            parallel with your work, and its final result or failure is automatically \
            delivered back to you as a message. Continue with independent work, or wait if \
            none remains; do not poll workflow_status or call it repeatedly. Invoking fails \
            when the session's nesting limits (depth or live runs) are reached."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "workflow": {
                    "type": "string",
                    "description": "The slug of a saved workflow definition."
                },
                "input": {
                    "type": "string",
                    "description": "The complete, self-contained input handed to the \
                        workflow's first step. The steps inherit nothing from your \
                        conversation — include everything they need to know."
                }
            },
            "required": ["workflow", "input"]
        }),
    }
}

fn workflow_status_spec() -> ToolSpec {
    ToolSpec {
        name: WORKFLOW_STATUS_TOOL.to_string(),
        description: "Inspect the workflow runs you have invoked in this session. Use only \
            when the user requests a progress update or to diagnose a suspected problem — \
            results are delivered to you automatically."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "run_id": {
                    "type": "string",
                    "description": "A run id returned by invoke_workflow. Omit to list \
                        every run you invoked."
                }
            }
        }),
    }
}

/// Wraps an agent's toolbox, adding `invoke_workflow` and `workflow_status`.
pub struct InvokeWorkflowToolbox {
    inner: Arc<dyn Toolbox>,
    session: SessionRef,
    /// Which agent this toolbox belongs to — the parent runs attribute to,
    /// and the agent their results are delivered to.
    caller: AgentId,
    /// Where a workflow name is resolved. Held here because resolution is
    /// store I/O, and this toolbox executes on the agent's own task.
    services: Arc<UserServices>,
}

impl InvokeWorkflowToolbox {
    pub fn new(
        inner: Arc<dyn Toolbox>,
        session: SessionRef,
        caller: AgentId,
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
            // Resolved here, on this agent's task: a name that means nothing is
            // this tool call's error, and the session never hears about it.
            let resolved = resolve_run_spec(&self.services, workflow, run_input)
                .await
                .map_err(|e| match e {
                    ResolveError::NotFound(m) | ResolveError::Invalid(m) => {
                        ToolCallError::InvalidInput(m)
                    }
                    ResolveError::Internal(m) => ToolCallError::ExecutionFailed(m),
                })?;
            let graph = (*resolved.run).clone();
            let id = self
                .session
                .ask(|reply| {
                    SessionCommand::Run(RunCommand::CreateRun {
                        parent: self.caller,
                        graph: Box::new(graph),
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
            let id = input
                .get("run_id")
                .and_then(Value::as_str)
                .map(|s| {
                    Uuid::parse_str(s)
                        .map_err(|_| ToolCallError::InvalidInput(format!("'{s}' is not a run id")))
                })
                .transpose()?;
            let rendered = self
                .session
                .ask(|reply| {
                    SessionCommand::Run(RunCommand::Status {
                        caller: self.caller,
                        id,
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
