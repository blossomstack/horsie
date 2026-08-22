//! How a session command finds the actor it is for, on whichever node that is.
//!
//! A clustered actor is reached through one reference for its whole type, so a
//! command has to say which actor it is for: [`Addressed`] is a command plus
//! that id. The [`Shard`] impls below are the only things that read it.
//!
//! **A session's id names its account as well.** A node that has never hosted a
//! session still has to build one, and building one needs that account's
//! services — its runtime manager, its provider registry, its plugin library.
//! None of that is derivable from a session uuid. The account therefore travels
//! *with* the id rather than being looked up from it, which is what lets a
//! recipe stay an ordinary synchronous function.
//!
//! **The shard id is the session alone.** Placement is decided over it, so one
//! session is one unit and an account's sessions spread across the cluster
//! rather than piling onto whichever node its supervisor is on. No hash and no
//! bucket count: `horsie-actor` places by a pure function over the live set with
//! no per-shard state to bound, so bucketing would buy a shorter address segment
//! and cost a number two nodes could disagree about.

use crate::projects::ProjectId;
use crate::sessions::session_actor::SessionCommand;
use crate::sessions::supervisor::SessionSupervisorCommand;
use horsie_actor::{ActorRef, ReplyTo, Shard, TellError};
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

/// Whether this node may still act on the actors it hosts.
///
/// `None` on a single-node deployment, which never stands down. On a clustered
/// node it is `ClusterNode::serving_watch()`, which goes false while the node
/// cannot see a leader — what being in a minority looks like from inside one.
/// A node that has lost touch with a quorum cannot know whether its instances
/// have been given to somebody else, so it must stop answering from them.
///
/// A watch rather than the node itself because these references are cloned per
/// request, and reading a watch is a load rather than a lock.
pub type Serving = Option<tokio::sync::watch::Receiver<bool>>;

/// Whether `serving` permits sending right now.
///
/// Absent means unclustered, which is never gated — `None` must not read as
/// "not serving", or a single-node deployment would refuse every request.
fn may_send(serving: &Serving) -> bool {
    serving.as_ref().is_none_or(|rx| *rx.borrow())
}

/// How a failed send reads once this node has stood down.
///
/// A node standing down fails every caller still waiting for an answer, so an
/// ask in flight across that moment comes back `MailboxClosed` — which at the
/// call site is indistinguishable from an actor that really is gone. It is not
/// the same thing: nothing is broken, this node simply stopped being allowed to
/// answer, and the caller should retry elsewhere rather than report a fault.
/// Asking again after the failure is what tells the two apart, because the ask
/// before it passed.
fn refusal(serving: &Serving, e: TellError) -> TellError {
    if may_send(serving) {
        e
    } else {
        TellError::Undeliverable
    }
}

/// Between an account and what it owns, in a rendered id.
///
/// Only ever written. Nothing reads one of these back apart, because every node
/// that needs the halves gets them off the command — it exists so an address
/// stays legible in a log.
const SEP: char = '|';

/// Which session, and whose.
///
/// The account is not decoration: a recipe is handed this and nothing else, and
/// it cannot build a session without knowing which account's services to build
/// it against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEntityId {
    pub project: ProjectId,
    pub session: Uuid,
}

impl fmt::Display for SessionEntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{SEP}{}", self.project, self.session)
    }
}

/// A command and the actor it is for.
///
/// A wrapper rather than an id on each of the ~79 command variants. The cost is
/// that a wrong id is a runtime mistake rather than a type error; the reference
/// types below are what keep that mistake in one place instead of at every send
/// site.
#[derive(Serialize, Deserialize)]
pub struct Addressed<Id, C> {
    pub entity: Id,
    pub cmd: C,
}

/// What a supervisor's mailbox accepts: a command, and whose list it is for.
pub type SupervisorInbox = Addressed<ProjectId, SessionSupervisorCommand>;

/// What a session's mailbox accepts: a command, and which session it is for.
pub type SessionInbox = Addressed<SessionEntityId, SessionCommand>;

/// One account's session list.
pub struct SupervisorShard;

impl Shard for SupervisorShard {
    type Command = SupervisorInbox;
    type EntityId = ProjectId;
    /// The account, which is the same as the entity: an account has exactly one
    /// supervisor, so there is nothing coarser to group it with.
    type ShardId = ProjectId;
    const TYPE: &'static str = "session-supervisor";

    fn entity_id(cmd: &Self::Command) -> ProjectId {
        cmd.entity.clone()
    }

    fn shard_id(cmd: &Self::Command) -> ProjectId {
        cmd.entity.clone()
    }
}

/// One interactive session.
pub struct SessionShard;

impl Shard for SessionShard {
    type Command = SessionInbox;
    type EntityId = SessionEntityId;
    /// The session alone, so sessions are placed independently of each other and
    /// of the supervisor that lists them.
    type ShardId = Uuid;
    const TYPE: &'static str = "session";

    fn entity_id(cmd: &Self::Command) -> SessionEntityId {
        cmd.entity.clone()
    }

    fn shard_id(cmd: &Self::Command) -> Uuid {
        cmd.entity.session
    }
}

/// One project's supervisor, addressed rather than held.
///
/// Wraps once, here, so the places that send the supervisor a command keep
/// sending it a command. Each of them already had the project in hand — it is
/// what they resolved this reference from — so repeating it at every call site
/// would be ceremony that can be got wrong over a value that cannot.
#[derive(Clone)]
pub struct SupervisorRef {
    shard: ActorRef<SupervisorInbox>,
    project: ProjectId,
    serving: Serving,
}

impl SupervisorRef {
    #[must_use]
    pub fn new(shard: ActorRef<SupervisorInbox>, project: ProjectId, serving: Serving) -> Self {
        Self {
            shard,
            project,
            serving,
        }
    }

    #[must_use]
    pub fn project(&self) -> &ProjectId {
        &self.project
    }

    /// # Errors
    /// If the command could not be delivered — see [`ActorRef::tell`].
    pub async fn tell(&self, cmd: SessionSupervisorCommand) -> Result<(), TellError> {
        if !may_send(&self.serving) {
            return Err(TellError::Undeliverable);
        }
        self.shard
            .tell(self.addressed(cmd))
            .await
            .map_err(|e| refusal(&self.serving, e))
    }

    /// # Errors
    /// If the command could not be delivered, or nothing answered it.
    pub async fn ask<F, R>(&self, make: F) -> Result<R, TellError>
    where
        F: FnOnce(ReplyTo<R>) -> SessionSupervisorCommand,
        R: Send + 'static,
    {
        if !may_send(&self.serving) {
            return Err(TellError::Undeliverable);
        }
        self.shard
            .ask(|reply| self.addressed(make(reply)))
            .await
            .map_err(|e| refusal(&self.serving, e))
    }

    /// [`ask`](Self::ask), giving up after `within`.
    ///
    /// # Errors
    /// [`TellError::NoAnswer`] if the deadline passes first, plus anything
    /// [`ask`](Self::ask) can fail with.
    pub async fn ask_within<F, R>(
        &self,
        within: std::time::Duration,
        make: F,
    ) -> Result<R, TellError>
    where
        F: FnOnce(ReplyTo<R>) -> SessionSupervisorCommand,
        R: Send + 'static,
    {
        if !may_send(&self.serving) {
            return Err(TellError::Undeliverable);
        }
        self.shard
            .ask_within(within, |reply| self.addressed(make(reply)))
            .await
            .map_err(|e| refusal(&self.serving, e))
    }

    fn addressed(&self, cmd: SessionSupervisorCommand) -> SupervisorInbox {
        Addressed {
            entity: self.project.clone(),
            cmd,
        }
    }
}

/// One session, addressed rather than held.
///
/// The supervisor's `children` map used to be this: a handle to an instance,
/// maintained so nobody was ever given a corpse. A reference is a name now, and
/// a name that currently resolves to nothing reactivates what belongs there — so
/// there is nothing to maintain and nothing to invalidate on offload.
#[derive(Clone)]
pub struct SessionRef {
    shard: ActorRef<SessionInbox>,
    entity: SessionEntityId,
    serving: Serving,
}

impl SessionRef {
    #[must_use]
    pub fn new(
        shard: ActorRef<SessionInbox>,
        project: ProjectId,
        session: Uuid,
        serving: Serving,
    ) -> Self {
        Self {
            shard,
            entity: SessionEntityId { project, session },
            serving,
        }
    }

    /// The session this reference addresses.
    #[must_use]
    pub fn session(&self) -> Uuid {
        self.entity.session
    }

    /// # Errors
    /// If the command could not be delivered — see [`ActorRef::tell`].
    pub async fn tell(&self, cmd: SessionCommand) -> Result<(), TellError> {
        if !may_send(&self.serving) {
            return Err(TellError::Undeliverable);
        }
        self.shard
            .tell(self.addressed(cmd))
            .await
            .map_err(|e| refusal(&self.serving, e))
    }

    /// # Errors
    /// If the command could not be delivered, or nothing answered it.
    pub async fn ask<F, R>(&self, make: F) -> Result<R, TellError>
    where
        F: FnOnce(ReplyTo<R>) -> SessionCommand,
        R: Send + 'static,
    {
        if !may_send(&self.serving) {
            return Err(TellError::Undeliverable);
        }
        self.shard
            .ask(|reply| self.addressed(make(reply)))
            .await
            .map_err(|e| refusal(&self.serving, e))
    }

    /// [`ask`](Self::ask), giving up after `within`.
    ///
    /// # Errors
    /// [`TellError::NoAnswer`] if the deadline passes first, plus anything
    /// [`ask`](Self::ask) can fail with.
    pub async fn ask_within<F, R>(
        &self,
        within: std::time::Duration,
        make: F,
    ) -> Result<R, TellError>
    where
        F: FnOnce(ReplyTo<R>) -> SessionCommand,
        R: Send + 'static,
    {
        if !may_send(&self.serving) {
            return Err(TellError::Undeliverable);
        }
        self.shard
            .ask_within(within, |reply| self.addressed(make(reply)))
            .await
            .map_err(|e| refusal(&self.serving, e))
    }

    fn addressed(&self, cmd: SessionCommand) -> SessionInbox {
        Addressed {
            entity: self.entity.clone(),
            cmd,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// A minority node cannot know whether its instances have been given to
    /// somebody else, so every send from it must be refused.
    #[test]
    fn a_node_that_has_stood_down_may_not_send() {
        let (_tx, rx) = tokio::sync::watch::channel(false);
        assert!(!may_send(&Some(rx)));
    }

    #[test]
    fn a_serving_node_may_send() {
        let (_tx, rx) = tokio::sync::watch::channel(true);
        assert!(may_send(&Some(rx)));
    }

    /// `None` must not read as "not serving", or a single-node deployment —
    /// the default, and almost every deployment — would refuse every request it
    /// ever received.
    #[test]
    fn an_unclustered_node_is_never_gated() {
        assert!(may_send(&None));
    }

    /// Standing down mid-ask is not a fault, and must not be reported as one.
    ///
    /// The stand-down itself is what fails the caller — every waiter is dropped
    /// — so the error that arrives says "mailbox closed", which everywhere else
    /// means the actor is gone and something is wrong. Here it means this node
    /// stopped being allowed to answer, and the caller should ask another one.
    #[test]
    fn a_failure_while_standing_down_reads_as_undeliverable() {
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let down = Some(rx);
        assert!(matches!(
            refusal(&down, TellError::MailboxClosed),
            TellError::Undeliverable
        ));
        assert!(matches!(
            refusal(&down, TellError::NoAnswer),
            TellError::Undeliverable
        ));
    }

    /// And the converse, which is the half that keeps 500 meaning something: a
    /// serving node whose actor really has gone reports exactly that.
    #[test]
    fn a_failure_while_serving_keeps_its_reason() {
        let (_tx, rx) = tokio::sync::watch::channel(true);
        assert!(matches!(
            refusal(&Some(rx), TellError::MailboxClosed),
            TellError::MailboxClosed
        ));
        assert!(matches!(
            refusal(&None, TellError::MailboxClosed),
            TellError::MailboxClosed
        ));
    }

    /// Read per send rather than captured once: a node that stands down
    /// mid-connection has to stop, and one that recovers has to resume without
    /// every held reference being rebuilt.
    #[test]
    fn the_flag_is_read_at_each_send() {
        let (tx, rx) = tokio::sync::watch::channel(true);
        let serving = Some(rx);
        assert!(may_send(&serving));
        tx.send(false).unwrap();
        assert!(!may_send(&serving));
        tx.send(true).unwrap();
        assert!(may_send(&serving));
    }
}
