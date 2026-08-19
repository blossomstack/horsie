//! The session's own bookkeeping: its title, its spec, its root runner, and
//! turn-preparation progress.

use super::runner::event::{RunnerArgs, RunnerEvent};
use super::{
    AgentId, CommandEffect, CoreCommand, RunnerId, SessionActor, SessionEvent, SessionState,
};
use crate::agent_loop::AgentCommand;
use crate::sessions::addressing::SessionInbox;
use crate::sessions::spec::SessionKind;
use crate::sessions::supervisor::SessionSupervisorCommand;
use crate::sessions::title_tool::normalize_session_title;
use horsie_actor::ActorContext;
use horsie_models::now_ms;

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

impl SessionActor {
    pub(super) async fn handle_core(
        &mut self,
        state: &SessionState,
        cmd: CoreCommand,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionEvent> {
        match cmd {
            CoreCommand::SetTitle { title, reply } => {
                let result = match normalize_session_title(&title) {
                    Ok(title) => self.rename_session(title).await,
                    Err(error) => Err(error.to_string()),
                };
                // Journal the name this session now answers to, but only if it
                // actually took: a rejected title must not be recorded as one.
                let effect = match result.as_ref() {
                    Ok(name) => {
                        CommandEffect::persist(vec![SessionEvent::Renamed { name: name.clone() }])
                    }
                    Err(_) => CommandEffect::none(),
                };
                let _ = reply.send(result);
                effect
            }
            CoreCommand::TitleSet { name } => {
                self.spec_mut().name = Some(name.clone());
                CommandEffect::persist(vec![SessionEvent::Renamed { name }])
            }
            CoreCommand::RecordSpec { spec } => {
                // Idempotent, because a log that already says what this
                // session is has said it: a later one would overwrite a name
                // the session has since been given, and recovery has already
                // adopted what was there.
                if state.spec.is_some() {
                    return CommandEffect::none();
                }
                // The other half of how a session learns what it is. Recovery
                // covers a session with a history; this covers the one case it
                // cannot — a session created a moment ago, whose log is empty
                // and whose agents nothing else would ever start.
                self.adopt((*spec).clone(), state, ctx).await;
                CommandEffect::persist(vec![SessionEvent::SpecRecorded { spec }])
            }
            CoreCommand::CreateRoot => {
                // Once: replaying, or two adopts racing, must not seed twice.
                if state.root().is_some() {
                    return CommandEffect::none();
                }
                let args = match &self.spec().kind {
                    SessionKind::Agent { .. } => RunnerArgs::Main,
                    // The graph is snapshotted into the runner's own record,
                    // so a root run and a nested one are self-contained the
                    // same way.
                    SessionKind::Workflow { run } => RunnerArgs::Workflow {
                        graph: (**run).clone(),
                    },
                };
                let created = SessionEvent::Runner {
                    id: RunnerId(self.id),
                    at_ms: now_ms(),
                    event: RunnerEvent::Created {
                        parent: None,
                        args: Box::new(args),
                    },
                };
                // Through the boundary: a workflow root's first step starts
                // the moment the record exists — gated, as everything is, on
                // the sandbox being ready.
                self.persist_and_advance(state, vec![created], ctx).await
            }
            CoreCommand::Progress {
                agent,
                stage,
                detail,
            } => {
                self.record_on(
                    agent,
                    horsie_agentcore::LifecycleEvent::Preparing(
                        horsie_agentcore::PreparingLifecycle { stage, detail },
                    ),
                )
                .await;
                CommandEffect::none()
            }
        }
    }

    /// Title an as-yet-unnamed session from the first thing the user says.
    ///
    /// Best-effort and fire-and-forget: a session that could not be titled is
    /// still a session, so a failure is logged and the message goes on to
    /// start its turn. The built-in title tool overwrites this later if the
    /// agent picks something better.
    pub(super) async fn title_from_first_message(&mut self, text: &str) {
        if self.spec().name.is_some() {
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
        let supervisor = self.supervisor.clone();
        let persisted = supervisor
            .ask(|reply| SessionSupervisorCommand::RenameSession {
                id: id.clone(),
                name: title.clone(),
                reply,
            })
            .await
            .map_err(|e| format!("session supervisor unavailable: {e}"))?;
        persisted.map_err(|e| format!("persist session title: {e}"))?;

        self.spec_mut().name = Some(title.clone());
        let _ = supervisor
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
    /// `main` is spawned at recovery and stays for the session's loaded life,
    /// a run's step agent is live for as long as its step is, and every
    /// subagent-targeted event happens while that subagent is running.
    pub(super) async fn record_lifecycle(&mut self, events: &[SessionEvent], state: &SessionState) {
        for event in events {
            for (agent, payload) in crate::sessions::lifecycle_routing::route(event, state) {
                let Some(resident) = self.agents.as_ref().and_then(|a| a.get(agent)).cloned()
                else {
                    tracing::warn!(
                        session = %self.id,
                        %agent,
                        "no resident agent to record a session event on; it will be missing from the log"
                    );
                    continue;
                };
                let _ = resident
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
        agent: AgentId,
        event: horsie_agentcore::LifecycleEvent,
    ) {
        let resident = self.agents.as_ref().and_then(|a| a.get(agent)).cloned();
        if let Some(resident) = resident {
            let _ = resident
                .actor
                .tell(AgentCommand::RecordLifecycle {
                    event,
                    at_ms: now_ms(),
                })
                .await;
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
