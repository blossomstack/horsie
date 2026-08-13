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
use horsie_models::runtime::{RuntimeInboundMessage, RuntimeOutboundMessage};
use std::sync::Arc;
use uuid::Uuid;

/// What the server sends a runtime: tool calls, scans, hook runs.
///
/// The incarnation is in the name and not merely in the session id, so a
/// sandbox from a previous provision of the same session subscribes to a topic
/// nobody publishes to and is inert. Without it, two sandboxes claiming one
/// runtime would both receive a tool call and both execute it.
#[must_use]
pub fn runtime_in(
    bus: Arc<dyn Bus>,
    session: Uuid,
    incarnation: Uuid,
) -> Topic<RuntimeInboundMessage> {
    Topic::new(bus, format!("rt:{session}:{incarnation}:in"))
}

/// What a runtime sends back: its handshake, and one reply per request.
#[must_use]
pub fn runtime_out(
    bus: Arc<dyn Bus>,
    session: Uuid,
    incarnation: Uuid,
) -> Topic<RuntimeOutboundMessage> {
    Topic::new(bus, format!("rt:{session}:{incarnation}:out"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::bus::MemoryBus;

    fn bus() -> Arc<dyn Bus> {
        Arc::new(MemoryBus::new())
    }

    /// The two directions must never collide. They carry different types, so a
    /// shared name would deliver every tool call straight back to the server as
    /// an undecodable frame — silently dropped, and the runtime would hear
    /// nothing at all.
    #[test]
    fn the_two_directions_of_one_runtime_are_different_topics() {
        let (session, incarnation) = (Uuid::new_v4(), Uuid::new_v4());
        assert_ne!(
            runtime_in(bus(), session, incarnation).name(),
            runtime_out(bus(), session, incarnation).name()
        );
    }

    /// The point of the incarnation: a sandbox left over from an earlier
    /// provision of the same session is addressed by a name nothing publishes
    /// to, so it cannot receive — and therefore cannot re-run — a tool call.
    #[test]
    fn a_second_incarnation_of_one_session_is_a_different_topic() {
        let session = Uuid::new_v4();
        assert_ne!(
            runtime_in(bus(), session, Uuid::new_v4()).name(),
            runtime_in(bus(), session, Uuid::new_v4()).name()
        );
    }

    /// Two sessions never share a topic, whatever else is equal.
    #[test]
    fn two_sessions_never_share_a_topic() {
        let incarnation = Uuid::new_v4();
        assert_ne!(
            runtime_in(bus(), Uuid::new_v4(), incarnation).name(),
            runtime_in(bus(), Uuid::new_v4(), incarnation).name()
        );
    }
}
