//! The session's own bookkeeping: its title, and turn-preparation progress.
//!
//! A component like the rest, but the one whose slice is the session itself
//! rather than a feature. It owns `UsageRecorded`, because all three
//! agent-owning components record into `agent_usage` and the total belongs to
//! none of them.

use super::CoreCommand;
use super::component::Component;
use super::{AgentKey, CommandEffect, SessionActor, SessionDomainEvent, SessionState};
use crate::sessions::supervisor::SessionSupervisorCommand;
use crate::sessions::title_tool::normalize_session_title;
use horsie_actor::ActorContext;
use horsie_models::now_ms;
use horsie_workflow::AgentCommand;

/// Longest auto-derived session title, in characters.
const TITLE_MAX_CHARS: usize = crate::sessions::title_tool::SESSION_TITLE_MAX_CHARS;

/// A short title derived from a user's first message.
fn derive_title(text: &str) -> Option<String> {
    let first_line = text.lines().next().unwrap_or("").trim();
    if first_line.is_empty() {
        return None;
    }
    if first_line.chars().count() <= TITLE_MAX_CHARS {
        return Some(first_line.to_string());
    }
    let truncated: String = first_line.chars().take(TITLE_MAX_CHARS).collect();
    Some(format!("{}…", truncated.trim_end()))
}

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
    /// Title an as-yet-unnamed session from the first thing the user says.
    ///
    /// Best-effort and fire-and-forget: a session that could not be titled is
    /// still a session, so a failure is logged and the message goes on to start
    /// its turn. The built-in title tool overwrites this later if the agent
    /// picks something better.
    pub(super) async fn title_from_first_message(&mut self, text: &str) {
        if self.spec.name.is_some() {
            return;
        }
        let Some(title) = derive_title(text) else {
            return;
        };
        if let Err(error) = self.rename_session(title).await {
            tracing::warn!(session = %self.id, error, "failed to persist fallback session title");
        }
    }

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
    /// spawn with. That is not the limitation it looks like: a conversation's
    /// `main` is spawned at recovery and stays for the session's loaded life, a
    /// run's step agent is live for as long as its step is, and every
    /// subagent-targeted event happens while that subagent is running. A miss is
    /// therefore a bug worth hearing about rather than a case to handle, which
    /// is what the warning is for — an event with genuinely nowhere to go routes
    /// to nothing at all, and never reaches this loop.
    ///
    /// Note the state it routes against: `on_events_persisted` is called once
    /// per batch, with the state the *whole* batch folded to. Two of the
    /// routings read that state, so an event is placed by where the batch ended
    /// rather than by where it itself sat.
    pub(super) async fn record_lifecycle(
        &mut self,
        events: &[SessionDomainEvent],
        state: &SessionState,
    ) {
        for event in events {
            for (key, payload) in crate::sessions::lifecycle_routing::route(event, state) {
                let Some(agent) = self.agents.as_ref().and_then(|a| a.get(key)).cloned() else {
                    tracing::warn!(
                        session = %self.id,
                        ?key,
                        "no resident agent to record a session event on; it will be missing from the log"
                    );
                    continue;
                };
                let _ = agent
                    .actor
                    .tell(AgentCommand::RecordLifecycle {
                        event: payload,
                        at_ms: now_ms(),
                    })
                    .await;
            }
        }
    }
    /// Record one lifecycle entry on a named agent, when it is resident.
    pub(super) async fn record_on(
        &mut self,
        key: AgentKey,
        event: horsie_agentcore::LifecycleEvent,
    ) {
        let agent = self.agents.as_ref().and_then(|a| a.get(key)).cloned();
        if let Some(agent) = agent {
            let _ = agent
                .actor
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// The fallback title: the first line, elided to fit. It exists so a
    /// session is never nameless in the list while the agent is still working
    /// out what to call it.
    #[test]
    fn a_title_is_derived_from_the_first_line_only() {
        assert_eq!(derive_title("hello\nworld").as_deref(), Some("hello"));
        assert!(derive_title("   \n").is_none());
        let long = "x".repeat(TITLE_MAX_CHARS + 10);
        let title = derive_title(&long).unwrap();
        assert!(title.ends_with('…'));
        assert_eq!(title.chars().count(), TITLE_MAX_CHARS + 1);
    }
}
