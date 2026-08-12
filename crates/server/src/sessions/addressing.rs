//! How a session command finds the actor it is for, on whichever node that is.
//!
//! A clustered actor is reached through one reference for its whole type, so the
//! command has to carry its own address: [`Addressed`] is a command plus the
//! entity id it is for. The [`Shard`] impls below are the only things that read
//! it, and the only place that knows how an address is built.
//!
//! **An entity id is a bare id, and that is deliberate.** A shard address is
//! `/system/shard/<type>/<shard>/<entity>` with no third slot, so anything else
//! packed into it — an account, a tenant — would have to be decoded again by
//! whoever builds the actor. Nothing here needs that: an actor's persistence id
//! is `(type, entity)`, exactly as Akka builds
//! `PersistenceId(TypeKey.name, entityId)`, and everything else a session needs
//! it reads out of its own journal once it has recovered.
//!
//! **A shard id is a bucket, not a name.** `shard_id` is what placement is
//! decided over. Naming a session's shard after the session would put the same
//! id in the address twice; a hash bucket keeps it out and still spreads an
//! account's sessions across the cluster, so no account is confined to one
//! machine. What it costs is that a topology change moves a bucket of entities
//! rather than one, which nothing here cares about.

use crate::sessions::session_actor::SessionCommand;
use crate::sessions::supervisor::SessionSupervisorCommand;
use horsie_actor::{ActorRef, ReplyTo, Shard, TellError};
use serde::{Deserialize, Serialize};

/// How many buckets each shard type's entities are spread over.
///
/// Fixed for the life of a deployment: changing it re-buckets every entity, and
/// two nodes that disagreed would each build the same one. Large enough to
/// spread evenly over any node count worth running, small enough to stay
/// readable in a path.
const BUCKETS: u64 = 256;

/// The bucket `entity` belongs to.
///
/// FNV-1a rather than [`DefaultHasher`], which is explicitly not stable across
/// Rust releases. Every node has to agree on this, so it must not depend on the
/// compiler that built it.
///
/// [`DefaultHasher`]: std::collections::hash_map::DefaultHasher
fn bucket(entity: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in entity.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (hash % BUCKETS).to_string()
}

/// A command and the entity it is addressed to.
///
/// A wrapper rather than an id on each of the ~79 command variants. The cost is
/// that a wrong entity id is a runtime mistake rather than a type error; the
/// reference types below are what keep that mistake in one place instead of at
/// every send site.
#[derive(Serialize, Deserialize)]
pub struct Addressed<C> {
    /// The account id for a supervisor, the session uuid for a session.
    pub entity: String,
    pub cmd: C,
}

/// One account's session list.
pub struct SupervisorShard;

impl Shard for SupervisorShard {
    type Command = Addressed<SessionSupervisorCommand>;
    const TYPE: &'static str = "session-supervisor";

    fn entity_id(cmd: &Self::Command) -> String {
        cmd.entity.clone()
    }

    fn shard_id(cmd: &Self::Command) -> String {
        bucket(&cmd.entity)
    }
}

/// One interactive session.
pub struct SessionShard;

impl Shard for SessionShard {
    type Command = Addressed<SessionCommand>;
    const TYPE: &'static str = "session";

    fn entity_id(cmd: &Self::Command) -> String {
        cmd.entity.clone()
    }

    fn shard_id(cmd: &Self::Command) -> String {
        bucket(&cmd.entity)
    }
}

/// One account's supervisor, addressed rather than held.
///
/// Wraps once, here, so the ~117 places that send the supervisor a command keep
/// sending it a command. Each of them already had the account in hand — it is
/// what they resolved this reference from — so repeating it at every call site
/// would be ceremony that can be got wrong over a value that cannot.
#[derive(Clone)]
pub struct SupervisorRef {
    shard: ActorRef<Addressed<SessionSupervisorCommand>>,
    account: String,
}

impl SupervisorRef {
    #[must_use]
    pub fn new(shard: ActorRef<Addressed<SessionSupervisorCommand>>, account: String) -> Self {
        Self { shard, account }
    }

    /// The account this addresses.
    #[must_use]
    pub fn account(&self) -> &str {
        &self.account
    }

    /// # Errors
    /// If the command could not be delivered — see [`ActorRef::tell`].
    pub async fn tell(&self, cmd: SessionSupervisorCommand) -> Result<(), TellError> {
        self.shard.tell(self.addressed(cmd)).await
    }

    /// # Errors
    /// If the command could not be delivered, or nothing answered it.
    pub async fn ask<F, R>(&self, make: F) -> Result<R, TellError>
    where
        F: FnOnce(ReplyTo<R>) -> SessionSupervisorCommand,
        R: Send + 'static,
    {
        self.shard.ask(|reply| self.addressed(make(reply))).await
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
        self.shard
            .ask_within(within, |reply| self.addressed(make(reply)))
            .await
    }

    fn addressed(&self, cmd: SessionSupervisorCommand) -> Addressed<SessionSupervisorCommand> {
        Addressed {
            entity: self.account.clone(),
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
    shard: ActorRef<Addressed<SessionCommand>>,
    session: String,
}

impl SessionRef {
    #[must_use]
    pub fn new(shard: ActorRef<Addressed<SessionCommand>>, session: String) -> Self {
        Self { shard, session }
    }

    /// # Errors
    /// If the command could not be delivered — see [`ActorRef::tell`].
    pub async fn tell(&self, cmd: SessionCommand) -> Result<(), TellError> {
        self.shard.tell(self.addressed(cmd)).await
    }

    /// # Errors
    /// If the command could not be delivered, or nothing answered it.
    pub async fn ask<F, R>(&self, make: F) -> Result<R, TellError>
    where
        F: FnOnce(ReplyTo<R>) -> SessionCommand,
        R: Send + 'static,
    {
        self.shard.ask(|reply| self.addressed(make(reply))).await
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
        self.shard
            .ask_within(within, |reply| self.addressed(make(reply)))
            .await
    }

    fn addressed(&self, cmd: SessionCommand) -> Addressed<SessionCommand> {
        Addressed {
            entity: self.session.clone(),
            cmd,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Every command for one entity has to agree on its bucket, or that entity
    /// has two homes — so this is a pure function of the entity id and of
    /// nothing else.
    #[test]
    fn a_bucket_is_decided_by_the_entity_alone() {
        assert_eq!(bucket("sess-3"), bucket("sess-3"));
        assert_ne!(bucket("sess-3"), bucket("sess-4"));
    }

    /// Pinned rather than merely computed, because every node has to agree
    /// across builds: a bucket that moved between two versions of the server
    /// would put two live actors on one journal during a rolling restart.
    #[test]
    fn buckets_do_not_move_between_builds() {
        assert_eq!(bucket("acct-7"), "150");
        assert_eq!(bucket("sess-3"), "27");
        assert_eq!(bucket(""), "37");
    }

    /// Sessions spread, which is the whole reason the shard id is not the
    /// account.
    #[test]
    fn sessions_do_not_share_a_bucket() {
        let spread: std::collections::HashSet<String> = (0..64)
            .map(|i| bucket(&format!("sess-{i}")))
            .collect();
        assert!(
            spread.len() > 32,
            "64 sessions landed in only {} buckets",
            spread.len()
        );
    }
}
