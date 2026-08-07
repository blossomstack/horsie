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
                actor.cancel_run().await;
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
