//! A publish/subscribe bus.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use tokio::sync::broadcast;

/// How many frames a subscriber may fall behind before it is lagged.
const TOPIC_CAPACITY: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum BusError {
    #[error("bus unavailable: {0}")]
    Unavailable(String),
}

/// One subscriber's end of a topic.
pub struct Subscription {
    rx: broadcast::Receiver<Vec<u8>>,
}

impl Subscription {
    /// The next frame, or `None` once the topic can produce no more.
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        self.rx.recv().await.ok()
    }
}

/// Where a frame goes, and where one comes from.
///
/// Two operations and nothing else. Everything a deployment differs in — how a
/// frame reaches another node, whether it is durable, what it costs — lives
/// inside an implementation, because a capability in the trait is a capability
/// every implementation has to answer for.
///
/// **A topic is a name, and a name contains no node.** That is the whole point:
/// an actor re-placed onto another host keeps publishing to the same topic, so
/// nothing a subscriber holds is invalidated by a move. Nothing here takes or
/// returns a node id, and nothing should.
///
/// Delivery is best-effort by design. A frame published with no subscriber is
/// gone, and a subscriber that falls behind loses the frames it missed —
/// callers publish values a reader can re-derive (a counter to compare, a
/// message it can ask for again), never state that exists only in transit.
#[async_trait::async_trait]
pub trait Bus: Send + Sync {
    /// Send one frame to whoever is subscribed to `topic`, now.
    async fn publish(&self, topic: &str, payload: Vec<u8>) -> Result<(), BusError>;

    /// Start receiving frames published to `topic` from this moment on.
    ///
    /// Async because a distributed implementation has to reach its backing
    /// service before it can promise anything arrives.
    async fn subscribe(&self, topic: &str) -> Result<Subscription, BusError>;
}

/// A bus confined to this process.
///
/// What a single-node deployment runs, and what the tests run. It is not a stub
/// for the clustered case: the two are the same code path, which is what keeps
/// the distributed one exercised by the ordinary suite rather than only by a
/// cluster test that cannot be written yet.
#[derive(Default)]
pub struct MemoryBus {
    topics: Mutex<HashMap<String, broadcast::Sender<Vec<u8>>>>,
}

impl MemoryBus {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// This topic's channel, created on first use by either side.
    ///
    /// Created by `subscribe` *and* by `publish`, so the two need no ordering
    /// between them: a publisher that arrives first leaves a channel a later
    /// subscriber joins, rather than dropping the topic on the floor.
    fn sender(&self, topic: &str) -> broadcast::Sender<Vec<u8>> {
        self.topics
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(topic.to_string())
            .or_insert_with(|| broadcast::Sender::new(TOPIC_CAPACITY))
            .clone()
    }
}

#[async_trait::async_trait]
impl Bus for MemoryBus {
    async fn publish(&self, topic: &str, payload: Vec<u8>) -> Result<(), BusError> {
        // A send with no receiver is an `Err` from `broadcast` and *not* an
        // error here: an offloaded session has unsubscribed while its sandbox
        // is still publishing, which is ordinary rather than a fault.
        let _ = self.sender(topic).send(payload);
        Ok(())
    }

    async fn subscribe(&self, topic: &str) -> Result<Subscription, BusError> {
        Ok(Subscription {
            rx: self.sender(topic).subscribe(),
        })
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
    use super::*;

    #[tokio::test]
    async fn a_subscriber_receives_what_is_published_to_its_topic() {
        let bus = MemoryBus::new();
        let mut sub = bus.subscribe("rt:s1:in").await.expect("subscribe");
        bus.publish("rt:s1:in", b"hello".to_vec())
            .await
            .expect("publish");
        assert_eq!(sub.recv().await, Some(b"hello".to_vec()));
    }

    /// The isolation the whole design rests on. Two accounts' streams are two
    /// topics, not one topic and a filter — a filter is one forgotten line from
    /// leaking every session title on the server.
    #[tokio::test]
    async fn a_subscriber_hears_nothing_from_another_topic() {
        let bus = MemoryBus::new();
        let mut mine = bus.subscribe("acct:a:sessions").await.unwrap();
        bus.publish("acct:b:sessions", b"not yours".to_vec())
            .await
            .unwrap();
        bus.publish("acct:a:sessions", b"yours".to_vec())
            .await
            .unwrap();
        assert_eq!(
            mine.recv().await,
            Some(b"yours".to_vec()),
            "the other account's frame must not arrive first, or at all"
        );
    }

    /// Fan-out: the property that makes this a bus rather than a queue. Two
    /// readers of one session's stream both have to see it move.
    #[tokio::test]
    async fn every_subscriber_of_a_topic_receives_the_frame() {
        let bus = MemoryBus::new();
        let mut first = bus.subscribe("t").await.unwrap();
        let mut second = bus.subscribe("t").await.unwrap();
        bus.publish("t", b"x".to_vec()).await.unwrap();
        assert_eq!(first.recv().await, Some(b"x".to_vec()));
        assert_eq!(second.recv().await, Some(b"x".to_vec()));
    }

    /// Publishing into an empty topic is ordinary, not an error. A session
    /// unsubscribes when it offloads, and the sandbox may still be publishing.
    #[tokio::test]
    async fn publishing_with_nobody_listening_is_not_an_error() {
        let bus = MemoryBus::new();
        bus.publish("nobody", b"x".to_vec())
            .await
            .expect("a topic with no subscriber still accepts a publish");
    }

    /// A deployment picks its bus; nothing above it may know which. The trait
    /// has to stay object-safe for that, which is what this asserts — a caller
    /// holding `Arc<dyn Bus>` publishes and subscribes without naming an
    /// implementation.
    #[tokio::test]
    async fn a_deployment_holds_its_bus_without_naming_the_implementation() {
        let bus: std::sync::Arc<dyn Bus> = std::sync::Arc::new(MemoryBus::new());
        let mut sub = bus.subscribe("t").await.unwrap();
        bus.publish("t", b"x".to_vec()).await.unwrap();
        assert_eq!(sub.recv().await, Some(b"x".to_vec()));
    }
}
