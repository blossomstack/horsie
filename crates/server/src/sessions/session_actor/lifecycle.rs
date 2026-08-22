//! Getting and releasing this session's sandboxes.
//!
//! A precondition rather than a participant: nothing else here talks to it, and
//! its whole interface to the rest of the session is [`RuntimeLifecycle::ready`],
//! which the turn boundary checks before asking any component what to start.
//!
//! Plural, since a sub session may ask for an environment of its own. Each
//! runtime is provisioned, narrated and failed independently, so a branch
//! booting a machine leaves the session it branched from working.
//!
//! The create is journaled *before* the vendor is called and runs off the
//! mailbox, so an interrupted create is discoverable at load and a session stays
//! able to answer reads, stops and deletes for the minutes one takes.

use super::component::Component;
use super::{
    AgentRuntime, CommandEffect, LifecycleCommand, ProvisioningState, SessionActor, SessionCommand,
    SessionDomainEvent, SessionState,
};
use crate::runtime_manager::RuntimeError;
use crate::sessions::addressing::SessionInbox;
use crate::sessions::run_forest::RuntimeChoice;
use crate::sessions::spec::RuntimeId;
use horsie_actor::{ActorContext, EventSourcedActor};
use horsie_models::now_ms;
#[cfg(test)]
use uuid::Uuid;

/// RuntimeLifecycle.
pub(super) struct RuntimeLifecycle;

impl RuntimeLifecycle {
    /// Whether this session has a runtime to run on — the single gate the turn
    /// boundary checks before asking any component what to start.
    ///
    /// Both answers are journaled facts, so they survive the process dying
    /// mid-create, which no in-memory gate could. `Failed` is the second of
    /// them: a create that failed on something retryable leaves a session with
    /// no runtime at all, and a turn started there would ask a vendor for one
    /// and be told, terminally, that it is gone.
    #[cfg(test)]
    pub(super) fn ready(state: &SessionState, agent: Uuid) -> bool {
        Self::ready_on(state.runtime_for(agent))
    }

    /// The same answer, from a runtime already resolved by some other walk —
    /// a step's, resolved through its run, or the session's own, resolved
    /// through its root entry whether that entry is a main agent or a run.
    pub(super) fn ready_on(runtime: AgentRuntime<'_>) -> bool {
        match runtime {
            // Per agent, not per session: what gates a turn is the runtime
            // *that agent* runs on. A sub session waiting on a machine of its
            // own must not hold up the session it branched from.
            AgentRuntime::On(_, rec) => !matches!(
                rec.provisioning,
                ProvisioningState::InFlight { .. } | ProvisioningState::Failed { .. }
            ),
            // It asked for no sandbox. Nothing to wait for, so nothing to gate
            // — this is the runtime-less session's whole interaction with this
            // component.
            AgentRuntime::Without => true,
            // It should have one and this state cannot yet say which. Wait.
            //
            // The distinction this arm exists for: reading it as "no runtime,
            // so nothing to wait for" let a session run its entire first turn
            // with no sandbox, silently, because the record naming its runtime
            // had not been journaled when the turn started.
            AgentRuntime::Pending => false,
        }
    }
}

impl RuntimeLifecycle {
    pub(super) async fn handle(
        actor: &mut SessionActor,
        state: &SessionState,
        cmd: LifecycleCommand,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionDomainEvent> {
        match cmd {
            LifecycleCommand::Provision { owner, env } => {
                if state.fatal.is_some() {
                    return CommandEffect::none();
                }
                // Which runtime this is about, and whether it needs building.
                // A record already `Ready` is not rebuilt — that would destroy
                // a workspace someone may be using, the thing this design
                // exists to make impossible. A record that is `InFlight` or
                // `Failed` is re-attempted, which is safe precisely because no
                // turn can have run under it.
                let existing = state
                    .runtimes_owned_by(owner)
                    .next()
                    .map(|(id, rec)| (id, rec.provisioning.clone(), rec.env.clone()));
                let (runtime, env, requested) = match (existing, env) {
                    (Some((_, ProvisioningState::Ready { .. }, _)), _) => {
                        return CommandEffect::none();
                    }
                    // A re-attempt: same id, same environment. Re-resolving one
                    // that may since have been edited would build a different
                    // sandbox under a session that believes it has the first.
                    (Some((id, _, env)), _) => (id, env, false),
                    // The first ask, with the environment the caller resolved.
                    // That is a sub session asking for one of its own.
                    (None, Some(env)) => (RuntimeId::generate(), *env, true),
                    // The session's own first ask. Its environment comes from
                    // the spec rather than the command, because the command is
                    // sent by the same write that records the spec — reading it
                    // at the send would read it before it was set.
                    (None, None) if owner == actor.id => {
                        match actor.spec().runtime_env() {
                            Some(env) => (RuntimeId::generate(), env, true),
                            // The session asked for no runtime. Nothing to
                            // build, and nothing wrong.
                            None => return CommandEffect::none(),
                        }
                    }
                    // Nothing to build one from and nobody to ask: this owner
                    // runs without a sandbox, which is a choice, not a fault.
                    (None, None) => return CommandEffect::none(),
                };
                let runtimes = actor.deps().runtimes.clone();
                let vendor = env.vendor.clone();
                let spec_env = env.clone();
                let id = runtime.to_string();
                let me = actor.me(ctx);
                // Minted here and journaled below in the same breath, so the
                // sandbox this create starts and the entry that records it
                // agree on one name. Reading the clock twice would give the
                // spawned task an identity the journal never saw.
                let at_ms = now_ms();
                let incarnation = at_ms.to_string();
                // Off the mailbox: a real create runs for minutes, and this
                // actor has to keep answering reads, stops and deletes
                // throughout. The status it just journaled is what holds the
                // turn back meanwhile.
                tokio::spawn(async move {
                    let (error, terminal, detail) =
                        match runtimes.create(&id, &incarnation, &vendor, &spec_env).await {
                            Ok(detail) => (None, false, detail),
                            // Exactly the split `get` makes: only a live vendor
                            // refusing to produce the runtime is terminal. An
                            // offline vendor or a failed token mint is a bad
                            // moment, not a dead session.
                            Err(e @ RuntimeError::Gone(_)) => (Some(e.to_string()), true, None),
                            Err(
                                e @ (RuntimeError::Unavailable(_) | RuntimeError::Provision(_)),
                            ) => (Some(e.to_string()), false, None),
                        };
                    // Before the outcome, and separately from it: the vendor
                    // described the runtime it accepted, and that sentence
                    // belongs to the wait rather than to how the wait ended.
                    if let Some(detail) = detail {
                        let _ = me
                            .tell(SessionCommand::Lifecycle(
                                LifecycleCommand::NarrateProvisioning { runtime, detail },
                            ))
                            .await;
                    }
                    let _ = me
                        .tell(SessionCommand::Lifecycle(
                            LifecycleCommand::FinishProvisioning {
                                runtime,
                                error,
                                terminal,
                            },
                        ))
                        .await;
                });
                let mut events = Vec::with_capacity(2);
                if requested {
                    events.push(SessionDomainEvent::RuntimeRequested {
                        at_ms,
                        runtime,
                        owner,
                        env,
                    });
                }
                events.push(SessionDomainEvent::ProvisioningStarted { at_ms, runtime });
                // The agents now know which runtime is theirs, even though it
                // is not up yet. Naming it here rather than only on success is
                // what makes a re-provision move a resident agent to the new
                // incarnation instead of leaving it on the old one.
                let mut next = state.clone();
                for e in &events {
                    next = SessionActor::apply_event(next, e.clone());
                }
                actor.repoint_agent_runtimes(&next);
                CommandEffect::persist(events)
            }
            LifecycleCommand::NarrateProvisioning { runtime, detail } => {
                // Only while that create is actually outstanding. A vendor's
                // word that lands after the outcome would say a runtime is
                // still coming up when it is already running.
                if !matches!(
                    state.runtimes.get(&runtime).map(|r| &r.provisioning),
                    Some(ProvisioningState::InFlight { .. })
                ) {
                    return CommandEffect::none();
                }
                CommandEffect::persist(vec![SessionDomainEvent::ProvisioningProgress {
                    at_ms: now_ms(),
                    runtime,
                    detail,
                }])
            }
            LifecycleCommand::FinishProvisioning {
                runtime,
                error,
                terminal,
            } => {
                let event = match error {
                    None => SessionDomainEvent::ProvisioningSucceeded {
                        at_ms: now_ms(),
                        runtime,
                    },
                    Some(error) => SessionDomainEvent::ProvisioningFailed {
                        at_ms: now_ms(),
                        runtime,
                        error,
                        terminal,
                    },
                };
                let next = SessionActor::apply_event(state.clone(), event.clone());
                // Before anything runs on it. An agent spawned while this
                // create was outstanding is holding `Pending`, and the flush
                // below is exactly what would have it take a turn — so the
                // answer has to reach it first.
                actor.repoint_agent_runtimes(&next);
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
                // Every runtime this session owns, not just its own: a sub
                // session that asked for a machine of its own has one too, and
                // an unloaded session leaving it warm is a bill nobody is
                // watching.
                for (runtime, rec) in state.runtimes.iter() {
                    actor
                        .deps()
                        .runtimes
                        .hibernate(&runtime.to_string(), &rec.env.vendor)
                        .await;
                }
                // Answered as this actor's last act: it writes nothing after
                // returning, so the supervisor can drop its reference the
                // moment it sees `true`.
                let _ = reply.send(true);
                CommandEffect::stop()
            }
            LifecycleCommand::Delete { reply } => {
                actor.cancel_in_flight(state).await;
                actor.stop_agents().await;
                // Deleting the session deletes every runtime under it. The
                // per-owner rule only decides what a *sub session's* deletion
                // takes with it; the session going away takes everything.
                for (runtime, rec) in state.runtimes.iter() {
                    actor
                        .deps()
                        .runtimes
                        .delete(&runtime.to_string(), &rec.env.vendor)
                        .await;
                }
                // Here, with the rest of the teardown, rather than at the
                // supervisor: an index entry outliving its transcript is worse
                // than no entry, because every search that hits it costs the
                // reader a call to discover the session is gone.
                actor.forget_agent_runs().await;
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
    fn on_load(state: &SessionState) -> Option<SessionCommand> {
        if state.fatal.is_some() {
            return None;
        }
        // The first unfinished one. One command per load rather than one per
        // record, because each re-attempt sends the next: a session with two
        // half-built sandboxes finishes both, and a session with none sends
        // nothing at all.
        state
            .runtimes
            .values()
            .find(|r| {
                matches!(
                    r.provisioning,
                    ProvisioningState::InFlight { .. } | ProvisioningState::Failed { .. }
                )
            })
            .map(|r| {
                SessionCommand::Lifecycle(LifecycleCommand::Provision {
                    owner: r.owner,
                    // The record carries the environment; a re-attempt must not
                    // re-resolve one that may since have been edited.
                    env: None,
                })
            })
    }

    fn busy(state: &SessionState) -> bool {
        state
            .runtimes
            .values()
            .any(|r| matches!(r.provisioning, ProvisioningState::InFlight { .. }))
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
            SessionDomainEvent::RuntimeRequested {
                runtime,
                owner,
                env,
                ..
            } => {
                state.runtimes.insert(
                    runtime,
                    crate::sessions::session_actor::RuntimeRecord {
                        env,
                        owner,
                        provisioning: ProvisioningState::Never,
                    },
                );
                // And the asking conversation now runs on it. The same write,
                // because a record nobody points at is a sandbox no agent can
                // reach — and one pointed at by an entry with no record is an
                // agent waiting on nothing.
                state
                    .forest
                    .point_at_runtime(owner, RuntimeChoice::On(runtime));
            }
            SessionDomainEvent::ProvisioningStarted { at_ms, runtime } => {
                // `at_ms` is the identity of this provision: every later
                // acquisition addresses the sandbox this create produced, so
                // the name has to outlive the create — and a reload.
                if let Some(rec) = state.runtimes.get_mut(&runtime) {
                    rec.provisioning = ProvisioningState::InFlight { at_ms };
                }
            }
            // Narration: it changes nothing, and the state it is describing
            // was set by the `ProvisioningStarted` before it. Journaled all the
            // same, so a client that arrives mid-create reads the same account
            // of the wait as one that watched it live.
            SessionDomainEvent::ProvisioningProgress { .. } => {}
            SessionDomainEvent::ProvisioningSucceeded { runtime, .. } => {
                if let Some(rec) = state.runtimes.get_mut(&runtime) {
                    let at_ms = rec.provisioning.at_ms().unwrap_or_default();
                    rec.provisioning = ProvisioningState::Ready { at_ms };
                }
            }
            SessionDomainEvent::ProvisioningFailed {
                runtime,
                error,
                terminal,
                ..
            } => {
                if let Some(rec) = state.runtimes.get_mut(&runtime) {
                    let at_ms = rec.provisioning.at_ms().unwrap_or_default();
                    rec.provisioning = ProvisioningState::Failed {
                        at_ms,
                        reason: error.clone(),
                    };
                }
                // A live vendor refusing to produce the runtime ends the
                // session; anything else stays a retryable provisioning fact.
                //
                // Session-wide even for a sub session's runtime: the vendor has
                // said this account cannot have the sandbox at all, which is
                // not a fact about which branch asked for it.
                if terminal {
                    state.fatal = Some(error);
                }
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
    use crate::sessions::spec::SessionStatus;

    use horsie_agentcore::LlmProvider;

    use std::sync::Arc;
    use uuid::Uuid;

    /// One fixed runtime for these tests. Which id it is never matters here —
    /// what matters is that the provisioning facts land on a record that
    /// exists, since a session now owns a map of them rather than one field.
    /// The session id `provisioning` seeds, so a readiness check can name the
    /// agent it is asking about.
    fn seeded_session() -> Uuid {
        Uuid::from_bytes([1; 16])
    }

    fn rt() -> crate::sessions::spec::RuntimeId {
        crate::sessions::spec::RuntimeId(Uuid::from_bytes([3; 16]))
    }

    /// The ask that has to precede any provisioning fact.
    ///
    /// A journal that starts at `ProvisioningStarted` describes a runtime the
    /// session never asked for — nothing names it, so nothing can be recovered
    /// against it. Seeding this first is what makes the seeded history one the
    /// running code could actually have written.
    fn asked_for_rt(owner: Uuid) -> SessionDomainEvent {
        SessionDomainEvent::RuntimeRequested {
            at_ms: 0,
            runtime: rt(),
            owner,
            env: actor_spec_fixture()
                .runtime_env()
                .expect("the fixture has a runtime"),
        }
    }

    /// A session that has asked for its own runtime, then whatever happened to
    /// it. The seed is two events rather than a hand-built state so these tests
    /// still run the fold they are about.
    fn provisioning(events: Vec<SessionDomainEvent>) -> SessionState {
        let session = Uuid::from_bytes([1; 16]);
        let mut seed = vec![SessionDomainEvent::RuntimeRequested {
            at_ms: 0,
            runtime: rt(),
            owner: session,
            env: crate::sessions::spec::SessionSpec::for_vendor("mock")
                .runtime_env()
                .expect("a vendor spec has a runtime"),
        }];
        seed.extend(events);
        let mut state = SessionState::default();
        // The root entry, seeded directly: this module's tests are about the
        // provisioning fold, and a session's own creation is another module's.
        state.forest.apply_root_agent(
            session,
            0,
            crate::sessions::run_forest::RuntimeChoice::Pending,
        );
        seed.into_iter()
            .fold(state, super::SessionActor::apply_event)
    }

    /// The identity a sandbox is addressed by, recovered from the journal.
    ///
    /// It has to come back on a reload: re-acquiring means reaching the sandbox
    /// this create started, and a server that forgot which provision that was
    /// would address one that never existed.
    #[test]
    fn a_provision_is_named_by_the_entry_that_began_it() {
        let started = provisioning(vec![SessionDomainEvent::ProvisioningStarted {
            at_ms: 1234,
            runtime: rt(),
        }]);
        assert_eq!(
            started.root_runtime().and_then(|r| r.provisioning.at_ms()),
            Some(1234)
        );
    }

    /// And a second provision replaces the first, which is the whole point: a
    /// sandbox left over from the earlier one answers to a name nothing
    /// publishes to any more, so it cannot be handed a tool call meant for its
    /// replacement.
    #[test]
    fn provisioning_again_gives_the_session_a_new_name() {
        let reprovisioned = provisioning(vec![
            SessionDomainEvent::ProvisioningStarted {
                at_ms: 1,
                runtime: rt(),
            },
            SessionDomainEvent::ProvisioningStarted {
                at_ms: 2,
                runtime: rt(),
            },
        ]);
        assert_eq!(
            reprovisioned
                .root_runtime()
                .and_then(|r| r.provisioning.at_ms()),
            Some(2)
        );
    }

    /// A session is `Provisioning` from the moment its create is journaled
    /// until the event that says how the create ended. Nothing else reaches
    /// this status, and no turn can run inside it.
    #[test]
    fn a_created_session_provisions_before_it_is_idle() {
        let started = provisioning(vec![SessionDomainEvent::ProvisioningStarted {
            at_ms: 0,
            runtime: rt(),
        }]);
        assert_eq!(started.status(), SessionStatus::Provisioning);

        let ready = provisioning(vec![
            SessionDomainEvent::ProvisioningStarted {
                at_ms: 0,
                runtime: rt(),
            },
            SessionDomainEvent::ProvisioningSucceeded {
                at_ms: 1,
                runtime: rt(),
            },
        ]);
        assert_eq!(ready.status(), SessionStatus::Idle);
    }

    /// A session that asked for no runtime is *ready*, not waiting.
    ///
    /// The two answers a create is gated on are deliberately different values:
    /// "this session has no sandbox" runs, "nobody has said yet" waits. Reading
    /// the first as the second parks a runtime-less session on a create that
    /// will never come; reading the second as the first runs an ordinary
    /// session's whole first turn with no sandbox and no complaint.
    #[test]
    fn a_session_that_asked_for_no_runtime_is_ready_rather_than_waiting() {
        let session = seeded_session();
        let spec = crate::sessions::spec::SessionSpec::runtime_less();
        assert!(spec.runtime_env().is_none(), "the fixture has no runtime");
        let mut state = SessionState::default();
        state.forest.apply_root_agent(
            session,
            0,
            crate::sessions::run_forest::RuntimeChoice::Without,
        );
        assert!(matches!(
            state.runtime_for(session),
            crate::sessions::session_actor::AgentRuntime::Without
        ));
        assert!(
            RuntimeLifecycle::ready(&state, session),
            "nothing is being waited for, so nothing is gated"
        );

        // And the contrast, on the same shape: unanswered is not the same fact.
        let mut unanswered = SessionState::default();
        unanswered.forest.apply_root_agent(
            session,
            0,
            crate::sessions::run_forest::RuntimeChoice::Pending,
        );
        assert!(
            !RuntimeLifecycle::ready(&unanswered, session),
            "an unresolved runtime must hold the turn, never run without one"
        );
    }

    /// The same, for a workflow session — whose root is a run, not a main
    /// agent.
    ///
    /// The two roots record the *same* fact and were not reading the spec the
    /// same way: an agent session's root asked what the spec wanted, a run's
    /// root always said "unanswered". A runtime-less run therefore waited
    /// forever on a create that correctly never came.
    #[test]
    fn a_runtime_less_workflow_session_is_ready_like_any_other() {
        use crate::sessions::run_forest::{RunId, RuntimeChoice};
        let session = seeded_session();
        let graph = std::sync::Arc::new(crate::sessions::workflow::WorkflowRunSpec {
            workflow: "nightly".into(),
            start: "first".into(),
            steps: Vec::new(),
            input: String::new(),
            max_steps: 1,
        });

        let mut without = SessionState::default();
        without.forest.apply_root_workflow(
            session,
            "nightly".into(),
            graph.clone(),
            0,
            RuntimeChoice::Without,
        );
        assert!(
            RuntimeLifecycle::ready_on(
                without.runtime_of_choice(without.forest.runtime_of_run(RunId(session)))
            ),
            "a run that asked for no runtime has nothing to wait for"
        );

        let mut pending = SessionState::default();
        pending.forest.apply_root_workflow(
            session,
            "nightly".into(),
            graph,
            0,
            RuntimeChoice::Pending,
        );
        assert!(
            !RuntimeLifecycle::ready_on(
                pending.runtime_of_choice(pending.forest.runtime_of_run(RunId(session)))
            ),
            "a run still waiting on its create must not start a step"
        );
    }

    /// The message the session was created with waits in its agent's queue
    /// rather than racing the vendor. What the session contributes is the gate:
    /// `ready` is false while provisioning and true once the runtime lands, and
    /// that answer is pushed to every agent.
    #[test]
    fn a_session_is_not_runnable_until_its_runtime_lands() {
        let waiting = provisioning(vec![SessionDomainEvent::ProvisioningStarted {
            at_ms: 0,
            runtime: rt(),
        }]);
        assert_eq!(waiting.status(), SessionStatus::Provisioning);
        assert!(
            !RuntimeLifecycle::ready(&waiting, seeded_session()),
            "nothing may run before the runtime exists"
        );

        let ready = SessionActor::apply_event(
            waiting,
            SessionDomainEvent::ProvisioningSucceeded {
                at_ms: 2,
                runtime: rt(),
            },
        );
        assert_eq!(ready.status(), SessionStatus::Idle);
        assert!(RuntimeLifecycle::ready(&ready, seeded_session()));
    }

    /// A create that failed on something retryable — an offline vendor, a
    /// GitHub token that could not be minted — leaves a session that can try
    /// again, and reports the reason the vendor actually gave rather than the
    /// "no such runtime" a later `get` would have invented.
    #[test]
    fn a_retryable_create_failure_is_reported_verbatim() {
        let s = provisioning(vec![
            SessionDomainEvent::ProvisioningStarted {
                at_ms: 0,
                runtime: rt(),
            },
            SessionDomainEvent::ProvisioningFailed {
                at_ms: 1,
                runtime: rt(),
                error: "runtime vendor unavailable: vendor 'local' is not connected".into(),
                terminal: false,
            },
        ]);
        // Its own status, not the `Failed` a failed *turn* leaves. The two look
        // identical to a reader and mean opposite things to the session: a
        // failed turn has a runtime and can simply run again, while this one has
        // no runtime at all and must build one before it can do anything.
        assert_eq!(
            s.status(),
            SessionStatus::ProvisioningFailed {
                reason: "runtime vendor unavailable: vendor 'local' is not connected".into(),
            }
        );
        assert!(s.last_error().is_some());
    }

    /// The status a failed create leaves must not let a turn start — the turn
    /// would ask for a runtime that was never built and be told, terminally,
    /// that it is gone. That is the whole defect in #239.
    #[test]
    fn a_failed_create_starts_no_turn() {
        let s = provisioning(vec![
            SessionDomainEvent::ProvisioningStarted {
                at_ms: 0,
                runtime: rt(),
            },
            SessionDomainEvent::ProvisioningFailed {
                at_ms: 1,
                runtime: rt(),
                error: "runtime vendor unavailable".into(),
                terminal: false,
            },
        ]);
        assert!(
            !RuntimeLifecycle::ready(&s, seeded_session()),
            "a session with no runtime must build one before it runs anything"
        );
    }

    /// A live vendor refusing to build the runtime is the terminal case, and
    /// the only one: it is the same `Gone` a `get` reports.
    #[test]
    fn a_terminal_create_failure_ends_the_session() {
        let s = provisioning(vec![
            SessionDomainEvent::ProvisioningStarted {
                at_ms: 0,
                runtime: rt(),
            },
            SessionDomainEvent::ProvisioningFailed {
                at_ms: 1,
                runtime: rt(),
                error: "runtime is gone: vendor cannot provision".into(),
                terminal: true,
            },
        ]);
        assert!(matches!(s.status(), SessionStatus::Unrecoverable { .. }));
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
            crate::sessions::spec::ModelEntry::provider_only(
                Arc::new(EchoProvider) as Arc<dyn LlmProvider>
            ),
        );
        let id = Uuid::new_v4();
        let (session, journal) = spawn_unprovisioned(&f, id).await;

        session
            .tell(SessionCommand::Lifecycle(LifecycleCommand::Provision {
                owner: seeded_session(),
                env: None,
            }))
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
        let journal = f.journal();
        let _session2 = seed_session(
            &f,
            id,
            actor_spec_fixture(),
            &[
                asked_for_rt(id),
                SessionDomainEvent::ProvisioningStarted {
                    at_ms: 0,
                    runtime: rt(),
                },
            ],
        )
        .await;
        wait_for_state(&journal, id, "the runtime finished after a restart", |s| {
            s.status() != SessionStatus::Provisioning
        })
        .await;
        assert!(
            f.agent.signals().iter().any(|s| s.starts_with("create:")),
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
            .tell(SessionCommand::Lifecycle(LifecycleCommand::Provision {
                owner: seeded_session(),
                env: None,
            }))
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
                .as_deref()
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
            f.agent.signals().iter().any(|s| s.starts_with("create:")),
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
            !matches!(ran.status(), SessionStatus::Unrecoverable { .. }),
            "retrying must never be what kills the session: {:?}",
            ran.status()
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
        let journal = f.journal();
        let _session = seed_session(
            &f,
            id,
            actor_spec_fixture(),
            &[
                asked_for_rt(id),
                SessionDomainEvent::ProvisioningStarted {
                    at_ms: 0,
                    runtime: rt(),
                },
                SessionDomainEvent::ProvisioningFailed {
                    at_ms: 1,
                    runtime: rt(),
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
            f.agent.signals().iter().any(|s| s.starts_with("create:")),
            "the runtime has to actually get built: {:?}",
            f.agent.signals()
        );
    }

    /// The whole of what a person has to go on while a session is provisioning.
    /// The vendor said "the machine is booting"; the create used to answer `()`,
    /// so the session journaled that it was provisioning and then that it was
    /// done, and every word in between was dropped at the manager.
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
            .tell(SessionCommand::Lifecycle(LifecycleCommand::Provision {
                owner: seeded_session(),
                env: None,
            }))
            .await
            .unwrap();

        let events = wait_for_events(&journal, id, "the create finishing", |events| {
            events
                .iter()
                .any(|e| matches!(e, SessionDomainEvent::ProvisioningSucceeded { .. }))
        })
        .await;
        let said: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                SessionDomainEvent::ProvisioningProgress { detail, .. } => Some(detail.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            said,
            vec![BOOTING_CREATE.to_string()],
            "the vendor's account of the create has to reach the journal: {events:?}"
        );

        // And it belongs to the wait rather than to how the wait ended: a line
        // recorded after `ProvisioningSucceeded` would say a session is still
        // coming up when it is already running.
        let position = |pred: fn(&SessionDomainEvent) -> bool| events.iter().position(pred);
        assert!(
            position(|e| matches!(e, SessionDomainEvent::ProvisioningProgress { .. }))
                < position(|e| matches!(e, SessionDomainEvent::ProvisioningSucceeded { .. })),
            "narration comes before the outcome: {events:?}"
        );
    }

    /// A vendor whose runtime is already up narrates nothing, and the session
    /// records nothing. There is no wait to describe, and a line invented for
    /// one would put a stage on screen that never happened.
    #[tokio::test]
    async fn a_create_with_nothing_to_say_records_nothing() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        let (session, journal) = spawn_unprovisioned(&f, id).await;
        session
            .tell(SessionCommand::Lifecycle(LifecycleCommand::Provision {
                owner: seeded_session(),
                env: None,
            }))
            .await
            .unwrap();
        let events = wait_for_events(&journal, id, "the create finishing", |events| {
            events
                .iter()
                .any(|e| matches!(e, SessionDomainEvent::ProvisioningSucceeded { .. }))
        })
        .await;
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SessionDomainEvent::ProvisioningProgress { .. })),
            "{events:?}"
        );
    }

    /// A vendor's word that arrives after the create has settled is dropped.
    /// Recording it would put a session that is already running back into
    /// "still coming up", which is the sort of thing a reader believes.
    #[tokio::test]
    async fn narration_that_outlives_the_create_is_ignored() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        let (session, journal) = spawn_unprovisioned(&f, id).await;
        session
            .tell(SessionCommand::Lifecycle(LifecycleCommand::Provision {
                owner: seeded_session(),
                env: None,
            }))
            .await
            .unwrap();
        // Asserted on the event, not the folded status: a session that has not
        // journaled its `ProvisioningStarted` yet folds to the default `Idle`,
        // so waiting for "not provisioning" answers before the create began.
        wait_for_events(&journal, id, "the create finishing", |events| {
            events
                .iter()
                .any(|e| matches!(e, SessionDomainEvent::ProvisioningSucceeded { .. }))
        })
        .await;

        session
            .tell(SessionCommand::Lifecycle(
                LifecycleCommand::NarrateProvisioning {
                    runtime: rt(),
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
                .any(|e| matches!(e, SessionDomainEvent::ProvisioningProgress { .. })),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn prepare_offload_refuses_while_a_run_is_in_flight() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        f.deps
            .runtimes
            .create(
                &id.to_string(),
                "i1",
                "mock",
                &actor_spec_fixture()
                    .runtime_env()
                    .expect("the fixture has a runtime"),
            )
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
        let _sub = spawn_sub(&session, "w", "t").await;
        wait_for_tree(&journal, id, |t| t.has_active_subs()).await;

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
        assert!(matches!(s.status(), SessionStatus::Unrecoverable { .. }));
    }
}
