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

/// What the server sends a runtime: tool calls, scans, hook runs.
///
/// `runtime` is a string rather than a `Uuid` because a runtime id is not always
/// one: the dial token's format admits "a session UUID **or** a vendor-minted
/// label", and typing this segment more narrowly than the id it names refuses a
/// legitimate runtime at the door.
///
/// The incarnation is in the name and not merely in the session id, so a
/// sandbox from a previous provision of the same session subscribes to a topic
/// nobody publishes to and is inert. Without it, two sandboxes claiming one
/// runtime would both receive a tool call and both execute it.
#[must_use]
pub fn runtime_in(
    bus: Arc<dyn Bus>,
    runtime: &str,
    incarnation: &str,
) -> Topic<RuntimeInboundMessage> {
    Topic::new(bus, format!("rt:{runtime}:{incarnation}:in"))
}

/// What a runtime sends back: its handshake, and one reply per request.
#[must_use]
pub fn runtime_out(
    bus: Arc<dyn Bus>,
    runtime: &str,
    incarnation: &str,
) -> Topic<RuntimeOutboundMessage> {
    Topic::new(bus, format!("rt:{runtime}:{incarnation}:out"))
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
            runtime_in(bus(), &session, incarnation).name(),
            runtime_out(bus(), &session, incarnation).name()
        );
    }

    /// A session's incarnation is the `at_ms` of the `ProvisioningStarted` that
    /// began its provision — a decimal number, never a uuid. A topic built from
    /// anything else names a runtime nobody is on, and the failure is silence.
    #[test]
    fn a_topic_names_the_incarnation_the_session_actually_minted() {
        let session = Uuid::new_v4().to_string();
        assert_eq!(
            runtime_in(bus(), &session, "1755043200000").name(),
            format!("rt:{session}:1755043200000:in")
        );
    }

    /// The point of the incarnation: a sandbox left over from an earlier
    /// provision of the same session is addressed by a name nothing publishes
    /// to, so it cannot receive — and therefore cannot re-run — a tool call.
    #[test]
    fn a_second_incarnation_of_one_session_is_a_different_topic() {
        let session = Uuid::new_v4().to_string();
        assert_ne!(
            runtime_in(bus(), &session, "1755043200000").name(),
            runtime_in(bus(), &session, "1755043200001").name()
        );
    }

    /// Two sessions never share a topic, whatever else is equal.
    #[test]
    fn two_sessions_never_share_a_topic() {
        let incarnation = "1755043200000";
        assert_ne!(
            runtime_in(bus(), &Uuid::new_v4().to_string(), incarnation).name(),
            runtime_in(bus(), &Uuid::new_v4().to_string(), incarnation).name()
        );
    }
}
