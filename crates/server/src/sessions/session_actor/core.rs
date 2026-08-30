//! The session's own bookkeeping: its title, and turn-preparation progress.
//!
//! A component like the rest, but the one whose slice is the session itself
//! rather than a feature. It owns `UsageRecorded`, because all three
//! agent-owning components record into `agent_usage` and the total belongs to
//! none of them.

use super::component::Component;
use super::{
    AgentKey, CommandEffect, CoreCommand, FirstMessage, LifecycleCommand, SessionActor,
    SessionCommand, SessionDomainEvent, SessionState, TurnCommand,
};
use crate::agent_loop::AgentCommand;
use crate::agent_loop::LogCommand as AgentLogCommand;
use crate::sessions::addressing::SessionInbox;
use crate::sessions::supervisor::SessionSupervisorCommand;
use crate::sessions::title_tool::normalize_session_title;
use horsie_actor::{ActorContext, EventSourcedActor};
use horsie_models::now_ms;

/// Longest auto-derived session title, in characters.
const TITLE_MAX_CHARS: usize = crate::sessions::title_tool::SESSION_TITLE_MAX_CHARS;

/// A short title derived from a user's first message.
pub(super) fn derive_title(text: &str) -> Option<String> {
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
    ) -> CommandEffect<SessionDomainEvent> {
        match cmd {
            CoreCommand::DeleteAgent { id, reply } => {
                // Resolved against the forest, not against a kind the caller
                // claimed: an id is all a URL carries, and only this session
                // knows what it names.
                let removable =
                    state.forest.sub(id).is_some() || state.forest.sub_session(id).is_some();
                if !removable {
                    let _ = reply.send(Err(match id == actor.id {
                        // The main agent is the session. Removing it is
                        // deleting the session, which is a different route
                        // with a different confirmation.
                        true => {
                            "the main agent is the session; delete the session instead".to_string()
                        }
                        false => format!("no such subagent or sub session: {id}"),
                    }));
                    return CommandEffect::none();
                }
                // Every actor under it, not just its own: the entries go in
                // one fold, and an actor left running would keep writing to a
                // log nothing can reach any more.
                for below in state.forest.descendant_agents(id) {
                    actor.retire_agent_actor(below).await;
                }
                actor.retire_agent_actor(id).await;
                let _ = reply.send(Ok(()));
                CommandEffect::persist(vec![SessionDomainEvent::AgentDeleted {
                    at_ms: now_ms(),
                    id,
                }])
            }
            CoreCommand::SetTitle { title, reply } => {
                let result = match normalize_session_title(&title) {
                    Ok(title) => actor.rename_main_agent(title).await,
                    Err(error) => Err(error.to_string()),
                };
                // Journal the name this session now answers to, but only if it
                // actually took: a rejected title must not be recorded as one.
                let effect = match result.as_ref() {
                    Ok(name) => CommandEffect::persist(vec![SessionDomainEvent::Renamed {
                        name: name.clone(),
                    }]),
                    Err(_) => CommandEffect::none(),
                };
                let _ = reply.send(result);
                effect
            }
            CoreCommand::TitleSet { name } => {
                CommandEffect::persist(vec![SessionDomainEvent::Renamed { name }])
            }
            CoreCommand::Create {
                spec,
                name,
                message,
            } => {
                // Whether *this* command is what brings the session into being,
                // and the answer to it is what makes the whole command
                // idempotent. A log that already says what this session is has
                // said it: writing a later spec would overwrite a name the
                // session has since been given, and recovery has already
                // adopted what was there. A redelivery, and a reload of a
                // session with a history, must also provision nothing —
                // loading a session starts nothing.
                let creating = state.spec.is_none();

                // The rest of the create, as self-sends, and queued *before*
                // adopting. The supervisor used to address these through the
                // shard itself, once each, and that is what let them land on a
                // node that had never heard of this session. Sent from here
                // they are ordinary mailbox entries, so nothing can arrive
                // without the spec.
                //
                // Ahead of `adopt` because `adopt` self-sends each component's
                // repairs, and those used to queue *behind* the supervisor's
                // `Provision` and first message rather than in front of them. A
                // workflow run repaired before it has a runtime does not start.
                let me = actor.me(ctx);
                if creating {
                    let _ = me
                        .tell(SessionCommand::Lifecycle(LifecycleCommand::Provision {
                            // The session's own runtime, owned by its main
                            // agent — whose id is the session's. No `env`: the
                            // spec is being journaled by this very command, so
                            // reading it here would read it before it is set.
                            // The handler reads it, by which time it is.
                            owner: actor.id,
                            env: None,
                        }))
                        .await;
                }
                // Answered either way: a create that carried a message owes
                // one, and a caller left waiting on a redelivery is the
                // failure this whole command exists to remove.
                if let Some(FirstMessage { message, reply }) = message {
                    let _ = me
                        .tell(SessionCommand::Turn(TurnCommand::UserMessage {
                            agent_id: None,
                            text: message.text,
                            // Whatever was attached to the create, carried on
                            // to the first turn: pasting a screenshot is how a
                            // session most often starts, and dropping it here
                            // would lose it silently. Already verified against
                            // this project by the HTTP layer.
                            artifacts: message.artifacts,
                            reply,
                        }))
                        .await;
                }

                if !creating {
                    CommandEffect::none()
                } else {
                    let mut events = vec![SessionDomainEvent::SpecRecorded {
                        at_ms: now_ms(),
                        session: actor.id,
                        spec,
                    }];
                    // After the spec, never before: `SpecRecorded` is what
                    // roots the forest, and a title has nowhere to land until
                    // the root entry exists.
                    if let Some(name) = name {
                        events.push(SessionDomainEvent::Renamed { name });
                    }
                    // The other half of how a session learns what it is.
                    // Recovery covers a session with a history; this covers the
                    // one case it cannot — a session created a moment ago,
                    // whose log is empty and whose agents nothing else would
                    // ever start. Adopted against the folded state, because
                    // this event is what roots the forest and the repairs read
                    // the root.
                    let next = events
                        .iter()
                        .cloned()
                        .fold(state.clone(), SessionActor::apply_event);
                    if let Some(spec) = next.spec.clone() {
                        actor.adopt(spec, &next, ctx).await;
                    }
                    CommandEffect::persist(events)
                }
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
    /// Title an as-yet-unnamed main agent from the first thing the user says.
    ///
    /// Best-effort: a session that could not be titled is still a session, so a
    /// failure is logged and the message goes on to start its turn. The
    /// built-in title tool overwrites this later if the agent picks something
    /// better.
    ///
    /// Hands the event back rather than journaling it, because the caller is
    /// already returning an effect and a second write from inside here would be
    /// a second journal round-trip on the message path. That the event is
    /// *returned at all* is the fix for a real gap: this used to update the
    /// resident spec copy and tell the supervisor, and write nothing to the
    /// session's own log — so a title derived from the first message survived
    /// in the session list and vanished from the session itself at the next
    /// load. Nothing noticed while the name lived in two places; now that the
    /// main agent's title *is* the session's name, the graph drew the main
    /// agent as "main agent" for ever.
    pub(super) async fn title_from_first_message(
        &mut self,
        state: &SessionState,
        text: &str,
    ) -> Option<SessionDomainEvent> {
        if state.forest.main_title().is_some() {
            return None;
        }
        let title = derive_title(text)?;
        match self.rename_main_agent(title).await {
            Ok(name) => Some(SessionDomainEvent::Renamed { name }),
            Err(error) => {
                tracing::warn!(session = %self.id, error, "failed to persist fallback session title");
                None
            }
        }
    }

    /// Persist the main agent's title through the supervisor, then publish it.
    ///
    /// The main agent *is* the session, so its title is what a session list
    /// shows and what a rename writes. The supervisor's copy is an index —
    /// what makes a name readable without loading the session — and the fold
    /// of [`SessionDomainEvent::Renamed`] onto the root run entry is the truth.
    pub(super) async fn rename_main_agent(&mut self, title: String) -> Result<String, String> {
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
    /// spawn with. That is not the limitation it looks like: a session's
    /// `main` is spawned at recovery and stays for the session's loaded life,
    /// a run's step agent is live for as long as its step is, and every
    /// subagent-targeted event happens while that subagent is running. A miss
    /// is therefore a bug worth hearing about rather than a case to handle,
    /// which is what the warning is for — an event with genuinely nowhere to
    /// go routes to nothing at all, and never reaches this loop.
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
                let Some(agent) = self.agents.get(key).cloned() else {
                    tracing::warn!(
                        session = %self.id,
                        ?key,
                        "no resident agent to record a session event on; it will be missing from the log"
                    );
                    continue;
                };
                let _ = agent
                    .actor
                    .tell(AgentCommand::Log(AgentLogCommand::RecordLifecycle {
                        event: payload,
                        at_ms: now_ms(),
                    }))
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
        let agent = self.agents.get(key).cloned();
        if let Some(agent) = agent {
            let _ = agent
                .actor
                .tell(AgentCommand::Log(AgentLogCommand::RecordLifecycle {
                    event,
                    at_ms: now_ms(),
                }))
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
    // The fallthrough is unreachable by construction:
    // `SessionActor::apply_event` matches every variant explicitly and routes
    // each to exactly one component, so a newly added event fails to compile
    // *there* — which is where it should be classified — rather than silently
    // reaching the wrong fold here.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn apply(state: &mut SessionState, event: &SessionDomainEvent) {
        match event.clone() {
            SessionDomainEvent::AgentDeleted { id, .. } => {
                state.forest.apply_agent_deleted(id);
            }
            SessionDomainEvent::UsageRecorded {
                agent_id,
                usage_total,
                context_tokens,
                ..
            } => {
                state.agent_usage.insert(agent_id.clone(), usage_total);
                state.agent_context_tokens.insert(agent_id, context_tokens);
            }
            SessionDomainEvent::SpecRecorded {
                at_ms,
                session,
                spec,
            } => {
                // The spec is also what roots the forest: a session's
                // main entry, or the session's own workflow run — keyed by the
                // session id, so replay needs nothing but this event.
                match &spec.kind {
                    crate::sessions::spec::SessionKind::Agent { .. } => {
                        // What the spec asked for. `Pending` rather than
                        // `Without` when it wants one: the record naming it
                        // lands moments later, and until then nothing may run.
                        state.forest.apply_root_agent(
                            session,
                            at_ms,
                            match spec.runtime {
                                Some(_) => crate::sessions::run_forest::RuntimeChoice::Pending,
                                None => crate::sessions::run_forest::RuntimeChoice::Without,
                            },
                        );
                    }
                    crate::sessions::spec::SessionKind::Workflow { run } => {
                        state.forest.apply_root_workflow(
                            session,
                            run.workflow.clone(),
                            run.clone(),
                            at_ms,
                            match spec.runtime {
                                Some(_) => crate::sessions::run_forest::RuntimeChoice::Pending,
                                None => crate::sessions::run_forest::RuntimeChoice::Without,
                            },
                        );
                    }
                }
                state.spec = Some(*spec);
            }
            SessionDomainEvent::Renamed { name } => {
                // Onto the root run entry, which is the main agent's: the main
                // agent is the session, so the session's name and its main
                // agent's title are one fact with one owner. A run has no main
                // agent and the fold is a no-op there — a run is named by its
                // workflow.
                state.forest.apply_main_titled(name);
            }
            other => unreachable!("SessionCore was handed {other:?}"),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::testing::*;
    use super::*;
    use crate::sessions::session_actor::{NewSessionMessage, SessionCommand};
    use std::sync::Arc;
    use uuid::Uuid;

    /// Fold a session's own journal back into state, so a test can assert on
    /// what was recorded rather than on what the actor happens to hold.
    async fn journaled_state(journal: &Arc<dyn horsie_actor::Journal>, id: Uuid) -> SessionState {
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
            let event: SessionDomainEvent = serde_json::from_slice(&payload).unwrap();
            state = <SessionActor as horsie_actor::EventSourcedActor>::apply_event(state, event);
        }
        state
    }

    async fn journaled_spec(
        journal: &Arc<dyn horsie_actor::Journal>,
        id: Uuid,
    ) -> Option<crate::sessions::spec::SessionSpec> {
        journaled_state(journal, id).await.spec
    }

    /// The session's name, read where it now lives: the main agent's title on
    /// the root run entry.
    async fn journaled_title(journal: &Arc<dyn horsie_actor::Journal>, id: Uuid) -> Option<String> {
        journaled_state(journal, id)
            .await
            .forest
            .main_title()
            .map(str::to_string)
    }

    /// Wait until the session's log says what the test is waiting for. `tell`
    /// is fire-and-forget, so there is nothing to await on the send itself.
    async fn until_named(journal: &Arc<dyn horsie_actor::Journal>, id: Uuid, name: &str) {
        for _ in 0..100 {
            if journaled_title(journal, id).await.as_deref() == Some(name) {
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
        assert_eq!(spec.vendor(), actor_spec_fixture().vendor());
        assert_eq!(
            journaled_title(&journal, id).await.as_deref(),
            Some("named"),
            "a rename is durable in the session's own log, not only the supervisor's"
        );
    }

    /// A session reached before it has been told what it is answers, rather
    /// than dying under the caller.
    ///
    /// The cluster failure this exists for. Addressing a session is what
    /// materialises its actor, so a command can arrive at one whose log is
    /// empty and whose spec is therefore unset. Reading that spec used to
    /// panic: the actor died, the reply channel closed, and the create still in
    /// flight for that session reported a 500, a 404 or a 409 from
    /// `POST /sessions` depending on which half lost the race. "No such
    /// session" is both survivable and true.
    #[tokio::test]
    async fn a_message_before_the_spec_is_answered_rather_than_fatal() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        // Deliberately not `f.start`: nothing has told this session what it is.
        let session = f.node.session(id);

        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = session
            .tell(SessionCommand::Turn(TurnCommand::UserMessage {
                agent_id: None,
                text: "hi".into(),
                reply: horsie_actor::ReplyTo::from_sender(tx),
                artifacts: Vec::new(),
            }))
            .await;

        let answered = tokio::time::timeout(std::time::Duration::from_secs(5), rx).await;
        assert!(
            matches!(
                answered,
                Ok(Ok(Err(crate::sessions::UserMessageError::NotFound)))
            ),
            "a session with no spec must answer, not take the actor down with it; \
             got {answered:?}"
        );
    }

    /// One command creates a session: what it is, its runtime and the first
    /// thing said to it.
    ///
    /// The property the fix turns on, asserted where it can be seen — the
    /// supervisor used to send these as three separately-addressed commands,
    /// and it is the *separation* that let them come apart across a placement
    /// move. Here the create carries a message and nothing else is ever sent.
    #[tokio::test]
    async fn one_command_records_the_spec_and_queues_the_first_message() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        let journal = f.journal();
        let session = f.node.session(id);

        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = session
            .tell(SessionCommand::Core(CoreCommand::Create {
                spec: Box::new(actor_spec_fixture()),
                name: None,
                message: Some(FirstMessage {
                    message: NewSessionMessage::text("hi"),
                    reply: horsie_actor::ReplyTo::from_sender(tx),
                }),
            }))
            .await;

        let accepted = tokio::time::timeout(std::time::Duration::from_secs(10), rx)
            .await
            .expect("the first message is answered")
            .expect("the session answered it");
        assert!(
            accepted.is_ok(),
            "the message carried by the create must be accepted: {accepted:?}"
        );
        assert!(
            journaled_spec(&journal, id).await.is_some(),
            "the same command must have recorded what this session is"
        );
    }

    /// The fallback title reaches the session's own log, not just the list.
    ///
    /// It used to write the resident spec copy and tell the supervisor, and
    /// journal nothing — so the name survived in the session list and was gone
    /// from the session itself at the next load. Invisible while a name lived
    /// in two places; fatal once the main agent's title became the one copy.
    #[tokio::test]
    async fn a_title_derived_from_the_first_message_is_in_the_sessions_own_log() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        let journal = f.journal();
        let session = f.start(id, actor_spec_fixture()).await;

        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = session
            .tell(SessionCommand::Turn(
                crate::sessions::session_actor::TurnCommand::UserMessage {
                    agent_id: None,
                    text: "migrate the journal to postgres".into(),
                    reply: horsie_actor::ReplyTo::from_sender(tx),
                    artifacts: Vec::new(),
                },
            ))
            .await;
        let _ = rx.await;

        until_named(&journal, id, "migrate the journal to postgres").await;
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
            journaled_title(&journal, id).await,
            Some("named".to_string())
        );
    }

    /// What was attached to the create reaches the turn its message starts.
    ///
    /// The reason the create carries artifacts at all: pasting a screenshot is
    /// how a session most often begins, and the id used to be dropped on the
    /// floor between `CreateSessionRequest` and the agent's queue — silently,
    /// because the sentence still arrived and only the picture was missing.
    ///
    /// Asserted on the agent's own history rather than on what was sent,
    /// because the parts are built two hops further on: the queue entry
    /// becomes a turn, and the turn becomes the message the model is shown.
    #[tokio::test]
    async fn the_creates_attachments_reach_the_first_turns_message() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        // The runtime the session will adopt, and a model behind "mock" — a
        // create whose turn never runs journals no message to read.
        f.deps
            .runtimes
            .create(
                crate::runtime_manager::RuntimeAddress {
                    session: &id.to_string(),
                    runtime: &id.to_string(),
                    incarnation: "i1",
                },
                "mock",
                &actor_spec_fixture()
                    .runtime_env()
                    .expect("the fixture has a runtime"),
            )
            .await
            .expect("create");
        f.deps.provider_registry.write().unwrap().insert(
            "mock".to_string(),
            crate::sessions::spec::ModelEntry::provider_only(Arc::new(EchoProvider)),
        );
        let session = f.node.session(id);
        let shot = horsie_models::agent::ArtifactRef {
            id: "sha256-of-the-screenshot".to_string(),
            media_type: "image/png".to_string(),
            kind: horsie_models::agent::ArtifactKind::Image(horsie_models::agent::ImageArtifact {
                width: Some(1),
                height: Some(1),
            }),
            byte_size: 29,
            filename: Some("shot.png".to_string()),
        };

        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = session
            .tell(SessionCommand::Core(CoreCommand::Create {
                spec: Box::new(actor_spec_fixture()),
                name: None,
                message: Some(FirstMessage {
                    message: NewSessionMessage {
                        text: "what is in this?".into(),
                        artifacts: vec![shot.clone()],
                    },
                    reply: horsie_actor::ReplyTo::from_sender(tx),
                }),
            }))
            .await;
        let accepted = tokio::time::timeout(std::time::Duration::from_secs(10), rx)
            .await
            .expect("the first message is answered")
            .expect("the session answered it");
        assert!(accepted.is_ok(), "the message is accepted: {accepted:?}");

        await_turns(&session, 1).await;
        let page = agent_history(&session, None).await;
        let attached: Vec<String> = page
            .messages()
            .filter(|m| m.role == horsie_agentcore::Role::User)
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| match p {
                horsie_agentcore::ContentPart::Artifact(a) => Some(a.artifact.id.clone()),
                horsie_agentcore::ContentPart::Text(_)
                | horsie_agentcore::ContentPart::ToolCall(_)
                | horsie_agentcore::ContentPart::ToolResult(_)
                | horsie_agentcore::ContentPart::Thinking(_)
                | horsie_agentcore::ContentPart::SubAgentResult(_) => None,
            })
            .collect();
        assert_eq!(
            attached,
            vec![shot.id],
            "the create's attachment is a part of the first user message: {:?}",
            page.entries
        );
        assert_eq!(
            user_texts(&page),
            vec!["what is in this?".to_string()],
            "and it did not cost the sentence it came with"
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
