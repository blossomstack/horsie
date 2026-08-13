//! A publish/subscribe bus.

use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, PoisonError};
use tokio::sync::broadcast;

/// How many frames a subscriber may fall behind before it is lagged.
const TOPIC_CAPACITY: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum BusError {
    #[error("bus unavailable: {0}")]
    Unavailable(String),
    /// A value could not be turned into a frame. A bug in the payload type
    /// rather than a fault of the deployment's bus, and kept separate for that
    /// reason: retrying it would fail identically forever.
    #[error("could not encode a frame for '{topic}': {reason}")]
    Encode { topic: String, reason: String },
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

/// One topic, and the single type that travels on it.
///
/// This is what callers hold; [`Bus`] is the transport underneath. Nothing
/// above this line encodes a frame by hand, which is the point — a topic name
/// and its payload type are chosen together, once, and a publisher that sends
/// the wrong shape is a compile error rather than a subscriber that quietly
/// decodes nothing.
///
/// Cheap to clone: a name and a handle to the bus.
///
/// **Where topic names belong.** Construct these in one module per family
/// (`rt:<session>:<incarnation>:in` and its payload, an account's session feed
/// and its payload) rather than at call sites. A name written twice is a name
/// that can be written differently twice, and the failure mode is silence.
pub struct Topic<T> {
    bus: Arc<dyn Bus>,
    name: String,
    /// `fn() -> T` rather than `T`: it makes the marker covariant and leaves
    /// `Topic<T>` unconditionally `Send + Sync`, which a bare `PhantomData<T>`
    /// would tie to `T` for no reason — nothing here ever holds a `T`.
    payload: PhantomData<fn() -> T>,
}

impl<T> Clone for Topic<T> {
    fn clone(&self) -> Self {
        Self {
            bus: self.bus.clone(),
            name: self.name.clone(),
            payload: PhantomData,
        }
    }
}

impl<T: Serialize + DeserializeOwned + Send + 'static> Topic<T> {
    #[must_use]
    pub fn new(bus: Arc<dyn Bus>, name: impl Into<String>) -> Self {
        Self {
            bus,
            name: name.into(),
            payload: PhantomData,
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Send one value to whoever is subscribed, now.
    pub async fn publish(&self, value: &T) -> Result<(), BusError> {
        let frame = serde_json::to_vec(value).map_err(|e| BusError::Encode {
            topic: self.name.clone(),
            reason: e.to_string(),
        })?;
        self.bus.publish(&self.name, frame).await
    }

    /// Start receiving values published from this moment on.
    pub async fn subscribe(&self) -> Result<Reader<T>, BusError> {
        Ok(Reader {
            frames: self.bus.subscribe(&self.name).await?,
            payload: PhantomData,
        })
    }
}

/// One subscriber's end of a [`Topic`].
pub struct Reader<T> {
    frames: Subscription,
    payload: PhantomData<fn() -> T>,
}

impl<T: DeserializeOwned> Reader<T> {
    /// The next value, or `None` once the topic can produce no more.
    ///
    /// A frame that will not decode is **skipped and logged**, never fatal. The
    /// alternative — ending the stream — would let one malformed publisher take
    /// down every reader of that topic, and a reader has no way to recover from
    /// somebody else's bug. Skipping keeps the blast radius at one frame.
    pub async fn recv(&mut self) -> Option<T> {
        loop {
            let frame = self.frames.recv().await?;
            match serde_json::from_slice(&frame) {
                Ok(value) => return Some(value),
                Err(e) => tracing::warn!(
                    error = %e,
                    "a frame on this topic did not decode; skipping it"
                ),
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

    #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    struct Moved {
        session: String,
        revision: u64,
    }

    /// What every caller actually holds. Nobody outside this module should be
    /// writing `serde_json::to_vec` to publish, or naming a topic as a bare
    /// string next to a payload type that is only correct by convention.
    #[tokio::test]
    async fn a_topic_publishes_and_receives_its_own_type() {
        let bus: std::sync::Arc<dyn Bus> = std::sync::Arc::new(MemoryBus::new());
        let topic = Topic::<Moved>::new(bus, "agent:s1:main");

        let mut reader = topic.subscribe().await.unwrap();
        topic
            .publish(&Moved {
                session: "s1".into(),
                revision: 7,
            })
            .await
            .unwrap();

        assert_eq!(
            reader.recv().await,
            Some(Moved {
                session: "s1".into(),
                revision: 7,
            })
        );
    }

    /// A frame that will not decode is skipped, not fatal. One malformed
    /// publisher must not end a reader's stream — it would take down every
    /// session watching that topic, and the reader has no way to recover.
    #[tokio::test]
    async fn a_frame_that_will_not_decode_is_skipped_rather_than_ending_the_stream() {
        let bus: std::sync::Arc<dyn Bus> = std::sync::Arc::new(MemoryBus::new());
        let topic = Topic::<Moved>::new(bus.clone(), "t");
        let mut reader = topic.subscribe().await.unwrap();

        bus.publish("t", b"not json at all".to_vec()).await.unwrap();
        topic
            .publish(&Moved {
                session: "s1".into(),
                revision: 1,
            })
            .await
            .unwrap();

        assert_eq!(
            reader.recv().await,
            Some(Moved {
                session: "s1".into(),
                revision: 1,
            }),
            "the good frame after a bad one must still arrive"
        );
    }
}
