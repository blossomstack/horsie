//! Getting and releasing this session's sandbox.
//!
//! A precondition rather than a participant: nothing else here talks to it, and
//! its whole interface to the rest of the session is [`RuntimeLifecycle::ready`],
//! which the turn boundary checks before asking any component what to start.
//!
//! The create is journaled *before* the vendor is called and runs off the
//! mailbox, so an interrupted create is discoverable at load and a session stays
//! able to answer reads, stops and deletes for the minutes one takes.

use super::component::{ActionCx, Component};
use super::{
    CommandEffect, LifecycleCommand, SessionActor, SessionCommand, SessionDomainEvent, SessionState,
};
use crate::runtime_manager::RuntimeError;
use crate::sessions::spec::SessionStatus;
use horsie_actor::{ActorContext, EventSourcedActor};
use horsie_models::now_ms;

/// RuntimeLifecycle.
pub(super) struct RuntimeLifecycle;

impl RuntimeLifecycle {
    /// Whether this session has a runtime to run on — the single gate the turn
    /// boundary checks before asking any component what to start.
    ///
    /// Both answers are journaled statuses, so they survive the process dying
    /// mid-create, which no in-memory gate could. `ProvisioningFailed` is the
    /// second of them: a create that failed on something retryable leaves a
    /// session with no runtime at all, and a turn started there would ask a
    /// vendor for one and be told, terminally, that it is gone.
    pub(super) fn ready(state: &SessionState) -> bool {
        !matches!(
            state.status,
            SessionStatus::Provisioning | SessionStatus::ProvisioningFailed { .. }
        )
    }
}

impl RuntimeLifecycle {
    pub(super) async fn handle(
        actor: &mut SessionActor,
        state: &SessionState,
        cmd: LifecycleCommand,
        ctx: &ActorContext<SessionActor>,
    ) -> CommandEffect<SessionDomainEvent> {
        match cmd {
            LifecycleCommand::Provision => {
                // Provision only from the three states that mean "no runtime has
                // ever been confirmed": a session just created (nothing
                // journaled, so the default `Idle`), one found still
                // `Provisioning` at load because the process died inside its
                // create, and one whose create failed on something retryable.
                //
                // Every other status means a create already succeeded, and
                // re-running one would rebuild a workspace someone may be using
                // — the thing this design exists to make impossible. The
                // `Idle` arm is the loose one: it is also every healthy
                // session's status, so it holds only because the supervisor
                // sends this exactly once, at creation.
                if !matches!(
                    state.status,
                    SessionStatus::Idle
                        | SessionStatus::Provisioning
                        | SessionStatus::ProvisioningFailed { .. }
                ) {
                    return CommandEffect::none();
                }
                let runtimes = actor.deps.runtimes.clone();
                let session = actor.id.to_string();
                let vendor = actor.spec.vendor.clone();
                let spec = actor.spec.clone();
                let me = ctx.self_ref();
                // Off the mailbox: a real create runs for minutes, and this
                // actor has to keep answering reads, stops and deletes
                // throughout. The status it just journaled is what holds the
                // turn back meanwhile.
                tokio::spawn(async move {
                    let (error, terminal) = match runtimes.create(&session, &vendor, &spec).await {
                        Ok(()) => (None, false),
                        // Exactly the split `get` makes: only a live vendor
                        // refusing to produce the runtime is terminal. An
                        // offline vendor or a failed token mint is a bad
                        // moment, not a dead session.
                        Err(e @ RuntimeError::Gone(_)) => (Some(e.to_string()), true),
                        Err(e @ (RuntimeError::Unavailable(_) | RuntimeError::Provision(_))) => {
                            (Some(e.to_string()), false)
                        }
                    };
                    let _ = me
                        .tell(SessionCommand::Lifecycle(
                            LifecycleCommand::FinishProvisioning { error, terminal },
                        ))
                        .await;
                });
                actor.report(SessionStatus::Provisioning).await;
                CommandEffect::persist(vec![SessionDomainEvent::ProvisioningStarted {
                    at_ms: now_ms(),
                }])
            }
            LifecycleCommand::FinishProvisioning { error, terminal } => {
                let event = match error {
                    None => SessionDomainEvent::ProvisioningSucceeded { at_ms: now_ms() },
                    Some(error) => SessionDomainEvent::ProvisioningFailed {
                        at_ms: now_ms(),
                        error,
                        terminal,
                    },
                };
                let next = SessionActor::apply_event(state.clone(), event.clone());
                actor.report(next.status.clone()).await;
                let mut events = vec![event];
                // The runtime landed, so whatever queued behind it starts now.
                // A failure drains nothing: the messages stay owed, and the
                // next thing the user sends is what tries again.
                events.extend(actor.flush_then_drain(&next, ctx).await);
                CommandEffect::persist(events)
            }
            LifecycleCommand::PrepareOffload { reply } => {
                // Work started while the supervisor was deciding: refuse, and
                // let the idle clock start again. Asked of every component
                // rather than hand-written here, so a component added later
                // makes itself heard instead of being silently unloadable.
                if actor.busy(state) {
                    let _ = reply.send(false);
                    return CommandEffect::none();
                }
                actor.stop_agents().await;
                actor
                    .deps
                    .runtimes
                    .hibernate(&actor.id.to_string(), &actor.spec.vendor)
                    .await;
                // Answered as this actor's last act: it writes nothing after
                // returning, so the supervisor can drop its reference the
                // moment it sees `true`.
                let _ = reply.send(true);
                CommandEffect::stop()
            }
            LifecycleCommand::Delete { reply } => {
                actor.cancel_in_flight(state).await;
                actor.stop_agents().await;
                actor
                    .deps
                    .runtimes
                    .delete(&actor.id.to_string(), &actor.spec.vendor)
                    .await;
                let _ = reply.send(());
                CommandEffect::stop()
            }
        }
    }
}

impl Component for RuntimeLifecycle {
    /// A create the process died inside, or one that failed on something
    /// retryable. Re-attempting is safe here and nowhere else: `Provisioning` is
    /// precisely the state in which no turn has run, so there is no work in the
    /// workspace for a rebuild to destroy.
    fn on_load(_cx: &ActionCx<'_>, state: &SessionState) -> Option<SessionCommand> {
        matches!(
            state.status,
            SessionStatus::Provisioning | SessionStatus::ProvisioningFailed { .. }
        )
        .then_some(SessionCommand::Lifecycle(LifecycleCommand::Provision))
    }

    fn busy(state: &SessionState) -> bool {
        matches!(state.status, SessionStatus::Provisioning)
    }

    /// The three provisioning facts. Each one sets `status`, which is how the
    /// rest of the session learns whether it has a runtime to run on.
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
            SessionDomainEvent::ProvisioningStarted { .. } => {
                state.status = SessionStatus::Provisioning;
            }
            SessionDomainEvent::ProvisioningSucceeded { .. } => {
                state.status = SessionStatus::Idle;
                state.last_error = None;
            }
            SessionDomainEvent::ProvisioningFailed {
                error, terminal, ..
            } => {
                state.status = if terminal {
                    SessionStatus::Unrecoverable {
                        reason: error.clone(),
                    }
                } else {
                    SessionStatus::ProvisioningFailed {
                        reason: error.clone(),
                    }
                };
                state.last_error = Some(error);
            }
            other => unreachable!("RuntimeLifecycle was handed {other:?}"),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    //! Getting and releasing the sandbox: what a create does, what an
    //! interrupted one replays as, and what refuses an offload.
    use super::super::testing::*;
    use super::super::*;
    use super::*;

    use horsie_agentcore::LlmProvider;

    use std::sync::Arc;
    use uuid::Uuid;

    /// A session is `Provisioning` from the moment its create is journaled
    /// until the event that says how the create ended. Nothing else reaches
    /// this status, and no turn can run inside it.
    #[test]
    fn a_created_session_provisions_before_it_is_idle() {
        let started = fold(vec![SessionDomainEvent::ProvisioningStarted { at_ms: 0 }]);
        assert_eq!(started.status, SessionStatus::Provisioning);

        let ready = fold(vec![
            SessionDomainEvent::ProvisioningStarted { at_ms: 0 },
            SessionDomainEvent::ProvisioningSucceeded { at_ms: 1 },
        ]);
        assert_eq!(ready.status, SessionStatus::Idle);
    }

    /// The message the session was created with waits in its agent's queue
    /// rather than racing the vendor. What the session contributes is the gate:
    /// `ready` is false while provisioning and true once the runtime lands, and
    /// that answer is pushed to every agent.
    #[test]
    fn a_session_is_not_runnable_until_its_runtime_lands() {
        let waiting = fold(vec![SessionDomainEvent::ProvisioningStarted { at_ms: 0 }]);
        assert_eq!(waiting.status, SessionStatus::Provisioning);
        assert!(
            !RuntimeLifecycle::ready(&waiting),
            "nothing may run before the runtime exists"
        );

        let ready = SessionActor::apply_event(
            waiting,
            SessionDomainEvent::ProvisioningSucceeded { at_ms: 2 },
        );
        assert_eq!(ready.status, SessionStatus::Idle);
        assert!(RuntimeLifecycle::ready(&ready));
    }

    /// A create that failed on something retryable — an offline vendor, a
    /// GitHub token that could not be minted — leaves a session that can try
    /// again, and reports the reason the vendor actually gave rather than the
    /// "no such runtime" a later `get` would have invented.
    #[test]
    fn a_retryable_create_failure_is_reported_verbatim() {
        let s = fold(vec![
            SessionDomainEvent::ProvisioningStarted { at_ms: 0 },
            SessionDomainEvent::ProvisioningFailed {
                at_ms: 1,
                error: "runtime vendor unavailable: vendor 'local' is not connected".into(),
                terminal: false,
            },
        ]);
        // Its own status, not the `Failed` a failed *turn* leaves. The two look
        // identical to a reader and mean opposite things to the session: a
        // failed turn has a runtime and can simply run again, while this one has
        // no runtime at all and must build one before it can do anything.
        assert_eq!(
            s.status,
            SessionStatus::ProvisioningFailed {
                reason: "runtime vendor unavailable: vendor 'local' is not connected".into(),
            }
        );
        assert!(s.last_error.is_some());
    }

    /// The status a failed create leaves must not let a turn start — the turn
    /// would ask for a runtime that was never built and be told, terminally,
    /// that it is gone. That is the whole defect in #239.
    #[test]
    fn a_failed_create_starts_no_turn() {
        let s = fold(vec![
            SessionDomainEvent::ProvisioningStarted { at_ms: 0 },
            SessionDomainEvent::ProvisioningFailed {
                at_ms: 1,
                error: "runtime vendor unavailable".into(),
                terminal: false,
            },
        ]);
        assert!(
            !RuntimeLifecycle::ready(&s),
            "a session with no runtime must build one before it runs anything"
        );
    }

    /// A live vendor refusing to build the runtime is the terminal case, and
    /// the only one: it is the same `Gone` a `get` reports.
    #[test]
    fn a_terminal_create_failure_ends_the_session() {
        let s = fold(vec![
            SessionDomainEvent::ProvisioningStarted { at_ms: 0 },
            SessionDomainEvent::ProvisioningFailed {
                at_ms: 1,
                error: "runtime is gone: vendor cannot provision".into(),
                terminal: true,
            },
        ]);
        assert!(matches!(s.status, SessionStatus::Unrecoverable { .. }));
    }

    /// The bug in one test: a message that arrives while the create is still in
    /// flight must queue, not ask a vendor that has never heard of the runtime.
    ///
    /// The create is held open for the whole window, so this is a statement
    /// about the design and not about scheduling luck — and the wait is the
    /// session's own journaled status, which is what makes it survive a restart
    /// where an in-memory gate would not.
    #[tokio::test]
    async fn a_message_arriving_mid_create_waits_for_the_runtime() {
        let f = actor_fixture_blocking_creates().await;
        f.deps.provider_registry.write().unwrap().insert(
            "mock".to_string(),
            Arc::new(EchoProvider) as Arc<dyn LlmProvider>,
        );
        let id = Uuid::new_v4();
        let (session, journal) = spawn_unprovisioned(&f, id);

        session
            .tell(SessionCommand::Lifecycle(LifecycleCommand::Provision))
            .await
            .unwrap();
        let (tx, _rx) = oneshot::channel();
        session
            .tell(SessionCommand::Turn(TurnCommand::UserMessage {
                agent_id: None,
                text: "hello".into(),
                reply: tx,
            }))
            .await
            .unwrap();

        // The message is owed an answer, not spent on a runtime that does not
        // exist: it waits in the agent's queue, and the agent has been told it
        // is not ready.
        wait_for_state(&journal, id, "a live create", |s| {
            s.status == SessionStatus::Provisioning
        })
        .await;
        assert!(
            !f.agent.signals().iter().any(|s| s.starts_with("get:")),
            "nothing may ask the vendor for a runtime it has not been told to build"
        );

        f.agent.release_creates();
        // The create's completion is what releases the queue: the session
        // announces readiness and the agent drains straight into a turn.
        // Asserted on the journal, not the status — a fast turn ends before a
        // poll could catch it `Running`, and only the event says it happened.
        wait_for_events(&journal, id, "the queued message running", |events| {
            events
                .iter()
                .any(|e| matches!(e, SessionDomainEvent::TurnBegan { .. }))
        })
        .await;
    }

    /// The capability an in-memory gate cannot have: a create the process died
    /// inside is finished by the next incarnation.
    ///
    /// Re-attempting is safe here and only here — `Provisioning` means no turn
    /// has ever run, so there is no work in the workspace to destroy.
    #[tokio::test]
    async fn a_create_interrupted_by_a_restart_is_re_attempted_at_load() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        // A journal that stops at `ProvisioningStarted` is exactly what a
        // process killed mid-create leaves behind. Seeded rather than produced
        // by a first incarnation, because the detached create holds a reference
        // to the actor it reports to — so dropping a handle is not death.
        let journal: Arc<dyn horsie_actor::Journal> =
            Arc::new(horsie_actor::InMemoryJournal::new());
        journal
            .persist(
                &SessionActor::persistence_id_for(id),
                &[
                    serde_json::to_vec(&SessionDomainEvent::ProvisioningStarted { at_ms: 0 })
                        .unwrap(),
                ],
            )
            .await
            .unwrap();

        let _session2 = horsie_actor::spawn_root(
            SessionActor::new(
                id,
                actor_spec_fixture(),
                f.deps.clone(),
                spawn_deaf_supervisor(),
                crate::sessions::Positions::default(),
            ),
            journal.clone(),
        );
        wait_for_state(&journal, id, "the runtime finished after a restart", |s| {
            s.status != SessionStatus::Provisioning
        })
        .await;
        assert!(
            f.agent
                .signals()
                .iter()
                .any(|s| s == &format!("create:{id}")),
            "the interrupted create has to be finished by somebody"
        );
    }

    /// #239: the message that retries a failed create has to *build* the
    /// runtime, not ask for one that was never built.
    ///
    /// The vendor is missing for the create and present for the retry, which is
    /// the canonical retryable failure — a laptop agent that was offline for a
    /// moment must not cost a session permanently.
    #[tokio::test]
    async fn a_message_after_a_failed_create_provisions_instead_of_dying() {
        let f = actor_fixture().await;
        f.deps.provider_registry.write().unwrap().insert(
            "mock".to_string(),
            Arc::new(EchoProvider) as Arc<dyn LlmProvider>,
        );
        let link = f
            .deps
            .vendors
            .write()
            .unwrap()
            .remove("mock")
            .expect("the fixture registers one");

        let id = Uuid::new_v4();
        let (session, journal) = spawn_unprovisioned(&f, id);
        session
            .tell(SessionCommand::Lifecycle(LifecycleCommand::Provision))
            .await
            .unwrap();
        let failed = wait_for_state(
            &journal,
            id,
            "a create that could not reach a vendor",
            |s| matches!(s.status, SessionStatus::ProvisioningFailed { .. }),
        )
        .await;
        assert!(
            failed
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("unavailable")),
            "the vendor's own reason survives: {:?}",
            failed.last_error
        );

        // The vendor comes back, and the user does what the UI tells them to.
        f.deps
            .vendors
            .write()
            .unwrap()
            .insert("mock".to_string(), link);
        let (tx, _rx) = oneshot::channel();
        session
            .tell(SessionCommand::Turn(TurnCommand::UserMessage {
                agent_id: None,
                text: "try again".into(),
                reply: tx,
            }))
            .await
            .unwrap();

        wait_for_state(&journal, id, "the retry building a runtime", |s| {
            !matches!(s.status, SessionStatus::ProvisioningFailed { .. })
        })
        .await;
        assert!(
            f.agent
                .signals()
                .iter()
                .any(|s| s == &format!("create:{id}")),
            "the retry has to build the runtime, not ask for one: {:?}",
            f.agent.signals()
        );
        wait_for_events(&journal, id, "the queued message running", |events| {
            events
                .iter()
                .any(|e| matches!(e, SessionDomainEvent::TurnBegan { .. }))
        })
        .await;
        let ran = crate::sessions::events::fold_session_state(&journal, id).await;
        assert!(
            !matches!(ran.status, SessionStatus::Unrecoverable { .. }),
            "retrying must never be what kills the session: {:?}",
            ran.status
        );
    }

    /// A workflow run takes no messages, so the message-shaped retry can never
    /// reach one. Without this it would sit in `ProvisioningFailed` forever —
    /// where before it at least died — so loading a session whose create failed
    /// re-attempts it, which is also how a run gets a second chance at all.
    #[tokio::test]
    async fn loading_a_session_whose_create_failed_re_attempts_it() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        let journal: Arc<dyn horsie_actor::Journal> =
            Arc::new(horsie_actor::InMemoryJournal::new());
        journal
            .persist(
                &SessionActor::persistence_id_for(id),
                &[
                    serde_json::to_vec(&SessionDomainEvent::ProvisioningStarted { at_ms: 0 })
                        .unwrap(),
                    serde_json::to_vec(&SessionDomainEvent::ProvisioningFailed {
                        at_ms: 1,
                        error: "runtime vendor unavailable: vendor 'mock' is not connected".into(),
                        terminal: false,
                    })
                    .unwrap(),
                ],
            )
            .await
            .unwrap();

        let _session = horsie_actor::spawn_root(
            SessionActor::new(
                id,
                actor_spec_fixture(),
                f.deps.clone(),
                spawn_deaf_supervisor(),
                crate::sessions::Positions::default(),
            ),
            journal.clone(),
        );
        wait_for_state(&journal, id, "the create re-attempted at load", |s| {
            !matches!(s.status, SessionStatus::ProvisioningFailed { .. })
        })
        .await;
        assert!(
            f.agent
                .signals()
                .iter()
                .any(|s| s == &format!("create:{id}")),
            "the runtime has to actually get built: {:?}",
            f.agent.signals()
        );
    }

    #[tokio::test]
    async fn prepare_offload_refuses_while_a_run_is_in_flight() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        f.deps
            .runtimes
            .create(&id.to_string(), "mock", &actor_spec_fixture())
            .await
            .expect("create");
        let provider = BlockingProvider::new();
        f.deps
            .provider_registry
            .write()
            .unwrap()
            .insert("mock".to_string(), provider.clone() as Arc<dyn LlmProvider>);

        let parent = spawn_deaf_supervisor();
        let journal: Arc<dyn horsie_actor::Journal> =
            Arc::new(horsie_actor::InMemoryJournal::new());
        let session = horsie_actor::spawn_root(
            SessionActor::new(
                id,
                actor_spec_fixture(),
                f.deps.clone(),
                parent,
                crate::sessions::Positions::default(),
            ),
            journal,
        );

        session
            .ask(|reply| {
                SessionCommand::Turn(TurnCommand::UserMessage {
                    agent_id: None,
                    text: "go".into(),
                    reply,
                })
            })
            .await
            .unwrap()
            .unwrap();

        let offloadable = session
            .ask(|reply| SessionCommand::Lifecycle(LifecycleCommand::PrepareOffload { reply }))
            .await
            .unwrap();
        assert!(
            !offloadable,
            "a run in flight must never be offloaded out from under itself"
        );
        assert!(
            f.agent
                .signals()
                .iter()
                .all(|s| !s.starts_with("hibernate:")),
            "refusing must not touch the runtime: {:?}",
            f.agent.signals()
        );

        // Refusing must leave the actor exactly as it was, still answering
        // commands normally rather than having torn itself down.
        provider.release();
        let (tx, rx) = oneshot::channel();
        session
            .tell(SessionCommand::Read(ReadCommand::UsageStats { reply: tx }))
            .await
            .unwrap();
        rx.await.unwrap();
    }

    #[tokio::test]
    async fn prepare_offload_refuses_with_an_active_subagent() {
        let gate = BlockingProvider::new();
        let (f, session, id, journal) = spawn_session_with_provider(gate.clone()).await;
        let _sub = spawn_sub(&session, "w", "t").await;
        wait_for_tree(&journal, id, |t| t.has_active()).await;

        let offloadable = session
            .ask(|reply| SessionCommand::Lifecycle(LifecycleCommand::PrepareOffload { reply }))
            .await
            .unwrap();
        assert!(!offloadable, "an active subagent must block offload");
        assert!(
            f.agent
                .signals()
                .iter()
                .all(|s| !s.starts_with("hibernate:")),
            "refusing must not touch the runtime"
        );
        gate.release();
    }

    #[test]
    fn a_gone_runtime_is_terminal() {
        let s = fold(vec![SessionDomainEvent::SessionFailed {
            at_ms: 0,
            reason: "vendor has no runtime".into(),
        }]);
        assert!(matches!(s.status, SessionStatus::Unrecoverable { .. }));
    }
}
