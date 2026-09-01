//! The provision component: the runtime and context setup every kind of work
//! shares.
//!
//! Rehydrating a suspended runtime, reconnecting MCP, scanning the workspace
//! and composing the toolbox all cross a process boundary, so the work runs on
//! a spawned task — the most hang-prone stretch of anything this agent does,
//! and the reason it selects on the cancel token. The finished [`TurnCtx`] is
//! published to the scratch, where a turn, a compaction and a summary all read
//! the same one.
//!
//! Nobody asks for this. The boundary starts it when the contexts it needs are
//! stale, and the work that follows never learns it happened.

use super::*;
use async_trait::async_trait;
use horsie_actor::CommandEffect;
use horsie_agentcore::{CompactionBudget, ToolOutcome, ToolSpec, Toolbox};
use serde_json::Value;
use std::sync::Arc;

/// Adds the component-claimed tool specs to a composed toolbox so the
/// selection filter sees the whole surface. Execution never reaches it for
/// those names — the turn routes them to their components — so its `execute`
/// for them is an error by construction.
struct WithComponentSpecs {
    inner: Arc<dyn Toolbox>,
}

#[async_trait]
impl Toolbox for WithComponentSpecs {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        specs.extend(component_tool_specs());
        specs
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<ToolOutcome, horsie_agentcore::ToolCallError> {
        if is_component_tool(name) {
            return Err(horsie_agentcore::ToolCallError::InvalidInput(format!(
                "'{name}' is handled by its component"
            )));
        }
        self.inner.execute(name, input, tool_call_id).await
    }
}

/// The runtime and context setup every kind of work shares.
pub(super) struct Provision;

impl Provision {
    /// Build this agent's contexts on a spawned task.
    pub(super) fn start(&mut self, cx: &mut Cx<'_>) {
        let (work, cancel) = cx.scratch.begin(WorkKind::Provisioning);
        let self_ref = cx.actor.self_ref();
        let context_provider = cx.runtime.context_provider.clone();
        let configured_prompt = cx.params.system_prompt.clone();
        let run_def_tools = cx.params.tools.clone();
        let thinking_effort = cx.params.thinking_effort;
        let conversation_id = cx.runtime.journal_id.to_string();
        tokio::spawn(async move {
            // Cancellable, because this is the *most* likely place to hang: it
            // awaits an MCP connect and a workspace scan across a process
            // boundary.
            let provided = tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                provided = context_provider.provide() => provided,
            };
            let outcome = provided.map(|contexts| {
                // The component specs join before the filter so the agent's
                // selection reaches them exactly as it reaches every other
                // layer. A plugin's narrowing stacks after: two filters can
                // only remove, so the narrower wins.
                let composed: Arc<dyn Toolbox> = Arc::new(WithComponentSpecs {
                    inner: contexts.toolbox,
                });
                let toolbox =
                    crate::agent_loop::FilteredToolbox::apply(composed, run_def_tools.as_deref());
                let toolbox = match &contexts.tool_narrowing {
                    None => toolbox,
                    Some(narrowed) => {
                        crate::agent_loop::FilteredToolbox::apply(toolbox, Some(narrowed))
                    }
                };
                let specs = toolbox.specs();
                let inline_names = specs
                    .iter()
                    .map(|s| s.name.clone())
                    .filter(|n| is_component_tool(n))
                    .collect();
                Box::new(TurnCtx {
                    provider: contexts.provider,
                    toolbox,
                    specs,
                    inline_names,
                    system_prompt: contexts
                        .system_prompt
                        .or(configured_prompt)
                        .unwrap_or_default(),
                    budget: contexts
                        .context_window
                        .map(|context_window| CompactionBudget {
                            context_window,
                            trigger_at_percent: COMPACT_AT_PERCENT,
                            retain_percent: COMPACT_RETAIN_PERCENT,
                        }),
                    conversation_id,
                    thinking_effort,
                    context_provider,
                })
            });
            let _ = self_ref
                .tell(AgentCommand::Provision(ProvisionCommand::Provided(
                    Box::new(ProvidedOutcome { work, outcome }),
                )))
                .await;
        });
    }
}

#[async_trait]
impl Component for Provision {
    type Command = ProvisionCommand;

    async fn handle(
        &mut self,
        cmd: ProvisionCommand,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        let ProvisionCommand::Provided(provided) = cmd;
        let ProvidedOutcome { work, outcome } = *provided;
        if !cx.scratch.finished(work) {
            return CommandEffect::none();
        }
        match outcome {
            Ok(ctx) => {
                cx.scratch.ctx = Some(std::sync::Arc::new(*ctx));
                cx.scratch.ctx_stale = false;
            }
            Err(error) => {
                // Reported from here, where the *why* is known — `terminal`
                // above all, which tells the session its sandbox is gone for
                // good rather than merely unreachable.
                //
                // The work that was waiting on these contexts is simply not
                // started: the boundary will ask again when something else
                // moves, and the next attempt provisions from scratch.
                cx.runtime
                    .parent
                    .deliver(crate::agent_loop::context::AgentOutcome::Failed {
                        agent: cx.runtime.journal_id,
                        error: error.message,
                        recoverable: true,
                        terminal: error.terminal,
                    })
                    .await;
                return CommandEffect::none();
            }
        }
        // Nothing was written, so nothing advances the agent by itself.
        cx.advance().await;
        CommandEffect::none()
    }
}
