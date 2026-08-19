//! The session's own bookkeeping: its title, and turn-preparation progress.
//!
//! A component like the rest, but the one whose slice is the session itself
//! rather than a feature. It owns `UsageRecorded`, because all three
//! agent-owning components record into `agent_usage` and the total belongs to
//! none of them.

use super::CoreCommand;
use super::{CommandEffect, SessionActor, SessionEvent, SessionState};
use crate::agent_loop::AgentCommand;
use crate::agent_loop::capabilities::title::normalize_session_title;
use crate::sessions::addressing::SessionInbox;
use crate::sessions::runners::conversation;
use crate::sessions::runners::ids::{AgentId, RunnerId};
use crate::sessions::runners::{RunnerEvent, RunnerState};
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

/// Which conversation a `set_session_title` call names.
///
/// The tool is one tool and the model is never told which kind of conversation
/// it is in, so the question is the session's to answer — from the runner the
/// asking agent belongs to, which is the same fact every other routing here
/// reads. Answering it with `rename_session` regardless is how a fork renamed
/// the session out from under the person reading the conversation it branched
/// from.
#[derive(Debug, PartialEq, Eq)]
enum Names {
    /// The session itself. Its own conversation *is* the session, so there is
    /// nothing on the runner to write: the name is the session's.
    Session,
    /// One fork, by the runner holding it.
    Fork(RunnerId),
    /// No conversation at all. Only a conversation is equipped with the tool,
    /// so this is a request that should not have been made — refused in words
    /// rather than dropped, because the agent that made it is parked on an
    /// answer.
    Nothing,
}

/// What this agent's `set_session_title` renames.
fn names(state: &SessionState, agent: AgentId) -> Names {
    let Some(runner) = state.runner_of(agent) else {
        return Names::Nothing;
    };
    match state.record(runner).map(|record| &record.state) {
        // The root conversation is the session, whatever else it is; every
        // other one is a fork, and a fork names itself.
        Some(RunnerState::Conversation(_)) if runner == state.root => Names::Session,
        Some(RunnerState::Conversation(_)) => Names::Fork(runner),
        Some(RunnerState::SubAgent(_) | RunnerState::Workflow(_) | RunnerState::Runtime(_))
        | None => Names::Nothing,
    }
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
            CoreCommand::SetTitle {
                agent,
                title,
                reply,
            } => {
                let name = match normalize_session_title(&title) {
                    Ok(name) => name,
                    Err(error) => {
                        let _ = reply.send(Err(error.to_string()));
                        return CommandEffect::none();
                    }
                };
                match names(state, agent) {
                    Names::Session => {
                        let result = actor.rename_session(name).await;
                        // Journal the name this session now answers to, but only
                        // if it actually took: a rejected title must not be
                        // recorded as one.
                        let effect = match result.as_ref() {
                            Ok(name) => CommandEffect::persist(vec![SessionEvent::Renamed {
                                name: name.clone(),
                            }]),
                            Err(_) => CommandEffect::none(),
                        };
                        let _ = reply.send(result);
                        effect
                    }
                    // Nothing to ask the supervisor: a fork's name is its own
                    // runner's, read from there by its row in the session list.
                    // The session's list entry is the session's name, and this
                    // is not it.
                    Names::Fork(runner) => {
                        let _ = reply.send(Ok(name.clone()));
                        CommandEffect::persist(vec![SessionEvent::Runner {
                            id: runner,
                            event: Box::new(RunnerEvent::Conversation(
                                conversation::Event::Titled { name },
                            )),
                            at_ms: now_ms(),
                        }])
                    }
                    Names::Nothing => {
                        let _ = reply.send(Err(
                            "only a conversation can be renamed, and this agent is not in one"
                                .to_string(),
                        ));
                        CommandEffect::none()
                    }
                }
            }
            // The one boundary nothing else reaches. Everything it starts is
            // whatever `Runner::actions` asked for, which is the same call a
            // live boundary makes — so there is no recovery-only path here to
            // drift from the ordinary one.
            CoreCommand::RuntimeEvent { runner, event } => {
                let events = vec![SessionEvent::Runner {
                    id: runner,
                    event: Box::new(crate::sessions::runners::RunnerEvent::Runtime(event)),
                    at_ms: now_ms(),
                }];
                actor.persist_and_advance(state, events, ctx).await
            }
            // Through `persist_and_advance` rather than a bare persist: a
            // branch point landing is what releases the fork's own agent, and
            // starting it is an action the boundary this creates performs.
            CoreCommand::SeedSettled { runner, result } => {
                let events = actor.seed_settled(runner, result, state, ctx).await;
                actor.persist_and_advance(state, events, ctx).await
            }
            CoreCommand::Advance => {
                // The one boundary that is a *load*: `adopt` sends this and
                // nothing else does, which is what makes the reconciliation
                // below sound — see `SessionActor::interrupted_at_load`.
                let events = actor.interrupted_at_load(state);
                actor.persist_and_advance(state, events, ctx).await
            }
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
                let mut events = vec![SessionEvent::SpecRecorded { spec: spec.clone() }];
                events.extend(actor.birth_runners(&spec));
                CommandEffect::persist(events)
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
    pub(super) async fn record_lifecycle(&mut self, events: &[SessionEvent], state: &SessionState) {
        for event in events {
            for (key, payload) in crate::sessions::runners::lifecycle_routing::route(event, state) {
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
        key: crate::sessions::runners::ids::AgentId,
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
    use crate::sessions::addressing::SessionRef;
    use crate::sessions::session_actor::SessionCommand;
    use std::sync::Arc;
    use uuid::Uuid;

    /// A session with a turn in flight refuses to go cold.
    ///
    /// The refusal is the whole of how a long tool call survives the idle
    /// sweep: answering `true` here unloads the session out from under a run
    /// that is still writing to its journal. Restored with the hibernate the
    /// runner swap dropped — the two are one act, and only the refusal has
    /// somewhere to be observed without a vendor.
    #[tokio::test]
    async fn going_cold_is_refused_while_an_agent_is_working() {
        let gate = BlockingProvider::new();
        let (_f, session, _id, journal) = spawn_session_with_provider(gate.clone()).await;
        send(&session, "go").await;
        wait_for_state(&journal, _id, "a turn in flight", |s| {
            crate::sessions::runners::reads::session_status(s)
                == crate::sessions::spec::SessionStatus::Running
        })
        .await;

        let offloaded = session
            .ask(|reply| SessionCommand::PrepareOffload { reply })
            .await
            .expect("the session answers");
        assert!(
            !offloaded,
            "a session with a turn in flight must not be unloaded"
        );
        gate.release();
    }

    /// And a worker counts too, even though the conversation that asked for it
    /// is idle: the session is what holds the sandbox they share.
    #[tokio::test]
    async fn going_cold_is_refused_while_a_worker_is_working() {
        let gate = BlockingProvider::new();
        let (f, session, id, journal) = spawn_session_with_provider(gate.clone()).await;
        let worker = spawn_sub(&session, "research", "dig into it").await;
        wait_for_tree(&journal, id, |t| {
            t.iter()
                .find(|r| r.id == worker.to_string())
                .is_some_and(|r| r.status == crate::sessions::session_actor::AgentStatus::Running)
        })
        .await;
        let _ = &f;

        let offloaded = session
            .ask(|reply| SessionCommand::PrepareOffload { reply })
            .await
            .expect("the session answers");
        assert!(!offloaded, "a working worker holds the session open");
        gate.release();
    }

    /// Fold a session's own journal back into state, so a test can assert on
    /// what was recorded rather than on what the actor happens to hold.
    async fn journaled_spec(
        journal: &Arc<dyn horsie_actor::Journal>,
        id: Uuid,
    ) -> Option<crate::sessions::spec::SessionSpec> {
        folded(journal, id).await.spec
    }

    /// The whole of what a session's log says, folded the way a load would.
    async fn folded(journal: &Arc<dyn horsie_actor::Journal>, id: Uuid) -> SessionState {
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
        state
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

    /// Type `text` at the session's own conversation and hand back the fork it
    /// created.
    async fn fork_via(session: &SessionRef, text: &str) -> String {
        session
            .ask(|reply| SessionCommand::UserMessage {
                agent_id: None,
                text: text.into(),
                reply,
            })
            .await
            .unwrap()
            .unwrap()
            .forked_agent
            .expect("a fork command answers with a fork")
    }

    /// What one conversation is called, read off the session's folded state.
    fn conversation_title(state: &SessionState, agent: &str) -> Option<String> {
        let agent = crate::sessions::runners::ids::AgentId(agent.parse().ok()?);
        match &state.record(state.runner_of(agent)?)?.state {
            crate::sessions::runners::RunnerState::Conversation(c) => c.title.clone(),
            crate::sessions::runners::RunnerState::SubAgent(_)
            | crate::sessions::runners::RunnerState::Workflow(_)
            | crate::sessions::runners::RunnerState::Runtime(_) => None,
        }
    }

    /// **A fork names itself, and leaves the session's name alone.**
    ///
    /// `set_session_title` is one tool in every conversation — the model is not
    /// told which kind it is in — so which conversation a call renames is the
    /// session's to decide, from the runner the asking agent belongs to.
    /// Answering every call with `rename_session` renames the whole session out
    /// from under the person reading the conversation the fork branched from.
    #[tokio::test]
    async fn a_fork_renaming_itself_does_not_rename_the_session() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let fork = fork_via(&session, "/fork try the other migration").await;

        let named = session
            .ask(|reply| {
                SessionCommand::Core(CoreCommand::SetTitle {
                    agent: crate::sessions::runners::ids::AgentId(fork.parse().unwrap()),
                    title: "the other migration".into(),
                    reply,
                })
            })
            .await
            .expect("the session answers")
            .expect("a name a fork may take");
        assert_eq!(named, "the other migration");

        wait_for_state(&journal, id, "the fork is named", |s| {
            conversation_title(s, &fork).as_deref() == Some("the other migration")
        })
        .await;
        assert_eq!(
            journaled_spec(&journal, id).await.and_then(|s| s.name),
            actor_spec_fixture().name,
            "a fork renamed the session it branched from"
        );
    }

    /// And the other half of the same routing: the session's own conversation
    /// *is* the session, so naming it renames the session and writes nothing on
    /// the runner.
    #[tokio::test]
    async fn the_session_s_own_conversation_renames_the_session() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let main = main_agent(&session).await;

        let named = session
            .ask(|reply| {
                SessionCommand::Core(CoreCommand::SetTitle {
                    agent: main,
                    title: "the flake".into(),
                    reply,
                })
            })
            .await
            .expect("the session answers")
            .expect("a name the session may take");
        assert_eq!(named, "the flake");

        until_named(&journal, id, "the flake").await;
        assert_eq!(
            conversation_title(&folded(&journal, id).await, &main.to_string()),
            None,
            "the session's own conversation kept a second copy of the session's name"
        );
    }

    /// **A session's total is what its agents spent, not their running total
    /// added again every turn.**
    ///
    /// An agent reports its cumulative usage at each turn boundary and the
    /// session's aggregate adds what it is handed, so turn two used to bank
    /// turn one a second time — and a session with exactly one agent read
    /// higher than that agent, which is a sum that cannot be right whatever the
    /// numbers are.
    #[tokio::test]
    async fn a_second_turn_banks_only_what_it_spent() {
        use crate::sessions::runners::reads::usage_stats;
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let main = main_agent(&session).await;

        // Two turns, one token in and one out apiece.
        send(&session, "one").await;
        send(&session, "two").await;
        wait_for_state(&journal, id, "two turns of usage", |s| {
            usage_stats(s).session_total.input_tokens == 2
        })
        .await;

        let stats = usage_stats(&folded(&journal, id).await);
        assert_eq!(
            stats.agents.get(&main.to_string()).copied(),
            Some(stats.session_total),
            "one agent: the session total must equal its own total: {stats:?}"
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
