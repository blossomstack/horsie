//! The provision component: the per-turn runtime and context setup.
//!
//! Rehydrating a suspended runtime, reconnecting MCP, scanning the workspace
//! and composing the toolbox all cross a process boundary, so the work runs on
//! a spawned task — the most hang-prone stretch of a turn, and the reason the
//! job carries the turn's cancel token. The finished [`TurnCtx`] goes back to
//! the turn as [`RunCommand::ContextReady`]; a failure is delivered to the
//! parent here (this component knows *why*) and the turn only hears
//! [`RunCommand::ContextFailed`].

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

/// The per-turn runtime and context setup.
pub(super) struct Provision;

#[async_trait]
impl Component for Provision {
    type Command = ProvisionCommand;

    async fn handle(
        &mut self,
        cmd: ProvisionCommand,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        match cmd {
            ProvisionCommand::Provide { turn } => {
                let Some(cancel) = cx.scratch.turn_cancel.clone() else {
                    return CommandEffect::none();
                };
                let self_ref = cx.actor.self_ref();
                let context_provider = cx.runtime.context_provider.clone();
                let configured_prompt = cx.params.system_prompt.clone();
                let run_def_tools = cx.params.tools.clone();
                let thinking_effort = cx.params.thinking_effort;
                let conversation_id = cx.runtime.journal_id.to_string();
                tokio::spawn(async move {
                    // Cancellable, because this is the *most* likely place to
                    // hang: it awaits an MCP connect and a workspace scan
                    // across a process boundary.
                    let provided = tokio::select! {
                        biased;
                        () = cancel.cancelled() => return,
                        provided = context_provider.provide() => provided,
                    };
                    let outcome = provided.map(|contexts| {
                        // The component specs join before the filter so the
                        // agent's selection reaches them exactly as it reaches
                        // every other layer. A plugin's narrowing stacks
                        // after: two filters can only remove, so the narrower
                        // wins.
                        let composed: Arc<dyn Toolbox> = Arc::new(WithComponentSpecs {
                            inner: contexts.toolbox,
                        });
                        let toolbox = crate::agent_loop::FilteredToolbox::apply(
                            composed,
                            run_def_tools.as_deref(),
                        );
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
                            budget: contexts.context_window.map(|context_window| {
                                CompactionBudget {
                                    context_window,
                                    trigger_at_percent: COMPACT_AT_PERCENT,
                                    retain_percent: COMPACT_RETAIN_PERCENT,
                                }
                            }),
                            conversation_id,
                            thinking_effort,
                            context_provider,
                        })
                    });
                    let _ = self_ref
                        .tell(AgentCommand::Provision(ProvisionCommand::Provided(
                            Box::new(ProvidedOutcome { turn, outcome }),
                        )))
                        .await;
                });
                CommandEffect::none()
            }
            ProvisionCommand::Provided(outcome) => {
                let ProvidedOutcome { turn, outcome } = *outcome;
                if cx.scratch.live_turn != Some(turn) {
                    return CommandEffect::none();
                }
                match outcome {
                    Ok(ctx) => {
                        // Published where every component that acts for this
                        // turn reads it; the turn only hears "ready".
                        cx.scratch.turn_ctx = Some(std::sync::Arc::new(*ctx));
                        cx.tell(AgentCommand::Run(RunCommand::ContextReady { turn }))
                            .await;
                    }
                    Err(error) => {
                        // Reported from here, where the *why* is known —
                        // `terminal` above all, which tells the session its
                        // sandbox is gone for good rather than merely
                        // unreachable. The turn only hears that it is over.
                        cx.runtime
                            .parent
                            .deliver(crate::agent_loop::context::AgentOutcome::Failed {
                                agent: cx.runtime.journal_id,
                                error: error.message,
                                recoverable: true,
                                terminal: error.terminal,
                            })
                            .await;
                        cx.tell(AgentCommand::Run(RunCommand::ContextFailed { turn }))
                            .await;
                    }
                }
                CommandEffect::none()
            }
        }
    }
}
