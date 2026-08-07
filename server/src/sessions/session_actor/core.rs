//! The session's own bookkeeping: its title, and turn-preparation progress.
//!
//! A component like the rest, but the one whose slice is the session itself
//! rather than a feature. It owns `UsageRecorded`, because all three
//! agent-owning components record into `agent_usage` and the total belongs to
//! none of them.

use super::CoreCommand;
use super::component::Component;
use super::{
    AgentKey, CommandEffect, SessionActor, SessionAgents, SessionDomainEvent, SessionState,
};
use crate::sessions::supervisor::SessionSupervisorCommand;
use crate::sessions::title_tool::normalize_session_title;
use horsie_actor::ActorContext;
use horsie_models::now_ms;
use horsie_workflow::AgentCommand;

/// SessionCore.
pub(super) struct SessionCore;

impl SessionCore {
    pub(super) async fn handle(
        actor: &mut SessionActor,
        _state: &SessionState,
        cmd: CoreCommand,
        _ctx: &ActorContext<SessionActor>,
    ) -> CommandEffect<SessionDomainEvent> {
        match cmd {
            CoreCommand::SetTitle { title, reply } => {
                let result = match normalize_session_title(&title) {
                    Ok(title) => actor.rename_session(title).await,
                    Err(error) => Err(error.to_string()),
                };
                let _ = reply.send(result);
                CommandEffect::none()
            }
            CoreCommand::Progress { key, stage, detail } => {
                actor
                    .record_on(
                        key,
                        horsie_agentcore::LifecycleEvent::Provisioning(
                            horsie_agentcore::ProvisioningLifecycle { stage, detail },
                        ),
                    )
                    .await;
                CommandEffect::none()
            }
        }
    }
}

/// Handlers that belong to this component but act on the actor's own
/// fields — the roster, the supervisor link, the spawn helpers. An inherent
/// `impl` in a child module sees them, so moving the code needed no plumbing.
impl SessionActor {
    /// Persist a session title through the supervisor, then publish it.
    pub(super) async fn rename_session(&mut self, title: String) -> Result<String, String> {
        let id = self.id.to_string();
        let persisted = self
            .parent
            .ask(|reply| SessionSupervisorCommand::RenameSession {
                id: id.clone(),
                name: title.clone(),
                reply,
            })
            .await
            .map_err(|e| format!("session supervisor unavailable: {e}"))?;
        persisted.map_err(|e| format!("persist session title: {e}"))?;

        self.spec.name = Some(title.clone());
        let _ = self
            .parent
            .tell(SessionSupervisorCommand::PublishSessionTitle {
                id,
                name: title.clone(),
            })
            .await;
        Ok(title)
    }
    /// Tell each agent about the session events it needs to show.
    ///
    /// This is the whole of "session events reach the client": the session
    /// still owns and journals every one of them, and hands the viewer-facing
    /// subset to the agent whose log a person would be reading. One direction
    /// only — an agent never tells the session anything back through here.
    ///
    /// **Resident agents only**, because this hook has no `ActorContext` to
    /// spawn with. That is not the limitation it looks like: `main` is spawned
    /// at recovery and stays for the session's loaded life, and every
    /// subagent-targeted event happens while that subagent is running. A miss
    /// is therefore a bug worth hearing about rather than a case to handle,
    /// which is what the warning is for.
    pub(super) async fn record_lifecycle(&mut self, events: &[SessionDomainEvent]) {
        for event in events {
            let (target, Some(payload)) = crate::sessions::lifecycle_routing::route(event) else {
                continue;
            };
            let agent = match &target {
                crate::sessions::lifecycle_routing::LifecycleTarget::None => continue,
                crate::sessions::lifecycle_routing::LifecycleTarget::Main => {
                    self.agents.as_ref().and_then(SessionAgents::main).cloned()
                }
                crate::sessions::lifecycle_routing::LifecycleTarget::Agent(AgentKey::Main) => {
                    self.agents.as_ref().and_then(SessionAgents::main).cloned()
                }
                crate::sessions::lifecycle_routing::LifecycleTarget::Agent(AgentKey::Sub(id))
                | crate::sessions::lifecycle_routing::LifecycleTarget::Agent(AgentKey::Step(id)) => {
                    self.agents.as_ref().and_then(|a| a.sub(*id)).cloned()
                }
            };
            let Some(agent) = agent else {
                tracing::warn!(
                    session = %self.id,
                    ?target,
                    "no resident agent to record a session event on; it will be missing from the log"
                );
                continue;
            };
            let _ = agent
                .tell(AgentCommand::RecordLifecycle {
                    event: payload,
                    at_ms: now_ms(),
                })
                .await;
        }
    }
    /// Record one lifecycle entry on a named agent, when it is resident.
    pub(super) async fn record_on(
        &mut self,
        key: AgentKey,
        event: horsie_agentcore::LifecycleEvent,
    ) {
        let agent = match key {
            AgentKey::Main => self.agents.as_ref().and_then(SessionAgents::main).cloned(),
            AgentKey::Sub(id) | AgentKey::Step(id) => {
                self.agents.as_ref().and_then(|a| a.sub(id)).cloned()
            }
        };
        if let Some(agent) = agent {
            let _ = agent
                .tell(AgentCommand::RecordLifecycle {
                    event,
                    at_ms: now_ms(),
                })
                .await;
        }
    }
}

impl Component for SessionCore {
    /// Banked usage. Core-owned because all three agent-owning components
    /// record into it and the total belongs to none of them.
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
            SessionDomainEvent::UsageRecorded {
                agent_id,
                usage_total,
                ..
            } => {
                state.agent_usage.insert(agent_id, usage_total);
            }
            other => unreachable!("SessionCore was handed {other:?}"),
        }
    }
}
