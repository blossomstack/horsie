//! What a runtime vendor is, and what a live runtime is.
//!
//! Two traits, deliberately small, and small is the point: a fourth vendor
//! should cost an implementation and no API change. Two rules keep them that
//! way, and both have already earned their keep by deleting members earlier
//! drafts had:
//!
//! - **A capability difference between substrates lives inside an
//!   implementation, never in a trait.** Fly Machines can only be polled; E2B
//!   pushes lifecycle webhooks. An earlier draft had a `poll` method, which
//!   forced *polling* into the contract — the very mistake this rule exists to
//!   prevent. A vendor now reports progress however it likes: Fly polls
//!   `/wait` internally, E2B consumes its own webhooks, and neither leaks here.
//! - **Adding a default-implemented method later is not a breaking change;
//!   altering a signature is.** So the surface starts at the minimum that
//!   serves the vendors we have.
//!
//! Both traits live here rather than in the server because the same contract
//! describes both sides of the wire: the server drives a `WebsocketRuntimeVendor`
//! that relays to a `horsie connect` process, and that process drives a vendor
//! of its own.

use crate::error::RuntimeError;
use async_trait::async_trait;
use horsie_models::runtime::{RuntimeInboundMessage, RuntimeOutboundMessage};
use horsie_models::runtime_vendor::{RuntimeSpec, RuntimeVendorCapabilities};
use horsie_runtime_client::TransportError;
use std::sync::Arc;

/// Where a runtime is, as its vendor currently understands it.
#[derive(Debug, Clone)]
pub enum RuntimeProgress {
    /// The substrate accepted the request and nothing more is known yet.
    Requested,
    /// The substrate's object is coming up.
    Starting { detail: String },
    /// The runtime is up and running its provision steps.
    Provisioning { detail: String },
    /// Reachable now.
    Ready(Arc<dyn RuntimeHandle>),
    /// On its way down.
    Stopping,
    /// Down, and revivable.
    Stopped,
    /// Down, and not coming back.
    Gone { reason: String },
}

/// One progress report, stamped with the runtime it concerns.
///
/// Carrying the id means an account needs **one** sink rather than a channel
/// per call, and costs the vendor nothing — every method already receives the
/// id it would stamp.
#[derive(Debug, Clone)]
pub struct RuntimeEvent {
    pub runtime_id: String,
    pub progress: RuntimeProgress,
}

/// Where a vendor reports progress.
///
/// A plain channel rather than another trait, and `try_send` rather than
/// `send`: dropping a report because the consumer is behind is correct
/// behaviour, since progress is advisory and the operation's return value is
/// the outcome. A vendor that blocked here would let a slow UI stall a
/// provision.
pub type RuntimeProgressSink = tokio::sync::mpsc::Sender<RuntimeEvent>;

/// Why a vendor could not do what was asked.
///
/// The distinction between [`Self::Gone`] and [`Self::Unavailable`] is the one
/// that matters and the one worth getting right: a session whose runtime is
/// *gone* can never run again and must say so, while a session whose vendor is
/// merely *unreachable* is fine and should be retried. Confusing them either
/// strands a working session or silently rebuilds a workspace the user believes
/// still holds their work.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeVendorError {
    /// Provisioning failed. The session can try again.
    #[error("provisioning failed: {0}")]
    Provision(String),
    /// This vendor has no runtime under that id and cannot produce one.
    /// Terminal for the owning session.
    #[error("runtime is gone: {0}")]
    Gone(String),
    /// The vendor itself could not be reached. Always retryable.
    #[error("runtime vendor unavailable: {0}")]
    Unavailable(String),
}

impl From<RuntimeError> for RuntimeVendorError {
    fn from(e: RuntimeError) -> Self {
        Self::Provision(e.to_string())
    }
}

/// A named source of runtimes. One implementation per substrate.
///
/// Every operation follows one shape: it returns the **first observation**, and
/// anything later arrives on the sink. A vendor that already knows the answer —
/// a `horsie connect` process only answers once its runtime is up — returns
/// `Ready` and never touches the sink at all. A vendor whose substrate needs
/// minutes returns `Starting` and finishes in the background. Neither is forced
/// to hold a long await, so an interrupted operation leaves no orphaned future.
///
/// **The ordering rule that makes this safe: an implementation must not emit on
/// the sink for an operation before that operation has returned.** Build the
/// return value, *then* start the background work. Without it a caller could
/// observe `Ready` before the `Starting` it was returned, and would need
/// reconciliation logic; with it, the return value is simply the first event,
/// and one reducer handles both.
#[async_trait]
pub trait RuntimeVendor: Send + Sync {
    /// The name sessions select this vendor by.
    fn name(&self) -> &str;

    /// What this vendor can do with a session's workspace. Announced rather
    /// than inferred, so nothing above has to branch on a vendor's kind.
    fn capabilities(&self) -> RuntimeVendorCapabilities;

    /// Whether this vendor can be used right now.
    ///
    /// Default `true`, because for most vendors it always is: a Fly vendor is
    /// a REST client, and a bad token surfaces as a failed operation rather
    /// than a state. A vendor reached over a socket overrides it, so a caller
    /// can wait out a reconnect instead of failing a turn.
    ///
    /// Defaulted rather than required, which is the additive-method pattern
    /// this contract is built around: implementations that do not care are
    /// unaffected.
    fn is_reachable(&self) -> bool {
        true
    }

    /// Build a runtime. Called exactly once per session; every later
    /// acquisition is [`Self::get`].
    async fn create(
        &self,
        runtime_id: &str,
        spec: &RuntimeSpec,
        progress: RuntimeProgressSink,
    ) -> Result<RuntimeProgress, RuntimeVendorError>;

    /// Acquire a runtime that already exists, reviving it if this vendor
    /// hibernated it.
    ///
    /// Never provisions from nothing: an acquisition that silently built a
    /// fresh workspace would destroy work the user believes is still there.
    /// Carries the spec because the server is its only durable holder.
    async fn get(
        &self,
        runtime_id: &str,
        spec: &RuntimeSpec,
        progress: RuntimeProgressSink,
    ) -> Result<RuntimeProgress, RuntimeVendorError>;

    /// Advisory suspend. A vendor that cannot suspend keeps the runtime
    /// running, which is a correct implementation and far better than
    /// destroying a workspace to save a little compute.
    async fn hibernate(
        &self,
        runtime_id: &str,
        progress: RuntimeProgressSink,
    ) -> Result<RuntimeProgress, RuntimeVendorError>;

    /// The owning session was deleted; the vendor decides the runtime's fate.
    async fn delete(
        &self,
        runtime_id: &str,
        progress: RuntimeProgressSink,
    ) -> Result<RuntimeProgress, RuntimeVendorError>;
}

/// A live runtime.
///
/// Every member is the runtime protocol, which is why a handle looks the same
/// whatever substrate is underneath — a Fly machine, a velos container and a
/// process on someone's laptop are all just something you can send a
/// [`RuntimeInboundMessage`] to.
///
/// There is deliberately no `stop`: stopping is a vendor operation keyed by id
/// ([`RuntimeVendor::hibernate`], [`RuntimeVendor::delete`]), and a `stop` here
/// would have been ambiguous against both.
#[async_trait]
pub trait RuntimeHandle: Send + Sync + std::fmt::Debug {
    /// The runtime this handle talks to. Equal to the owning session's id.
    fn id(&self) -> &str;

    /// Send a message and wait for its reply.
    async fn relay(
        &self,
        message: RuntimeInboundMessage,
    ) -> Result<RuntimeOutboundMessage, TransportError>;

    /// Send a message that draws no reply (a relayed `CancelCall`).
    async fn relay_oneway(&self, message: RuntimeInboundMessage) -> Result<(), TransportError>;

    /// Resolves once this runtime can no longer be reached.
    ///
    /// The unifying signal for a runtime going away, whatever noticed first:
    /// a vendor link reporting a state change, a WebSocket closing, or a
    /// substrate that reported a dead machine. Whoever holds the handle drops
    /// it, so a dead runtime is never handed to a turn.
    async fn closed(&self);
}

/// The only [`RuntimeHandle`] implementation there needs to be.
///
/// Every difference between vendors lives in the transport, which is a seam
/// that already existed: a websocket vendor supplies one that relays through
/// its vendor link, and an in-server vendor supplies the socket its runtime
/// registered when it dialled back. Two handle types would have re-derived a
/// split [`RuntimeTransport`] already makes.
pub struct RuntimeHandleImpl {
    runtime_id: String,
    transport: Arc<dyn horsie_runtime_client::RuntimeTransport>,
    /// Flipped by whoever owns the connection when it can no longer be reached.
    closed: tokio::sync::watch::Receiver<bool>,
}

impl RuntimeHandleImpl {
    #[must_use]
    pub fn new(
        runtime_id: String,
        transport: Arc<dyn horsie_runtime_client::RuntimeTransport>,
        closed: tokio::sync::watch::Receiver<bool>,
    ) -> Self {
        Self {
            runtime_id,
            transport,
            closed,
        }
    }
}

impl std::fmt::Debug for RuntimeHandleImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeHandleImpl")
            .field("runtime_id", &self.runtime_id)
            .field("closed", &*self.closed.borrow())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl RuntimeHandle for RuntimeHandleImpl {
    fn id(&self) -> &str {
        &self.runtime_id
    }

    async fn relay(
        &self,
        message: RuntimeInboundMessage,
    ) -> Result<RuntimeOutboundMessage, TransportError> {
        self.transport.relay(message).await
    }

    async fn relay_oneway(&self, message: RuntimeInboundMessage) -> Result<(), TransportError> {
        self.transport.send_oneway(message).await
    }

    async fn closed(&self) {
        let mut rx = self.closed.clone();
        // Checked before waiting, not after: a handle whose runtime died before
        // anyone asked must still answer immediately, and `wait_for` alone
        // would miss a flip that happened before this clone.
        if *rx.borrow() {
            return;
        }
        let _ = rx.wait_for(|closed| *closed).await;
    }
}

/// Lets a handle be used wherever a [`RuntimeTransport`] is expected.
///
/// `RuntimeClient` is built over a transport and a handle is built over one, so
/// this is the one-line adapter between them rather than a second abstraction:
/// it keeps `RuntimeHandle` free to be about a *runtime* while `RuntimeTransport`
/// stays about *bytes*.
///
/// [`RuntimeTransport`]: horsie_runtime_client::RuntimeTransport
pub struct RuntimeHandleTransport(pub Arc<dyn RuntimeHandle>);

#[async_trait]
impl horsie_runtime_client::RuntimeTransport for RuntimeHandleTransport {
    async fn relay(
        &self,
        message: RuntimeInboundMessage,
    ) -> Result<RuntimeOutboundMessage, TransportError> {
        self.0.relay(message).await
    }

    async fn send_oneway(&self, message: RuntimeInboundMessage) -> Result<(), TransportError> {
        self.0.relay_oneway(message).await
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
    use std::time::Duration;

    fn spec() -> RuntimeSpec {
        RuntimeSpec {
            workspaces: vec![],
            env: vec![],
            provision: vec![],
        }
    }

    #[derive(Debug)]
    struct StubHandle(String);

    #[async_trait]
    impl RuntimeHandle for StubHandle {
        fn id(&self) -> &str {
            &self.0
        }
        async fn relay(
            &self,
            _: RuntimeInboundMessage,
        ) -> Result<RuntimeOutboundMessage, TransportError> {
            Err(TransportError::SendFailed("stub".to_string()))
        }
        async fn relay_oneway(&self, _: RuntimeInboundMessage) -> Result<(), TransportError> {
            Ok(())
        }
        async fn closed(&self) {
            std::future::pending::<()>().await;
        }
    }

    fn caps() -> RuntimeVendorCapabilities {
        RuntimeVendorCapabilities {
            supports_provisioning: true,
        }
    }

    /// Answers everything immediately, the way a `horsie connect` process does:
    /// it only replies once its runtime is already up.
    struct ImmediateVendor;

    #[async_trait]
    impl RuntimeVendor for ImmediateVendor {
        fn name(&self) -> &str {
            "immediate"
        }
        fn capabilities(&self) -> RuntimeVendorCapabilities {
            caps()
        }
        async fn create(
            &self,
            runtime_id: &str,
            _: &RuntimeSpec,
            _: RuntimeProgressSink,
        ) -> Result<RuntimeProgress, RuntimeVendorError> {
            Ok(RuntimeProgress::Ready(Arc::new(StubHandle(
                runtime_id.to_string(),
            ))))
        }
        async fn get(
            &self,
            runtime_id: &str,
            _: &RuntimeSpec,
            _: RuntimeProgressSink,
        ) -> Result<RuntimeProgress, RuntimeVendorError> {
            Ok(RuntimeProgress::Ready(Arc::new(StubHandle(
                runtime_id.to_string(),
            ))))
        }
        async fn hibernate(
            &self,
            _: &str,
            _: RuntimeProgressSink,
        ) -> Result<RuntimeProgress, RuntimeVendorError> {
            Ok(RuntimeProgress::Stopped)
        }
        async fn delete(
            &self,
            _: &str,
            _: RuntimeProgressSink,
        ) -> Result<RuntimeProgress, RuntimeVendorError> {
            Ok(RuntimeProgress::Gone {
                reason: "deleted".to_string(),
            })
        }
    }

    /// Returns `Starting` and finishes on the sink, the way a substrate that
    /// boots a machine does. Honours the ordering rule by spawning only after
    /// the return value has been built.
    struct SlowVendor;

    #[async_trait]
    impl RuntimeVendor for SlowVendor {
        fn name(&self) -> &str {
            "slow"
        }
        fn capabilities(&self) -> RuntimeVendorCapabilities {
            caps()
        }
        async fn create(
            &self,
            runtime_id: &str,
            _: &RuntimeSpec,
            progress: RuntimeProgressSink,
        ) -> Result<RuntimeProgress, RuntimeVendorError> {
            let first = RuntimeProgress::Starting {
                detail: "booting".to_string(),
            };
            let id = runtime_id.to_string();
            tokio::spawn(async move {
                for progress_step in [
                    RuntimeProgress::Provisioning {
                        detail: "cloning".to_string(),
                    },
                    RuntimeProgress::Ready(Arc::new(StubHandle(id.clone()))),
                ] {
                    let _ = progress
                        .send(RuntimeEvent {
                            runtime_id: id.clone(),
                            progress: progress_step,
                        })
                        .await;
                }
            });
            Ok(first)
        }
        async fn get(
            &self,
            runtime_id: &str,
            spec: &RuntimeSpec,
            progress: RuntimeProgressSink,
        ) -> Result<RuntimeProgress, RuntimeVendorError> {
            self.create(runtime_id, spec, progress).await
        }
        async fn hibernate(
            &self,
            _: &str,
            _: RuntimeProgressSink,
        ) -> Result<RuntimeProgress, RuntimeVendorError> {
            Ok(RuntimeProgress::Stopping)
        }
        async fn delete(
            &self,
            _: &str,
            _: RuntimeProgressSink,
        ) -> Result<RuntimeProgress, RuntimeVendorError> {
            Ok(RuntimeProgress::Stopping)
        }
    }

    fn sink(
        capacity: usize,
    ) -> (
        RuntimeProgressSink,
        tokio::sync::mpsc::Receiver<RuntimeEvent>,
    ) {
        tokio::sync::mpsc::channel(capacity)
    }

    #[tokio::test]
    async fn a_vendor_that_knows_the_answer_returns_ready_without_touching_the_sink() {
        // The reason create returns progress rather than a handle: a vendor
        // with nothing to wait for should not be forced through a sink.
        let (tx, mut rx) = sink(8);
        let progress = ImmediateVendor.create("s1", &spec(), tx).await.unwrap();
        match progress {
            RuntimeProgress::Ready(handle) => assert_eq!(handle.id(), "s1"),
            other => panic!("expected Ready, got {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "nothing should have been emitted");
    }

    #[tokio::test]
    async fn a_slow_vendor_returns_its_first_observation_then_finishes_on_the_sink() {
        let (tx, mut rx) = sink(8);
        let first = SlowVendor.create("s1", &spec(), tx).await.unwrap();
        assert!(matches!(first, RuntimeProgress::Starting { .. }));

        let mut seen = Vec::new();
        while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await {
            let ready = matches!(event.progress, RuntimeProgress::Ready(_));
            assert_eq!(event.runtime_id, "s1");
            seen.push(event.progress);
            if ready {
                break;
            }
        }
        assert!(matches!(
            seen.first(),
            Some(RuntimeProgress::Provisioning { .. })
        ));
        assert!(matches!(seen.last(), Some(RuntimeProgress::Ready(_))));
    }

    #[tokio::test]
    async fn one_sink_serves_every_runtime_because_events_carry_the_id() {
        let (tx, mut rx) = sink(16);
        SlowVendor.create("s1", &spec(), tx.clone()).await.unwrap();
        SlowVendor.create("s2", &spec(), tx).await.unwrap();

        let mut ids = std::collections::HashSet::new();
        for _ in 0..4 {
            if let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await {
                ids.insert(event.runtime_id);
            }
        }
        assert_eq!(ids.len(), 2, "both runtimes reported on the one channel");
    }

    #[derive(Default)]
    struct RecordingTransport {
        relays: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl horsie_runtime_client::RuntimeTransport for RecordingTransport {
        async fn relay(
            &self,
            _: RuntimeInboundMessage,
        ) -> Result<RuntimeOutboundMessage, TransportError> {
            self.relays
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Err(TransportError::SendFailed("recorded".to_string()))
        }
        async fn send_oneway(&self, _: RuntimeInboundMessage) -> Result<(), TransportError> {
            Ok(())
        }
    }

    fn ping() -> RuntimeInboundMessage {
        RuntimeInboundMessage::CancelCall(horsie_models::runtime::CancelCallRequest {
            call_id: "c1".to_string(),
        })
    }

    #[tokio::test]
    async fn a_handle_delegates_every_message_to_its_transport() {
        let transport = Arc::new(RecordingTransport::default());
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let handle = RuntimeHandleImpl::new("s1".to_string(), transport.clone(), rx);

        assert_eq!(handle.id(), "s1");
        let _ = handle.relay(ping()).await;
        handle.relay_oneway(ping()).await.unwrap();
        assert_eq!(
            transport.relays.load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    #[tokio::test]
    async fn closed_resolves_when_the_connection_goes_away() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let handle = RuntimeHandleImpl::new(
            "s1".to_string(),
            Arc::new(RecordingTransport::default()),
            rx,
        );

        let waiting = tokio::spawn(async move { handle.closed().await });
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("closed() should resolve once the connection is marked dead")
            .unwrap();
    }

    #[tokio::test]
    async fn closed_resolves_immediately_for_a_runtime_that_already_died() {
        // A handle asked after the fact must still answer: checking the value
        // only after subscribing would hang forever on a flip that already
        // happened.
        let (tx, rx) = tokio::sync::watch::channel(false);
        tx.send(true).unwrap();
        let handle = RuntimeHandleImpl::new(
            "s1".to_string(),
            Arc::new(RecordingTransport::default()),
            rx,
        );

        tokio::time::timeout(Duration::from_millis(200), handle.closed())
            .await
            .expect("closed() must not wait for a flip that already happened");
    }

    #[tokio::test]
    async fn a_vendor_is_usable_behind_a_trait_object() {
        let vendor: Arc<dyn RuntimeVendor> = Arc::new(ImmediateVendor);
        let (tx, _rx) = sink(4);
        assert_eq!(vendor.name(), "immediate");
        assert!(vendor.capabilities().supports_provisioning);
        assert!(matches!(
            vendor.hibernate("s1", tx.clone()).await.unwrap(),
            RuntimeProgress::Stopped
        ));
        assert!(matches!(
            vendor.delete("s1", tx).await.unwrap(),
            RuntimeProgress::Gone { .. }
        ));
    }
}
