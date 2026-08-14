//! Every topic this server publishes to, and the type that travels on each.
//!
//! One module, deliberately. A topic name written at a call site is a name that
//! can be written differently at the next call site, and the failure mode is
//! silence — a publisher and a subscriber that disagree by one character both
//! work perfectly and never meet. Naming them here once makes that a
//! compile-time question instead.
//!
//! The same reason the payload type is bound to the name rather than chosen by
//! the caller: a `Topic<T>` can only be built by one of these functions, so
//! nothing can publish a runtime message onto an account's session feed.

use super::{Bus, Topic};
use crate::sessions::AgentRevision;
use horsie_models::runtime::{RuntimeInboundMessage, RuntimeOutboundMessage};
use std::sync::Arc;

/// How far each of one session's agents has got, for whichever node answers
/// reads of them.
///
/// One topic per session rather than per agent: a reader of a session is
/// interested in its agents together, and a per-agent topic would mean a
/// subscription per subagent — several per session, appearing and vanishing as
/// subagents come and go.
///
/// **The account is in the name and is load-bearing.** One bus serves the whole
/// deployment, and a session id is the only other segment; without the account
/// a node could follow a session by id alone. The supervisor's session-list
/// check is what establishes a session belongs to the account asking, and this
/// name is what keeps the bus from being a way around it.
#[must_use]
pub fn session_revisions(bus: Arc<dyn Bus>, account: &str, session: &str) -> Topic<AgentRevision> {
    Topic::new(bus, format!("rev:{account}:{session}"))
}

/// What the server sends a runtime: tool calls, scans, hook runs.
///
/// `runtime` is a string rather than a `Uuid` because a runtime id is not always
/// one: the dial token's format admits "a session UUID **or** a vendor-minted
/// label", and typing this segment more narrowly than the id it names refuses a
/// legitimate runtime at the door.
///
/// **The account is in the name, and it is load-bearing precisely because a
/// runtime id is not always a UUID.** One bus serves the whole deployment, so
/// without this segment two accounts that minted the same vendor label would
/// share a topic — each one's tool calls reaching the other's sandbox. This is
/// the guarantee the per-account connected registry used to provide by being
/// per account; the topic namespace is where it lives now.
///
/// The incarnation is in the name and not merely in the session id, so a
/// sandbox from a previous provision of the same session subscribes to a topic
/// nobody publishes to and is inert. Without it, two sandboxes claiming one
/// runtime would both receive a tool call and both execute it.
#[must_use]
pub fn runtime_in(
    bus: Arc<dyn Bus>,
    account: &str,
    runtime: &str,
    incarnation: &str,
) -> Topic<RuntimeInboundMessage> {
    Topic::new(bus, format!("rt:{account}:{runtime}:{incarnation}:in"))
}

/// What a runtime sends back: its handshake, and one reply per request.
#[must_use]
pub fn runtime_out(
    bus: Arc<dyn Bus>,
    account: &str,
    runtime: &str,
    incarnation: &str,
) -> Topic<RuntimeOutboundMessage> {
    Topic::new(bus, format!("rt:{account}:{runtime}:{incarnation}:out"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::bus::MemoryBus;
    use uuid::Uuid;

    fn bus() -> Arc<dyn Bus> {
        Arc::new(MemoryBus::new())
    }

    /// The two directions must never collide. They carry different types, so a
    /// shared name would deliver every tool call straight back to the server as
    /// an undecodable frame — silently dropped, and the runtime would hear
    /// nothing at all.
    #[test]
    fn the_two_directions_of_one_runtime_are_different_topics() {
        let (session, incarnation) = (Uuid::new_v4().to_string(), "1755043200000");
        assert_ne!(
            runtime_in(bus(), "acct-1", &session, incarnation).name(),
            runtime_out(bus(), "acct-1", &session, incarnation).name()
        );
    }

    /// A session's incarnation is the `at_ms` of the `ProvisioningStarted` that
    /// began its provision — a decimal number, never a uuid. A topic built from
    /// anything else names a runtime nobody is on, and the failure is silence.
    #[test]
    fn a_topic_names_the_incarnation_the_session_actually_minted() {
        let session = Uuid::new_v4().to_string();
        assert_eq!(
            runtime_in(bus(), "acct-1", &session, "1755043200000").name(),
            format!("rt:acct-1:{session}:1755043200000:in")
        );
    }

    /// The point of the incarnation: a sandbox left over from an earlier
    /// provision of the same session is addressed by a name nothing publishes
    /// to, so it cannot receive — and therefore cannot re-run — a tool call.
    #[test]
    fn a_second_incarnation_of_one_session_is_a_different_topic() {
        let session = Uuid::new_v4().to_string();
        assert_ne!(
            runtime_in(bus(), "acct-1", &session, "1755043200000").name(),
            runtime_in(bus(), "acct-1", &session, "1755043200001").name()
        );
    }

    /// The account segment is the only thing standing between a session's
    /// counters and a node acting for somebody else, because the rest of the
    /// name is a session id and nothing more.
    #[test]
    fn two_accounts_do_not_share_a_session_revision_topic() {
        let session = Uuid::new_v4().to_string();
        assert_ne!(
            session_revisions(bus(), "acct-1", &session).name(),
            session_revisions(bus(), "acct-2", &session).name()
        );
    }

    /// A session's counters and its runtime traffic are separate feeds; a
    /// collision would deliver each side frames it cannot decode.
    #[test]
    fn a_sessions_counters_are_not_its_runtime_channel() {
        let session = Uuid::new_v4().to_string();
        assert_ne!(
            session_revisions(bus(), "acct-1", &session).name(),
            runtime_in(bus(), "acct-1", &session, "1755043200000").name()
        );
    }

    /// Two sessions never share a topic, whatever else is equal.
    #[test]
    fn two_sessions_never_share_a_topic() {
        let incarnation = "1755043200000";
        assert_ne!(
            runtime_in(bus(), "acct-1", &Uuid::new_v4().to_string(), incarnation).name(),
            runtime_in(bus(), "acct-1", &Uuid::new_v4().to_string(), incarnation).name()
        );
    }

    /// One bus serves every account, and a runtime id is not always a UUID —
    /// the dial token admits a vendor-minted label, and two accounts are free
    /// to mint the same one. Without the account segment they would share a
    /// topic, and one account's tool calls would reach the other's sandbox.
    /// This is what the per-account connected registry used to guarantee.
    #[test]
    fn two_accounts_that_minted_the_same_runtime_label_do_not_share_a_topic() {
        assert_ne!(
            runtime_in(bus(), "acct-1", "my-laptop", "1755043200000").name(),
            runtime_in(bus(), "acct-2", "my-laptop", "1755043200000").name()
        );
    }
}
