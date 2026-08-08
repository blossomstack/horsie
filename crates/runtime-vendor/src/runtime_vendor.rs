//! What a runtime vendor is, and what a live runtime is.
//!
//! Two traits, deliberately small, and small is the point: a fourth vendor
//! should cost an implementation and no API change. Two rules keep them that
//! way, and both have already earned their keep by deleting members an earlier
//! draft had:
//!
//! - **A capability difference between substrates lives inside an
//!   implementation, never in a trait.** Fly Machines can only be polled; E2B
//!   pushes lifecycle webhooks. Modelling that as an optional event stream here
//!   would bake today's two substrates into the contract and break on the first
//!   one that streams over something else. A vendor that can push consumes its
//!   own webhooks and answers [`RuntimeVendor::poll`] from cache, instantly.
//!   This trait never learns that push exists.
//! - **Adding a default-implemented method later is not a breaking change;
//!   altering a signature is.** So the surface starts at the minimum that
//!   serves the vendors we have.
//!
//! Both traits live here rather than in the server because the same contract
//! describes both sides of the wire: the server drives a `RemoteRuntimeVendor`
//! that relays to a `horsie connect` process, and that process drives a vendor
//! of its own.

use crate::error::RuntimeError;
use async_trait::async_trait;
use horsie_models::runtime::{RuntimeInboundMessage, RuntimeOutboundMessage};
use horsie_models::runtime_vendor::{RuntimeSpec, RuntimeVendorCapabilities};
use horsie_runtime_client::TransportError;
use std::sync::Arc;

/// Where a runtime is, as its vendor currently understands it.
///
/// A vendor reports progress rather than handing back a runtime because no
/// substrate can report readiness in one round trip. Four phases sit between
/// "please build this" and "you can send it a tool call" — the substrate
/// accepts the request, the substrate's object starts, `horsie-runtime` boots
/// and dials back, and it finishes its provision steps — and only the last two
/// are ours to observe. A `create` that returned a handle would have to hide
/// all four behind one await, which is what makes a slow provision
/// indistinguishable from a hang.
#[derive(Debug, Clone)]
pub enum RuntimeProgress {
    /// The substrate is still working. `detail` is written for the person
    /// watching a session start, not for a log.
    Pending { detail: String },
    /// Reachable now.
    Ready(Arc<dyn RuntimeHandle>),
}

impl RuntimeProgress {
    /// The handle, if this runtime is reachable.
    #[must_use]
    pub fn ready(&self) -> Option<&Arc<dyn RuntimeHandle>> {
        match self {
            Self::Ready(handle) => Some(handle),
            Self::Pending { .. } => None,
        }
    }
}

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
#[async_trait]
pub trait RuntimeVendor: Send + Sync {
    /// The name sessions select this vendor by.
    fn name(&self) -> &str;

    /// What this vendor can do with a session's workspace. Announced rather
    /// than inferred, so nothing above has to branch on a vendor's kind.
    fn capabilities(&self) -> RuntimeVendorCapabilities;

    /// Ask the substrate to build a runtime.
    ///
    /// Returns once the *request* is accepted, not once the runtime is usable.
    /// Called exactly once per session; every later acquisition is a
    /// [`Self::poll`].
    async fn create(
        &self,
        runtime_id: &str,
        spec: &RuntimeSpec,
    ) -> Result<RuntimeProgress, RuntimeVendorError>;

    /// Where are these runtimes now? Idempotent and cheap. Resuming one the
    /// vendor hibernated happens here, and so does noticing one that died.
    ///
    /// Takes a slice rather than an id because poll granularity is per vendor
    /// and never per runtime: Fly rate-limits per-machine polling and answers
    /// for a whole app in a single list call, so a one-id-at-a-time signature
    /// would make the only affordable implementation impossible to write.
    ///
    /// An id this vendor knows nothing about is simply absent from the result.
    /// That is not an error — the caller asked a question, and "no such
    /// runtime" is an answer.
    async fn poll(
        &self,
        runtime_ids: &[&str],
    ) -> Result<Vec<(String, RuntimeProgress)>, RuntimeVendorError>;

    /// Advisory suspend. A vendor that cannot suspend keeps the runtime
    /// running, which is a correct implementation and far better than
    /// destroying a workspace to save a little compute.
    async fn hibernate(&self, runtime_id: &str) -> Result<(), RuntimeVendorError>;

    /// The owning session was deleted; the vendor decides the runtime's fate.
    async fn delete(&self, runtime_id: &str) -> Result<(), RuntimeVendorError>;
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
    /// substrate that answered a poll with a dead machine. Whoever holds the
    /// handle drops it, so a dead runtime is never handed to a turn.
    async fn closed(&self);
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

    fn spec() -> RuntimeSpec {
        RuntimeSpec {
            workspaces: vec![],
            env: vec![],
            provision: vec![],
        }
    }

    /// A vendor with no socket behind it. The point of the trait: a map of
    /// `dyn RuntimeVendor` holds substrate-backed and process-backed vendors
    /// identically, and nothing above it branches on which one it has.
    struct CountingVendor {
        polls: std::sync::atomic::AtomicUsize,
    }

    impl CountingVendor {
        fn new() -> Self {
            Self {
                polls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn polls(&self) -> usize {
            self.polls.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    #[async_trait]
    impl RuntimeVendor for CountingVendor {
        fn name(&self) -> &str {
            "counting"
        }
        fn capabilities(&self) -> RuntimeVendorCapabilities {
            RuntimeVendorCapabilities {
                supports_provisioning: true,
            }
        }
        async fn create(
            &self,
            runtime_id: &str,
            _: &RuntimeSpec,
        ) -> Result<RuntimeProgress, RuntimeVendorError> {
            Ok(RuntimeProgress::Pending {
                detail: format!("creating {runtime_id}"),
            })
        }
        async fn poll(
            &self,
            runtime_ids: &[&str],
        ) -> Result<Vec<(String, RuntimeProgress)>, RuntimeVendorError> {
            self.polls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(runtime_ids
                .iter()
                .map(|id| {
                    (
                        (*id).to_string(),
                        RuntimeProgress::Pending {
                            detail: "starting".to_string(),
                        },
                    )
                })
                .collect())
        }
        async fn hibernate(&self, _: &str) -> Result<(), RuntimeVendorError> {
            Ok(())
        }
        async fn delete(&self, _: &str) -> Result<(), RuntimeVendorError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn create_reports_progress_rather_than_handing_back_a_runtime() {
        // No substrate reports readiness in one round trip, so the contract
        // must not promise one. This is the whole reason for RuntimeProgress.
        let vendor = CountingVendor::new();
        let progress = vendor.create("s1", &spec()).await.unwrap();
        assert!(progress.ready().is_none());
        assert!(matches!(progress, RuntimeProgress::Pending { .. }));
    }

    #[tokio::test]
    async fn one_poll_answers_for_every_runtime_on_the_vendor() {
        // Fly rate-limits per-machine polling and answers for a whole app in a
        // single list call. A slice-taking signature is what lets a vendor be
        // implemented that way at all.
        let vendor = CountingVendor::new();
        let answered = vendor.poll(&["s1", "s2", "s3"]).await.unwrap();
        assert_eq!(answered.len(), 3);
        assert_eq!(vendor.polls(), 1);
    }

    #[tokio::test]
    async fn a_vendor_is_usable_behind_a_trait_object() {
        let vendor: Arc<dyn RuntimeVendor> = Arc::new(CountingVendor::new());
        assert_eq!(vendor.name(), "counting");
        assert!(vendor.capabilities().supports_provisioning);
        vendor.hibernate("s1").await.unwrap();
        vendor.delete("s1").await.unwrap();
    }
}
