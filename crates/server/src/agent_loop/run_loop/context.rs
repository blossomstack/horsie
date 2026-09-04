//! Initialization and reconnection, each as a fenced foreground step.
//!
//! Initialization provisions the agent, scans the workspace, composes the
//! immutable system prompt, and returns a durable manifest. Reconnection uses
//! that manifest to rebuild live runtime and MCP clients without repeating
//! semantic initialization.

use crate::agent_loop::prelude::*;
use crate::agent_loop::shared::summarise::{COMPACT_AT_PERCENT, COMPACT_RETAIN_PERCENT};
use horsie_actor::CommandEffect;
use horsie_agentcore::{CompactionBudget, Toolbox};
use std::sync::Arc;

#[derive(Clone, Copy)]
enum ContextMode {
    Initialize,
    Reconnect,
}

pub(crate) fn initialize(
    marker_seq: u64,
    toolboxes: Vec<Arc<dyn Toolbox>>,
    cx: &mut CommandContext<'_>,
) {
    start(ContextMode::Initialize, marker_seq, toolboxes, cx);
}

pub(crate) fn reconnect(
    marker_seq: u64,
    toolboxes: Vec<Arc<dyn Toolbox>>,
    cx: &mut CommandContext<'_>,
) {
    start(ContextMode::Reconnect, marker_seq, toolboxes, cx);
}

fn start(
    mode: ContextMode,
    marker_seq: u64,
    vended: Vec<Arc<dyn Toolbox>>,
    cx: &mut CommandContext<'_>,
) {
    let cancel = match mode {
        ContextMode::Initialize => cx.step_run.begin_initialization(marker_seq),
        ContextMode::Reconnect => cx.step_run.begin_connection(marker_seq),
    };
    let self_ref = cx.actor.self_ref();
    let context_provider = cx.runtime.context_provider.clone();
    let configured_prompt = cx.params.system_prompt.clone();
    let run_def_tools = cx.params.tools.clone();
    let thinking_effort = cx.params.thinking_effort;
    let conversation_id = cx.runtime.journal_id.to_string();
    let manifest = cx.state.context_manifest().cloned();
    tokio::spawn(async move {
        // Cancellable, because this is the *most* likely place to hang: it
        // awaits an MCP connect and a workspace scan across a process
        // boundary.
        let provided = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            provided = async {
                match mode {
                    ContextMode::Initialize => context_provider.provide().await,
                    ContextMode::Reconnect => match &manifest {
                        Some(manifest) => context_provider.reconnect(manifest).await,
                        None => Err(crate::agent_loop::ContextError::terminal(
                            "initialized agent has no durable context manifest",
                        )),
                    },
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
            Box::new(ExecutionContext {
                provider: contexts.provider,
                manifest: contexts.manifest,
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
        let ready = Box::new(ContextReady {
            marker_seq,
            outcome,
        });
        let command = match mode {
            ContextMode::Initialize => ContextCommand::InitializationReady(ready),
            ContextMode::Reconnect => ContextCommand::ConnectionReady(ready),
        };
        let _ = self_ref.tell(AgentCommand::Context(command)).await;
    });
}

pub(crate) async fn handle(
    cmd: ContextCommand,
    cx: &mut CommandContext<'_>,
) -> CommandEffect<AgentDomainEvent> {
    let (mode, ready) = match cmd {
        ContextCommand::InitializationReady(ready) => (ContextMode::Initialize, ready),
        ContextCommand::ConnectionReady(ready) => (ContextMode::Reconnect, ready),
    };
    let ContextReady {
        marker_seq,
        outcome,
    } = *ready;
    let marker_is_open = cx.state.open_step().is_some_and(|(seq, kind)| {
        seq == marker_seq
            && matches!(
                (kind, mode),
                (StepKind::Initialize, ContextMode::Initialize)
                    | (StepKind::Connect, ContextMode::Reconnect)
            )
    });
    let live_step_finished = match mode {
        ContextMode::Initialize => cx.step_run.finish_initialization(marker_seq),
        ContextMode::Reconnect => cx.step_run.finish_connection(marker_seq),
    };
    if !marker_is_open || !live_step_finished {
        return CommandEffect::none();
    }
    match outcome {
        Ok(mut execution_context) => {
            let events = match mode {
                ContextMode::Initialize => {
                    let mut events = Vec::new();
                    if !execution_context.system_prompt.is_empty() {
                        events.push(AgentDomainEvent::SystemPromptRecorded {
                            source: SystemPromptSource::InitialContext,
                            content: execution_context.system_prompt.clone(),
                        });
                    }
                    events.push(AgentDomainEvent::AgentInitialized {
                        manifest: execution_context.manifest.clone(),
                    });
                    events
                }
                ContextMode::Reconnect => {
                    execution_context.system_prompt = cx.state.system_prompt();
                    vec![AgentDomainEvent::ConnectionCompleted]
                }
            };
            cx.step_run.execution = Some(std::sync::Arc::new(*execution_context));
            cx.step_run.reconnect_required = false;
            CommandEffect::persist(events)
        }
        Err(error) => {
            let at_ms = horsie_models::now_ms();
            let mut events = Vec::new();
            if let Some(pending) = cx.state.next_input()
                && !pending.consumed().is_empty()
            {
                events.push(AgentDomainEvent::Consumed {
                    ids: pending.consumed().to_vec(),
                    at_ms,
                });
            }
            events.extend([
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
            ]);
            CommandEffect::persist(events)
        }
    }
}
