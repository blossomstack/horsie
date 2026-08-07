//! The tree of delegated work.
//!
//! Enforces depth and concurrency, persists a spawn *before* the child actor
//! exists — a crash between the two replays as a node recovery can fail, which
//! is strictly better than an untracked agent — records terminal results, and
//! reconciles nodes a dead process left running.
//!
//! **Never asks what kind of session it is in.** Every query it makes spans the
//! whole forest, which is what makes a workflow step's subagents work through
//! the identical code path a conversation's use. The previous shape put the tree
//! inside the session's mode and every read silently answered empty for a run.

use super::component::{ActionCx, Component};
use super::context::{SessionAgentKind, SessionContextProvider, session_run_def};
use super::hooks::StopHookParent;
use super::{
    AgentAction, AgentKey, CommandEffect, SessionActor, SessionCommand, SessionDomainEvent,
    SessionState, SubAgentCommand,
};
use crate::sessions::subagents::{
    INTERRUPTED_ERROR, MAX_SUBAGENT_DEPTH, SubAgentParent, TreeOwner,
};
use horsie_actor::ActorContext;
use horsie_actor::ActorRef;
use horsie_actor::EventSourcedActor;
use horsie_models::now_ms;
use horsie_workflow::AgentActor;
use horsie_workflow::AgentCommand;
use horsie_workflow::{AgentOutcome, AgentParams, AgentRuntimeContext};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
use uuid::Uuid;

/// SubAgents.
pub(super) struct SubAgents;

impl SubAgents {
    pub(super) async fn handle(
        actor: &mut SessionActor,
        state: &SessionState,
        cmd: SubAgentCommand,
        ctx: &ActorContext<SessionActor>,
    ) -> CommandEffect<SessionDomainEvent> {
        match cmd {
            SubAgentCommand::Spawn {
                caller,
                label,
                task,
                agent_type,
                reply,
            } => {
                let owner = state.subagents.owner_for(caller, state.root_owner());
                let Some(parent_depth) = owner
                    .and_then(|owner| state.subagents.tree(owner))
                    .map_or_else(
                        // An empty forest still has a Main at depth 0: the very
                        // first spawn of a session has no tree to look in yet.
                        || matches!(caller, SubAgentParent::Main).then_some(0),
                        |tree| tree.depth_of(caller),
                    )
                else {
                    let _ = reply.send(Err("caller is not a known agent".to_string()));
                    return CommandEffect::none();
                };
                if parent_depth >= MAX_SUBAGENT_DEPTH {
                    let _ = reply.send(Err(format!(
                        "max subagent depth {MAX_SUBAGENT_DEPTH} reached"
                    )));
                    return CommandEffect::none();
                }
                let max = actor.spec.agent.max_subagents();
                if state.subagents.active_count() >= max {
                    let _ = reply.send(Err(format!("{max} subagents already active")));
                    return CommandEffect::none();
                }
                // Persist first, spawn second: a crash between the two replays
                // as a Running node with no actor, which recovery reconciles
                // to failed — never an untracked agent.
                let id = Uuid::new_v4();
                let spawned = SessionDomainEvent::SubAgentSpawned {
                    at_ms: now_ms(),
                    id,
                    parent: caller,
                    label: label.clone(),
                    task: task.clone(),
                    depth: parent_depth + 1,
                    agent_type: agent_type.clone(),
                };
                let (tx, rx) = oneshot::channel();
                let self_ref = ctx.self_ref();
                tokio::spawn(async move {
                    let persisted = rx.await.unwrap_or_else(|_| {
                        Err(horsie_actor::JournalError::Backend(
                            "spawn ack channel closed".to_string(),
                        ))
                    });
                    let _ = self_ref
                        .tell(SessionCommand::SubAgent(SubAgentCommand::FinishSpawn {
                            id,
                            label,
                            task,
                            agent_type,
                            reply,
                            persisted,
                        }))
                        .await;
                });
                CommandEffect::persist(vec![spawned]).and_ack(tx)
            }
            SubAgentCommand::FinishSpawn {
                id,
                label,
                task,
                agent_type,
                reply,
                persisted,
            } => {
                if let Err(e) = persisted {
                    let _ = reply.send(Err(format!("persist subagent: {e}")));
                    return CommandEffect::none();
                }
                let agent = actor.spawn_sub_agent_actor(ctx, id, agent_type);
                let _ = agent
                    .tell(AgentCommand::Resume {
                        results: Vec::new(),
                        message: Some(task),
                        subagent_results: Vec::new(),
                    })
                    .await;
                actor
                    .record_on(
                        AgentKey::Main,
                        horsie_agentcore::LifecycleEvent::Provisioning(
                            horsie_agentcore::ProvisioningLifecycle {
                                stage: "subagent_spawned".into(),
                                detail: Some(format!("\"{label}\" ({id})")),
                            },
                        ),
                    )
                    .await;
                let _ = reply.send(Ok(id));
                CommandEffect::none()
            }
            SubAgentCommand::Status { caller, id, reply } => {
                // Visibility is answered within the caller's own tree: a step
                // and a conversation each see their own, and neither learns the
                // other exists.
                let tree = state
                    .subagents
                    .owner_for(caller, state.root_owner())
                    .and_then(|owner| state.subagents.tree(owner));
                let rendered = match id {
                    Some(id) if tree.is_some_and(|t| t.visible_to(caller, &id)) => tree
                        .and_then(|t| t.render_node(&id))
                        .ok_or_else(|| format!("no such subagent: {id}")),
                    // Out-of-subtree and unknown ids are indistinguishable —
                    // neither confirms the node exists.
                    Some(id) => Err(format!("no such subagent: {id}")),
                    None => Ok(tree
                        .map(|t| t.render_subtree(caller))
                        .unwrap_or_else(|| "No subagents.\n".to_string())),
                };
                let _ = reply.send(rendered);
                CommandEffect::none()
            }
            SubAgentCommand::Tree { reply } => {
                // Every tree, not one: the API reports a run's step subagents
                // alongside a conversation's.
                let tree = state
                    .subagents
                    .ids()
                    .into_iter()
                    .filter_map(|id| state.subagents.node(id).map(|rec| (id, rec.clone())))
                    .collect();
                let _ = reply.send(tree);
                CommandEffect::none()
            }
            SubAgentCommand::Reconcile => {
                let interrupted = state.subagents.interrupted();
                if interrupted.is_empty() {
                    return CommandEffect::none();
                }
                CommandEffect::persist(
                    interrupted
                        .into_iter()
                        .map(|id| SessionDomainEvent::SubAgentFailed {
                            at_ms: now_ms(),
                            id,
                            error: INTERRUPTED_ERROR.to_string(),
                        })
                        .collect(),
                )
            }
        }
    }
}

/// Handlers that belong to this component but act on the actor's own
/// fields — the roster, the supervisor link, the spawn helpers. An inherent
/// `impl` in a child module sees them, so moving the code needed no plumbing.
impl SessionActor {
    /// A subagent's outcome: record it in the tree, then deliver every result
    /// owed to idle parents — wakes for subagent parents, a turn (via
    /// `drain`) when the main agent is owed and idle.
    pub(super) async fn on_sub_agent_outcome(
        &mut self,
        state: &SessionState,
        id: Uuid,
        outcome: AgentOutcome,
        ctx: &ActorContext<Self>,
    ) -> CommandEffect<SessionDomainEvent> {
        if let AgentOutcome::UsageRecorded { usage_total, .. } = outcome {
            return CommandEffect::persist(vec![SessionDomainEvent::UsageRecorded {
                at_ms: now_ms(),
                agent_id: id.to_string(),
                usage_total,
            }]);
        }
        let Some(rec) = state.subagents.node(id).cloned() else {
            tracing::warn!(subagent = %id, "outcome from an unknown subagent; ignored");
            return CommandEffect::none();
        };
        let terminal = match outcome {
            AgentOutcome::Concluded { output, .. } => {
                let text = output
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| output.to_string());
                self.record_on(
                    AgentKey::Main,
                    horsie_agentcore::LifecycleEvent::Provisioning(
                        horsie_agentcore::ProvisioningLifecycle {
                            stage: "subagent_completed".into(),
                            detail: Some(format!("\"{}\" ({id})", rec.label)),
                        },
                    ),
                )
                .await;
                SessionDomainEvent::SubAgentCompleted {
                    at_ms: now_ms(),
                    id,
                    output: text,
                }
            }
            AgentOutcome::Failed { error, .. } => {
                self.record_on(
                    AgentKey::Main,
                    horsie_agentcore::LifecycleEvent::Provisioning(
                        horsie_agentcore::ProvisioningLifecycle {
                            stage: "subagent_failed".into(),
                            detail: Some(format!("\"{}\" ({id})", rec.label)),
                        },
                    ),
                )
                .await;
                SessionDomainEvent::SubAgentFailed {
                    at_ms: now_ms(),
                    id,
                    error,
                }
            }
            // Defensive: a subagent has no ask or timer tools, so neither
            // outcome should ever occur.
            AgentOutcome::Asked { .. } => SessionDomainEvent::SubAgentFailed {
                at_ms: now_ms(),
                id,
                error: "subagent asked the user; not supported".to_string(),
            },
            AgentOutcome::Parked { .. } => SessionDomainEvent::SubAgentFailed {
                at_ms: now_ms(),
                id,
                error: "subagent parked; timers are not supported in sessions".to_string(),
            },
            AgentOutcome::UsageRecorded { .. } => unreachable!("handled above"),
        };
        let mut events = vec![terminal];
        let next = events
            .iter()
            .cloned()
            .fold(state.clone(), SessionActor::apply_event);
        events.extend(self.flush_then_drain(&next, ctx).await);
        CommandEffect::persist(events)
    }
    /// Spawn a resident subagent actor — journal replay only; the caller
    /// decides whether a run starts (spawn) or not (recovery).
    /// Spawn one subagent's actor. `agent_type` names a plugin-declared agent to
    /// run as, and travels no further than the provider: the *definition* is
    /// resolved from the library scan when the subagent runs, so an agent whose
    /// plugin was removed in between fails loudly rather than running with a
    /// prompt nobody can point at.
    pub(super) fn spawn_sub_agent_actor(
        &mut self,
        ctx: &ActorContext<Self>,
        id: Uuid,
        agent_type: Option<String>,
    ) -> ActorRef<AgentCommand> {
        let context_provider = Arc::new(SessionContextProvider {
            runtimes: self
                .deps
                .runtimes
                .provider(self.id.to_string(), self.spec.vendor.clone()),
            registry: self.deps.provider_registry.clone(),
            mcp: self.deps.mcp.clone(),
            memory: self.deps.memory.clone(),
            settings: self.spec.agent.clone(),
            step_output_schema: None,
            session_id: self.id,
            kind: SessionAgentKind::Sub(id),
            agent_type,
            unattended: self.spec.is_unattended(),
            session: ctx.self_ref(),
            plugins: self.spec.plugins.clone(),
            plugin_library: self.deps.plugins.clone(),
            last_client: Mutex::new(None),
        });
        let mut params = AgentParams::from_def(&session_run_def(&self.spec.agent));
        params.interactive = true;
        // No handoff tool: a subagent ends its turn with plain text, which
        // becomes the output its parent is notified with.
        params.thinking_effort = self
            .spec
            .agent
            .thinking_effort
            .as_deref()
            .and_then(horsie_agentcore::ThinkingEffort::parse);
        let agent_ctx = AgentRuntimeContext {
            context_provider: context_provider.clone(),
            position: self.positions.for_agent(&id.to_string()),
            parent: StopHookParent::wrap(
                ctx.self_ref(),
                AgentKey::Sub(id),
                context_provider.clone(),
            ),
            session_id: id,
        };
        let actor = ctx.spawn(AgentActor::new(agent_ctx, params));
        if let Some(agents) = self.agents.as_mut() {
            agents.insert_sub(id, actor.clone());
        }
        actor
    }
}

impl Component for SubAgents {
    /// Wake every idle parent its children owe results to. Reads the forest, so
    /// it works in a run exactly as in a conversation — and it never asks which
    /// it is in.
    fn actions(_cx: &ActionCx<'_>, state: &SessionState) -> Vec<AgentAction> {
        crate::sessions::orchestrator::wake_owed_parents(state)
    }

    /// Nodes left `Running` by a dead process. Their runs are over; the parents
    /// are owed the failure like any other terminal result.
    fn on_load(_cx: &ActionCx<'_>, state: &SessionState) -> Option<SessionCommand> {
        (!state.subagents.interrupted().is_empty())
            .then_some(SessionCommand::SubAgent(SubAgentCommand::Reconcile))
    }

    /// This is the invariant that keeps a forty-minute tool call from being
    /// unloaded out from under itself.
    fn busy(state: &SessionState) -> bool {
        state.subagents.has_active()
    }

    /// The forest. The owner is resolved from the state as it stands *before*
    /// the event, which is the step in flight for a run and `Main` otherwise.
    ///
    /// Pure, and an associated function rather than a method: replay runs with
    /// no instance in scope, which is what makes a recovered session and a live
    /// one follow the same path.
    // The fallthrough is unreachable by construction: `SessionActor::apply_event`
    // matches every variant explicitly and routes each to exactly one component,
    // so a newly added event fails to compile *there* — which is where it should
    // be classified — rather than silently reaching the wrong fold here.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn apply(state: &mut SessionState, event: &SessionDomainEvent) {
        match event.clone() {
            SessionDomainEvent::SubAgentSpawned {
                id,
                parent,
                label,
                task,
                depth,
                at_ms,
                agent_type,
            } => {
                // The owner is resolved against the state as it stands *before*
                // this event: the step in flight for a run, Main otherwise.
                let owner = state
                    .subagents
                    .owner_for(parent, state.root_owner())
                    .unwrap_or(TreeOwner::Main);
                state
                    .subagents
                    .tree_mut(owner)
                    .apply_spawned(id, parent, label, task, depth, at_ms, agent_type);
            }
            SessionDomainEvent::SubAgentRunning { id, at_ms } => {
                if let Some(owner) = state.subagents.owner_of(id) {
                    state.subagents.tree_mut(owner).apply_running(id, at_ms);
                }
            }
            SessionDomainEvent::SubAgentCompleted { id, output, at_ms } => {
                if let Some(owner) = state.subagents.owner_of(id) {
                    state
                        .subagents
                        .tree_mut(owner)
                        .apply_completed(id, output, at_ms);
                }
            }
            SessionDomainEvent::SubAgentFailed { id, error, at_ms } => {
                if let Some(owner) = state.subagents.owner_of(id) {
                    state
                        .subagents
                        .tree_mut(owner)
                        .apply_failed(id, error, at_ms);
                }
            }
            SessionDomainEvent::SubAgentNotified { id, .. } => {
                if let Some(owner) = state.subagents.owner_of(id) {
                    state.subagents.tree_mut(owner).apply_notified(id);
                }
            }
            other => unreachable!("SubAgents was handed {other:?}"),
        }
    }
}
