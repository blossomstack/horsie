//! The session's own bookkeeping: its title, and turn-preparation progress.
//!
//! A component like the rest, but the one whose slice is the session itself
//! rather than a feature. It owns `UsageRecorded`, because all three
//! agent-owning components record into `agent_usage` and the total belongs to
//! none of them.

use super::CoreCommand;
use super::{AgentKey, CommandEffect, SessionActor, SessionEvent, SessionState};
use crate::agent_loop::AgentCommand;
use crate::agent_loop::capabilities::title::normalize_session_title;
use crate::sessions::addressing::SessionInbox;
use crate::sessions::supervisor::SessionSupervisorCommand;
use horsie_actor::ActorContext;
use horsie_models::now_ms;

/// Longest auto-derived session title, in characters.
const TITLE_MAX_CHARS: usize = crate::agent_loop::capabilities::title::SESSION_TITLE_MAX_CHARS;

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
        state: &SessionState,
        cmd: CoreCommand,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionEvent> {
        match cmd {
            CoreCommand::SetTitle { title, reply, .. } => {
                let result = match normalize_session_title(&title) {
                    Ok(title) => actor.rename_session(title).await,
                    Err(error) => Err(error.to_string()),
                };
                // Journal the name this session now answers to, but only if it
                // actually took: a rejected title must not be recorded as one.
                let effect = match result.as_ref() {
                    Ok(name) => CommandEffect::persist(vec![SessionEvent::Renamed {
                        name: name.clone(),
                    }]),
                    Err(_) => CommandEffect::none(),
                };
                let _ = reply.send(result);
                effect
            }
            // The one boundary nothing else reaches. Everything it starts is
            // whatever `Runner::actions` asked for, which is the same call a
            // live boundary makes — so there is no recovery-only path here to
            // drift from the ordinary one.
            CoreCommand::Advance => actor.persist_and_advance(state, Vec::new(), ctx).await,
            CoreCommand::TitleSet { name } => {
                actor.spec_mut().name = Some(name.clone());
                CommandEffect::persist(vec![SessionEvent::Renamed { name }])
            }
            CoreCommand::RecordSpec { spec } => {
                // Idempotent, because a log that already says what this session
                // is has said it: a later one would overwrite a name the
                // session has since been given, and recovery has already
                // adopted what was there.
                if state.spec.is_some() {
                    return CommandEffect::none();
                }
                // The other half of how a session learns what it is. Recovery
                // covers a session with a history; this covers the one case it
                // cannot — a session created a moment ago, whose log is empty
                // and whose agents nothing else would ever start.
                actor.adopt((*spec).clone(), state, ctx).await;
                CommandEffect::persist(vec![SessionEvent::SpecRecorded { spec }])
            }
            CoreCommand::Progress { key, stage, detail } => {
                actor
                    .record_on(
                        key,
                        horsie_agentcore::LifecycleEvent::Preparing(
                            horsie_agentcore::PreparingLifecycle { stage, detail },
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
        events: &[SessionEvent],
        state: &SessionState,
    ) {
        for event in events {
            for (key, payload) in crate::sessions::lifecycle_routing::route(event, state) {
                let Some(agent) = self.agents.get(&key).cloned() else {
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
        let agent = self.agents.get(&key).cloned();
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


#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::testing::*;
    use super::*;
    use crate::sessions::session_actor::SessionCommand;
    use std::sync::Arc;
    use uuid::Uuid;

    /// Fold a session's own journal back into state, so a test can assert on
    /// what was recorded rather than on what the actor happens to hold.
    async fn journaled_spec(
        journal: &Arc<dyn horsie_actor::Journal>,
        id: Uuid,
    ) -> Option<crate::sessions::spec::SessionSpec> {
        use futures_util::StreamExt;
        let pid = SessionActor::persistence_id_for(id);
        #[expect(
            clippy::disallowed_methods,
            reason = "a test reads the journal directly to check what was written"
        )]
        let mut events = journal.replay(&pid, 0).await;
        let mut state = SessionState::default();
        while let Some(raw) = events.next().await {
            let (_, payload) = raw.unwrap();
            let event: SessionEvent = serde_json::from_slice(&payload).unwrap();
            state = <SessionActor as horsie_actor::EventSourcedActor>::apply_event(state, event);
        }
        state.spec
    }

    /// Wait until the session's log says what the test is waiting for. `tell`
    /// is fire-and-forget, so there is nothing to await on the send itself.
    async fn until_named(journal: &Arc<dyn horsie_actor::Journal>, id: Uuid, name: &str) {
        for _ in 0..100 {
            if journaled_spec(journal, id)
                .await
                .and_then(|s| s.name)
                .as_deref()
                == Some(name)
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the rename never reached the session's own log");
    }

    /// A session writes what it is into its own log, so a host that never saw
    /// the request creating it can still run it.
    #[tokio::test]
    async fn a_session_records_its_spec_in_its_own_log() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        let journal = f.journal();
        let session = f.start(id, actor_spec_fixture()).await;

        // A rename the session did not initiate — the path that does not have
        // to ask the supervisor first.
        let _ = session
            .tell(SessionCommand::Core(CoreCommand::TitleSet {
                name: "named".into(),
            }))
            .await;
        until_named(&journal, id, "named").await;

        let spec = journaled_spec(&journal, id)
            .await
            .expect("the spec is recorded");
        assert_eq!(spec.vendor, actor_spec_fixture().vendor);
        assert_eq!(
            spec.name.as_deref(),
            Some("named"),
            "a rename is durable in the session's own log, not only the supervisor's"
        );
    }

    /// The log is the truth, so loading again must adopt what is there rather
    /// than writing the seed over it. If it did not, every load would append
    /// and a session's journal would grow without anything happening in it.
    #[tokio::test]
    async fn a_reload_adopts_the_journaled_spec_instead_of_recording_again() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        let journal = f.journal();

        let first = f.start(id, actor_spec_fixture()).await;
        let _ = first
            .tell(SessionCommand::Core(CoreCommand::TitleSet {
                name: "named".into(),
            }))
            .await;
        until_named(&journal, id, "named").await;
        let settled = session_journal_len(&journal, id).await;
        drop(first);

        f.node.restart().await;
        let second = f.start(id, actor_spec_fixture()).await;
        let _ = second
            .tell(SessionCommand::Core(CoreCommand::TitleSet {
                name: "named".into(),
            }))
            .await;
        // Two renames to the same name, so waiting on the name cannot tell them
        // apart — wait on the log growing instead.
        for _ in 0..100 {
            if session_journal_len(&journal, id).await > settled {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        assert_eq!(
            session_journal_len(&journal, id).await,
            settled + 1,
            "loading again must add only the rename, never a second spec"
        );
        assert_eq!(
            journaled_spec(&journal, id).await.and_then(|s| s.name),
            Some("named".to_string())
        );
    }

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
