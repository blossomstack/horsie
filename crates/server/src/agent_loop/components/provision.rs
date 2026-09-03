//! The provision component: the runtime and context setup every kind of work
//! shares.
//!
//! Rehydrating a suspended runtime, reconnecting MCP, scanning the workspace
//! and composing the toolbox all cross a process boundary, so the work runs on
//! a spawned task — the most hang-prone stretch of anything this agent does,
//! and the reason it selects on the cancel token. The finished [`TurnCtx`] is
//! published to the step_run, where a turn, a compaction and a summary all read
//! the same one.
//!
//! Nobody asks for this. The boundary starts it when the contexts it needs are
//! stale, and the work that follows never learns it happened.

use crate::agent_loop::prelude::*;
use crate::agent_loop::shared::summarise::{COMPACT_AT_PERCENT, COMPACT_RETAIN_PERCENT};
use async_trait::async_trait;
use horsie_actor::CommandEffect;
use horsie_agentcore::{CompactionBudget, Toolbox};
use std::sync::Arc;

/// The runtime and context setup every kind of work shares.
pub(crate) struct Provision;

impl Provision {
    /// Build this agent's contexts on a spawned task. `vended` is every
    /// toolbox the actor collected from its stateful components.
    pub(crate) fn start(
        &mut self,
        marker_seq: u64,
        initializing: bool,
        vended: Vec<Arc<dyn Toolbox>>,
        cx: &mut Cx<'_>,
    ) {
        let cancel = cx.step_run.begin(
            if initializing {
                StepPhase::Initialize
            } else {
                StepPhase::Connect
            },
            marker_seq,
        );
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
                provided = async {
                    match initializing {
                        true => context_provider.provide().await,
                        false => context_provider.reconnect().await,
                    }
                } => provided,
            };
            let outcome = provided.map(|contexts| {
                // The component toolboxes join before the filter, so the
                // agent's tool selection reaches them exactly as it reaches
                // every other layer — and ahead of the runtime's, so a
                // component tool wins a name collision. A plugin's narrowing
                // stacks after: two filters can only remove, so the narrower
                // wins.
                let mut boxes = vended;
                boxes.push(contexts.toolbox);
                let composed: Arc<dyn Toolbox> =
                    Arc::new(crate::agent_loop::shared::mcp_toolbox::CompositeToolbox::new(boxes));
                let toolbox =
                    crate::agent_loop::FilteredToolbox::apply(composed, run_def_tools.as_deref());
                let toolbox = match &contexts.tool_narrowing {
                    None => toolbox,
                    Some(narrowed) => {
                        crate::agent_loop::FilteredToolbox::apply(toolbox, Some(narrowed))
                    }
                };
                let specs = toolbox.specs();
                Box::new(TurnCtx {
                    provider: contexts.provider,
                    toolbox,
                    specs,
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
                    Box::new(ProvidedOutcome {
                        marker_seq,
                        initializing,
                        outcome,
                    }),
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
        let ProvidedOutcome {
            marker_seq,
            initializing,
            outcome,
        } = *provided;
        let marker_is_open = cx.state.open_step().is_some_and(|(seq, kind)| {
            seq == marker_seq
                && matches!(
                    (kind, initializing),
                    (StepKind::Initialize, true) | (StepKind::Connect, false)
                )
        });
        if !marker_is_open || !cx.step_run.finished(marker_seq) {
            return CommandEffect::none();
        }
        match outcome {
            Ok(mut turn_ctx) => {
                let events = if initializing {
                    let mut events = Vec::new();
                    if !turn_ctx.system_prompt.is_empty() {
                        events.push(AgentDomainEvent::SystemPromptRecorded {
                            source: SystemPromptSource::InitialContext,
                            content: turn_ctx.system_prompt.clone(),
                        });
                    }
                    events.push(AgentDomainEvent::AgentInitialized);
                    events
                } else {
                    turn_ctx.system_prompt = cx.state.system_prompt();
                    vec![AgentDomainEvent::ConnectionCompleted]
                };
                cx.step_run.ctx = Some(std::sync::Arc::new(*turn_ctx));
                cx.step_run.ctx_stale = false;
                CommandEffect::persist(events)
            }
            Err(error) => {
                let at_ms = horsie_models::now_ms();
                CommandEffect::persist(vec![
                    AgentDomainEvent::StepFailed {
                        reason: StepFailure::Provider(error.message.clone()),
                    },
                    AgentDomainEvent::RunEnded {
                        reason: RunEnd::Failed {
                            error: error.message,
                            recoverable: true,
                            terminal: error.terminal,
                        },
                        at_ms,
                    },
                ])
            }
        }
    }
}
