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
