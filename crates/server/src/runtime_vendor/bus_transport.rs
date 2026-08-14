//! A [`RuntimeTransport`] that reaches a sandbox by topic rather than by socket.
//!
//! The same correlation model as `SocketRuntimeTransport` — one
//! `call_id → oneshot` map, a reader task that routes replies into it, and an
//! unmatched id dropped rather than treated as a protocol error — with the
//! socket replaced by a pair of topics. That is the whole difference, and it is
//! what makes the transport identical on every node: a topic is a name, and a
//! name contains no node, so nothing here has to know where the sandbox's
//! connection landed.
//!
//! **One thing the socket gave for free and this does not: a closure signal.**
//! `SocketRuntimeTransport`'s reader drains its pending map with `Disconnected`
//! when the link drops. Nothing publishes anything when a pump's host dies, so a
//! request whose answer is never coming waits forever here. That is not an
//! oversight — it is precisely the gap reconciliation fills, and until it lands
//! a caller must not treat silence as success.
//!
//! Two ordering rules the bus forces, both from its own contract that *a frame
//! published with no subscriber is gone*:
//!
//! 1. subscribe to the out topic **before** publishing anything on the in topic,
//!    or a fast reply races the subscription and is lost;
//! 2. the reader task must be running before `relay` returns, for the same
//!    reason.
//!
//! Both are satisfied by construction: [`BusTransport::open`] subscribes and
//! spawns before it hands back a transport at all.

use crate::bus::{Bus, Topic, topics};
use async_trait::async_trait;
use horsie_models::runtime::{RuntimeInboundMessage, RuntimeOutboundMessage};
use horsie_runtime_host::{RuntimeTransport, TransportError, outbound_call_id};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, oneshot};

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<RuntimeOutboundMessage>>>>;

pub struct BusTransport {
    inbound: Topic<RuntimeInboundMessage>,
    pending: Pending,
    /// Ends the reader task when the last holder of this transport goes away.
    _reader: DropGuard,
}

/// Aborts the reader when the transport is dropped, so an acquisition that is
/// abandoned does not leave a subscription draining forever.
struct DropGuard(tokio::task::JoinHandle<()>);

impl Drop for DropGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl BusTransport {
    /// Subscribe to the runtime's replies and start routing them, then hand back
    /// a transport that can be used.
    ///
    /// Subscribing first is load-bearing: a runtime that answers instantly would
    /// otherwise publish into a topic nobody is reading yet, and the reply would
    /// simply not exist.
    pub async fn open(
        bus: Arc<dyn Bus>,
        account: &str,
        runtime: &str,
        incarnation: &str,
    ) -> Result<Self, TransportError> {
        let inbound = topics::runtime_in(bus.clone(), account, runtime, incarnation);
        let mut replies = topics::runtime_out(bus, account, runtime, incarnation)
            .subscribe()
            .await
            .map_err(|e| TransportError::SendFailed(e.to_string()))?;

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let routed = pending.clone();
        let reader = tokio::spawn(async move {
            while let Some(message) = replies.recv().await {
                // A reply with no correlation id is a handshake message; one
                // whose id nobody holds is a reply to a request that no longer
                // exists — a restart on either side leaves exactly that behind.
                // Both are dropped, deliberately: there is nowhere to deliver
                // them and inventing a destination would resurrect work the
                // session has already written off as interrupted.
                let Some(call_id) = outbound_call_id(&message) else {
                    continue;
                };
                let waiter = routed.lock().await.remove(call_id);
                if let Some(tx) = waiter {
                    let _ = tx.send(message);
                }
            }
        });

        Ok(Self {
            inbound,
            pending,
            _reader: DropGuard(reader),
        })
    }

    /// How many replies this transport is still waiting for. Test observability
    /// for the drop rule above.
    #[cfg(test)]
    pub(crate) async fn pending_count(&self) -> usize {
        self.pending.lock().await.len()
    }
}

#[async_trait]
impl RuntimeTransport for BusTransport {
    async fn relay(
        &self,
        message: RuntimeInboundMessage,
    ) -> Result<RuntimeOutboundMessage, TransportError> {
        let call_id = horsie_runtime_host::inbound_call_id(&message).to_string();
        let (tx, rx) = oneshot::channel();
        // Registered before the publish, never after: a runtime fast enough to
        // answer between the two would find no waiter and its reply would be
        // dropped by the reader above.
        self.pending.lock().await.insert(call_id.clone(), tx);

        if let Err(e) = self.inbound.publish(&message).await {
            self.pending.lock().await.remove(&call_id);
            return Err(TransportError::SendFailed(e.to_string()));
        }

        // No deadline here, on purpose. A tool has no natural bound — a file
        // read and a twenty-minute build ride this same call — so the bound
        // belongs on a liveness check that does, and lives with the caller's
        // reconciliation rather than in the pipe.
        rx.await.map_err(|_| TransportError::Disconnected)
    }

    async fn send_oneway(&self, message: RuntimeInboundMessage) -> Result<(), TransportError> {
        self.inbound
            .publish(&message)
            .await
            .map_err(|e| TransportError::SendFailed(e.to_string()))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::bus::MemoryBus;
    use horsie_models::runtime::{BashInput, ToolCall, ToolCallResponse, ToolOutput, ToolResult};
    use uuid::Uuid;

    const ACCOUNT: &str = "acct-1";

    fn bash() -> ToolCall {
        ToolCall::Bash(BashInput {
            command: "true".to_string(),
            timeout_secs: None,
        })
    }

    fn answered(call_id: &str, stdout: &str) -> RuntimeOutboundMessage {
        RuntimeOutboundMessage::ToolCallResponse(ToolCallResponse {
            call_id: call_id.to_string(),
            result: ToolResult::Ok(ToolOutput {
                stdout: stdout.to_string(),
                stderr: String::new(),
                exit_code: 0,
            }),
            hooks: vec![],
        })
    }

    /// Stand in for the pump: echo a response for whatever arrives on the in
    /// topic, so a relay completes end to end over the two names.
    async fn echo(bus: Arc<dyn Bus>, session: &str, incarnation: &str) {
        let mut inbound = topics::runtime_in(bus.clone(), ACCOUNT, session, incarnation)
            .subscribe()
            .await
            .unwrap();
        let out = topics::runtime_out(bus, ACCOUNT, session, incarnation);
        tokio::spawn(async move {
            while let Some(message) = inbound.recv().await {
                let call_id = horsie_runtime_host::inbound_call_id(&message).to_string();
                let _ = out.publish(&answered(&call_id, "hi")).await;
            }
        });
    }

    #[tokio::test]
    async fn a_call_is_answered_across_the_two_topics() {
        let bus: Arc<dyn Bus> = Arc::new(MemoryBus::new());
        let session = Uuid::new_v4().to_string();
        let transport = BusTransport::open(bus.clone(), ACCOUNT, &session, "1")
            .await
            .unwrap();
        echo(bus, &session, "1").await;

        let (result, hooks) = transport.invoke("c1", "a1", bash()).await.unwrap();
        assert!(hooks.is_empty());
        match result {
            ToolResult::Ok(output) => assert_eq!(output.stdout, "hi"),
            ToolResult::Err(e) => panic!("expected a result, got {e:?}"),
        }
    }

    /// A reply nobody is waiting for is dropped, and the transport stays usable.
    ///
    /// This is the shape both restart cases arrive in — a node restart and a
    /// session restart are indistinguishable here, since each is simply a
    /// `call_id` with no waiter. Delivering it anywhere would resurrect a call
    /// the session has already journaled as interrupted.
    #[tokio::test]
    async fn a_reply_nobody_is_waiting_for_is_dropped() {
        let bus: Arc<dyn Bus> = Arc::new(MemoryBus::new());
        let session = Uuid::new_v4().to_string();
        let transport = BusTransport::open(bus.clone(), ACCOUNT, &session, "1")
            .await
            .unwrap();

        topics::runtime_out(bus.clone(), ACCOUNT, &session, "1")
            .publish(&answered("a-call-from-a-previous-life", "stale"))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            transport.pending_count().await,
            0,
            "an orphan reply must not register a waiter"
        );

        // And the transport still works afterwards.
        echo(bus, &session, "1").await;
        assert!(transport.invoke("c1", "a1", bash()).await.is_ok());
    }

    /// A sandbox from an earlier provision publishes onto its own out topic, and
    /// nothing on this one is listening to it.
    #[tokio::test]
    async fn a_reply_from_another_incarnation_never_reaches_this_one() {
        let bus: Arc<dyn Bus> = Arc::new(MemoryBus::new());
        let session = Uuid::new_v4().to_string();
        let transport = BusTransport::open(bus.clone(), ACCOUNT, &session, "2")
            .await
            .unwrap();
        echo(bus.clone(), &session, "1").await;

        let call = transport.invoke("c1", "a1", bash());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(150), call)
                .await
                .is_err(),
            "incarnation 1's echo must not answer incarnation 2's call"
        );
    }

    /// And the same fence between accounts. One bus serves the deployment, and a
    /// runtime id may be a vendor-minted label rather than a UUID — so two
    /// accounts naming a sandbox `my-laptop` is ordinary, not contrived.
    #[tokio::test]
    async fn a_reply_for_another_accounts_runtime_never_reaches_this_one() {
        let bus: Arc<dyn Bus> = Arc::new(MemoryBus::new());
        let transport = BusTransport::open(bus.clone(), ACCOUNT, "my-laptop", "1")
            .await
            .unwrap();

        // Another account's pump, answering for a runtime of the same name.
        let mut theirs = topics::runtime_in(bus.clone(), "acct-2", "my-laptop", "1")
            .subscribe()
            .await
            .unwrap();
        let out = topics::runtime_out(bus, "acct-2", "my-laptop", "1");
        tokio::spawn(async move {
            while let Some(message) = theirs.recv().await {
                let call_id = horsie_runtime_host::inbound_call_id(&message).to_string();
                let _ = out.publish(&answered(&call_id, "theirs")).await;
            }
        });

        let call = transport.invoke("c1", "a1", bash());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(150), call)
                .await
                .is_err(),
            "another account's sandbox must never answer this account's call"
        );
    }
}
