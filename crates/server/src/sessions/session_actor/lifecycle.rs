//! Getting and releasing this session's sandbox.
//!
//! A precondition rather than a participant: nothing else here talks to it,
//! and its whole interface to the rest of the session is the provisioning
//! phase in the folded state, which the turn boundary checks before asking any
//! runner what to start.
//!
//! The create is journaled *before* the vendor is called and runs off the
//! mailbox, so an interrupted create is discoverable at load and a session
//! stays able to answer reads, stops and deletes for the minutes one takes.

use super::runner::state::ProvisionPhase;
use super::{
    CommandEffect, LifecycleCommand, SessionActor, SessionCommand, SessionEvent, SessionState,
    runner,
};
use crate::runtime_manager::RuntimeError;
use crate::sessions::addressing::SessionInbox;
use horsie_actor::ActorContext;
use horsie_models::now_ms;

impl SessionActor {
    pub(super) async fn handle_lifecycle(
        &mut self,
        state: &SessionState,
        cmd: LifecycleCommand,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionEvent> {
        match cmd {
            LifecycleCommand::Provision => {
                // Provision only while no runtime has ever been confirmed: a
                // session just created, one found still in flight at load
                // because the process died inside its create, and one whose
                // create failed on something retryable. `Ready` means a create
                // already succeeded, and re-running one would rebuild a
                // workspace someone may be using — the thing this design
                // exists to make impossible.
                if matches!(state.provisioning.phase, ProvisionPhase::Ready)
                    || state.fatal.is_some()
                {
                    return CommandEffect::none();
                }
                let runtimes = self.deps().runtimes.clone();
                let session = self.id.to_string();
                let vendor = self.spec().vendor.clone();
                let spec = self.spec().clone();
                let me = self.me(ctx);
                // Minted here and journaled below in the same breath, so the
                // sandbox this create starts and the entry that records it
                // agree on one name. Reading the clock twice would give the
                // spawned task an identity the journal never saw.
                let at_ms = now_ms();
                let incarnation = at_ms.to_string();
                // Off the mailbox: a real create runs for minutes, and this
                // actor has to keep answering reads, stops and deletes
                // throughout. The phase it just journaled is what holds the
                // turn back meanwhile.
                tokio::spawn(async move {
                    let (error, terminal, detail) = match runtimes
                        .create(&session, &incarnation, &vendor, &spec)
                        .await
                    {
                        Ok(detail) => (None, false, detail),
                        // Exactly the split `get` makes: only a live vendor
                        // refusing to produce the runtime is terminal. An
                        // offline vendor or a failed token mint is a bad
                        // moment, not a dead session.
                        Err(e @ RuntimeError::Gone(_)) => (Some(e.to_string()), true, None),
                        Err(e @ (RuntimeError::Unavailable(_) | RuntimeError::Provision(_))) => {
                            (Some(e.to_string()), false, None)
                        }
                    };
                    // Before the outcome, and separately from it: the vendor
                    // described the runtime it accepted, and that sentence
                    // belongs to the wait rather than to how the wait ended.
                    if let Some(detail) = detail {
                        let _ = me
                            .tell(SessionCommand::Lifecycle(
                                LifecycleCommand::NarrateProvisioning { detail },
                            ))
                            .await;
                    }
                    let _ = me
                        .tell(SessionCommand::Lifecycle(
                            LifecycleCommand::FinishProvisioning { error, terminal },
                        ))
                        .await;
                });
                CommandEffect::persist(vec![SessionEvent::ProvisioningStarted { at_ms }])
            }
            LifecycleCommand::NarrateProvisioning { detail } => {
                // Only while a create is actually outstanding. A vendor's word
                // that lands after the outcome would say a session is still
                // coming up when it is already running.
                if !matches!(state.provisioning.phase, ProvisionPhase::InFlight) {
                    return CommandEffect::none();
                }
                CommandEffect::persist(vec![SessionEvent::ProvisioningProgress {
                    at_ms: now_ms(),
                    detail,
                }])
            }
            LifecycleCommand::FinishProvisioning { error, terminal } => {
                let event = match error {
                    None => SessionEvent::ProvisioningSucceeded { at_ms: now_ms() },
                    Some(error) => SessionEvent::ProvisioningFailed {
                        at_ms: now_ms(),
                        error,
                        terminal,
                    },
                };
                // The runtime landed, so whatever queued behind it starts now.
                // A failure drains nothing: the boundary is closed, the
                // messages stay owed, and the next thing the user sends is
                // what tries again.
                self.persist_and_advance(state, vec![event], ctx).await
            }
            LifecycleCommand::PrepareOffload { reply } => {
                // Work started while the supervisor was deciding: refuse, and
                // let the idle clock start again. Asked of every runner rather
                // than hand-written here, so a runner kind added later makes
                // itself heard instead of being silently unloadable.
                if runner::session_busy(state) {
                    let _ = reply.send(false);
                    return CommandEffect::none();
                }
                self.stop_agents().await;
                self.deps()
                    .runtimes
                    .hibernate(&self.id.to_string(), &self.spec().vendor)
                    .await;
                // Answered as this actor's last act: it writes nothing after
                // returning, so the supervisor can drop its reference the
                // moment it sees `true`.
                let _ = reply.send(true);
                CommandEffect::stop()
            }
            LifecycleCommand::Delete { reply } => {
                self.cancel_in_flight(state).await;
                self.stop_agents().await;
                self.deps()
                    .runtimes
                    .delete(&self.id.to_string(), &self.spec().vendor)
                    .await;
                let _ = reply.send(());
                CommandEffect::stop()
            }
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
    //! Getting and releasing the sandbox through the actor: what a create
    //! does, what an interrupted one replays as, and what refuses an offload.
    use super::super::testing::*;
    use super::super::*;
    use crate::sessions::session_actor::testing::seed_session;
    use crate::sessions::spec::SessionStatus;
    use horsie_actor::ReplyTo;
    use tokio::sync::oneshot;

    use horsie_agentcore::LlmProvider;

    use std::sync::Arc;
    use uuid::Uuid;

    /// A message that arrives while the create is still in flight must queue,
    /// not ask a vendor that has never heard of the runtime. The wait is the
    /// session's own journaled phase, which is what makes it survive a restart
    /// where an in-memory gate would not.
    #[tokio::test]
    async fn a_message_arriving_mid_create_waits_for_the_runtime() {
        let f = actor_fixture_blocking_creates().await;
        f.deps.provider_registry.write().unwrap().insert(
            "mock".to_string(),
            crate::sessions::spec::ModelEntry::provider_only(
                Arc::new(EchoProvider) as Arc<dyn LlmProvider>
            ),
        );
        let id = Uuid::new_v4();
        let (session, journal) = spawn_unprovisioned(&f, id).await;

        session
            .tell(SessionCommand::Lifecycle(LifecycleCommand::Provision))
            .await
            .unwrap();
        let (tx, _rx) = oneshot::channel();
        session
            .tell(SessionCommand::Turn(TurnCommand::UserMessage {
                agent_id: None,
                text: "hello".into(),
                reply: ReplyTo::from_sender(tx),
            }))
            .await
            .unwrap();

        // The message is owed an answer, not spent on a runtime that does not
        // exist: it waits in the agent's queue, and the agent has been told it
        // is not ready.
        wait_for_state(&journal, id, "a live create", |s| {
            s.status() == SessionStatus::Provisioning
        })
        .await;
        assert!(
            !f.agent.signals().iter().any(|s| s.starts_with("get:")),
            "nothing may ask the vendor for a runtime it has not been told to build"
        );

        f.agent.release_creates();
        // The create's completion is what releases the queue. Asserted on the
        // journal, not the status — a fast turn ends before a poll could catch
        // it `Running`, and only the event says it happened.
        wait_for_events(&journal, id, "the queued message running", |events| {
            events
                .iter()
                .any(|e| matches!(e, SessionEvent::TurnBegan { .. }))
        })
        .await;
    }

    /// The capability an in-memory gate cannot have: a create the process died
    /// inside is finished by the next incarnation. Re-attempting is safe here
    /// and only here — an in-flight phase means no turn has ever run.
    #[tokio::test]
    async fn a_create_interrupted_by_a_restart_is_re_attempted_at_load() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        // A journal that stops at `ProvisioningStarted` is exactly what a
        // process killed mid-create leaves behind.
        let journal = f.journal();
        let _session2 = seed_session(
            &f,
            id,
            actor_spec_fixture(),
            &[SessionEvent::ProvisioningStarted { at_ms: 0 }],
        )
        .await;
        wait_for_state(&journal, id, "the runtime finished after a restart", |s| {
            s.status() != SessionStatus::Provisioning
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

    /// The message that retries a failed create has to *build* the runtime,
    /// not ask for one that was never built.
    #[tokio::test]
    async fn a_message_after_a_failed_create_provisions_instead_of_dying() {
        let f = actor_fixture().await;
        f.deps.provider_registry.write().unwrap().insert(
            "mock".to_string(),
            crate::sessions::spec::ModelEntry::provider_only(
                Arc::new(EchoProvider) as Arc<dyn LlmProvider>
            ),
        );
        let link = f
            .deps
            .vendors
            .write()
            .unwrap()
            .remove("mock")
            .expect("the fixture registers one");

        let id = Uuid::new_v4();
        let (session, journal) = spawn_unprovisioned(&f, id).await;
        session
            .tell(SessionCommand::Lifecycle(LifecycleCommand::Provision))
            .await
            .unwrap();
        let failed = wait_for_state(
            &journal,
            id,
            "a create that could not reach a vendor",
            |s| matches!(s.status(), SessionStatus::ProvisioningFailed { .. }),
        )
        .await;
        assert!(
            failed
                .last_error()
                .is_some_and(|e| e.contains("unavailable")),
            "the vendor's own reason survives: {:?}",
            failed.last_error()
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
                reply: ReplyTo::from_sender(tx),
            }))
            .await
            .unwrap();

        wait_for_state(&journal, id, "the retry building a runtime", |s| {
            !matches!(s.status(), SessionStatus::ProvisioningFailed { .. })
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
                .any(|e| matches!(e, SessionEvent::TurnBegan { .. }))
        })
        .await;
        let ran = crate::sessions::events::fold_session_state(&journal, id).await;
        assert!(
            !matches!(ran.status(), SessionStatus::Unrecoverable { .. }),
            "retrying must never be what kills the session: {:?}",
            ran.status()
        );
    }

    /// A workflow run takes no messages, so the message-shaped retry can never
    /// reach one. Loading a session whose create failed re-attempts it, which
    /// is also how a run gets a second chance at all.
    #[tokio::test]
    async fn loading_a_session_whose_create_failed_re_attempts_it() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        let journal = f.journal();
        let _session = seed_session(
            &f,
            id,
            actor_spec_fixture(),
            &[
                SessionEvent::ProvisioningStarted { at_ms: 0 },
                SessionEvent::ProvisioningFailed {
                    at_ms: 1,
                    error: "runtime vendor unavailable: vendor 'mock' is not connected".into(),
                    terminal: false,
                },
            ],
        )
        .await;
        wait_for_state(&journal, id, "the create re-attempted at load", |s| {
            !matches!(s.status(), SessionStatus::ProvisioningFailed { .. })
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

    /// The whole of what a person has to go on while a session is
    /// provisioning: the vendor's own words reach the journal, before the
    /// outcome.
    #[tokio::test]
    async fn what_the_vendor_says_about_a_create_is_journaled() {
        let f = actor_fixture().await;
        f.deps.vendors.write().unwrap().insert(
            "mock".to_string(),
            Arc::new(BootingVendor) as Arc<dyn crate::runtime_vendor::RuntimeVendor>,
        );
        let id = Uuid::new_v4();
        let (session, journal) = spawn_unprovisioned(&f, id).await;
        session
            .tell(SessionCommand::Lifecycle(LifecycleCommand::Provision))
            .await
            .unwrap();

        let events = wait_for_events(&journal, id, "the create finishing", |events| {
            events
                .iter()
                .any(|e| matches!(e, SessionEvent::ProvisioningSucceeded { .. }))
        })
        .await;
        let said: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::ProvisioningProgress { detail, .. } => Some(detail.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            said,
            vec![BOOTING_CREATE.to_string()],
            "the vendor's account of the create has to reach the journal: {events:?}"
        );

        // And it belongs to the wait rather than to how the wait ended.
        let position = |pred: fn(&SessionEvent) -> bool| events.iter().position(pred);
        assert!(
            position(|e| matches!(e, SessionEvent::ProvisioningProgress { .. }))
                < position(|e| matches!(e, SessionEvent::ProvisioningSucceeded { .. })),
            "narration comes before the outcome: {events:?}"
        );
    }

    /// A vendor whose runtime is already up narrates nothing, and the session
    /// records nothing.
    #[tokio::test]
    async fn a_create_with_nothing_to_say_records_nothing() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        let (session, journal) = spawn_unprovisioned(&f, id).await;
        session
            .tell(SessionCommand::Lifecycle(LifecycleCommand::Provision))
            .await
            .unwrap();
        let events = wait_for_events(&journal, id, "the create finishing", |events| {
            events
                .iter()
                .any(|e| matches!(e, SessionEvent::ProvisioningSucceeded { .. }))
        })
        .await;
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SessionEvent::ProvisioningProgress { .. })),
            "{events:?}"
        );
    }

    /// A vendor's word that arrives after the create has settled is dropped.
    #[tokio::test]
    async fn narration_that_outlives_the_create_is_ignored() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        let (session, journal) = spawn_unprovisioned(&f, id).await;
        session
            .tell(SessionCommand::Lifecycle(LifecycleCommand::Provision))
            .await
            .unwrap();
        wait_for_events(&journal, id, "the create finishing", |events| {
            events
                .iter()
                .any(|e| matches!(e, SessionEvent::ProvisioningSucceeded { .. }))
        })
        .await;

        session
            .tell(SessionCommand::Lifecycle(
                LifecycleCommand::NarrateProvisioning {
                    detail: BOOTING_CREATE.into(),
                },
            ))
            .await
            .unwrap();
        // Round-tripped through the mailbox so the assertion is not racing the
        // command it is about.
        let _ = session
            .ask(|reply| SessionCommand::Read(ReadCommand::Snapshot { reply }))
            .await;
        let events = crate::sessions::events::session_events(&journal, id).await;
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SessionEvent::ProvisioningProgress { .. })),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn prepare_offload_refuses_while_a_run_is_in_flight() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        f.deps
            .runtimes
            .create(&id.to_string(), "i1", "mock", &actor_spec_fixture())
            .await
            .expect("create");
        let provider = BlockingProvider::new();
        f.deps.provider_registry.write().unwrap().insert(
            "mock".to_string(),
            crate::sessions::spec::ModelEntry::provider_only(
                provider.clone() as Arc<dyn LlmProvider>
            ),
        );

        let session = f.start(id, actor_spec_fixture()).await;

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
            .tell(SessionCommand::Read(ReadCommand::UsageStats {
                reply: ReplyTo::from_sender(tx),
            }))
            .await
            .unwrap();
        rx.await.unwrap();
    }

    #[tokio::test]
    async fn prepare_offload_refuses_with_an_active_subagent() {
        let gate = BlockingProvider::new();
        let (f, session, id, journal) = spawn_session_with_provider(gate.clone()).await;
        let _sub = spawn_sub(&session, id, "w", "t").await;
        wait_for_tree(&journal, id, any_sub_active).await;

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
}
