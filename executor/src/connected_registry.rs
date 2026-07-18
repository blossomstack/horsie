use horsie_runtime_client::RuntimeTransport;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, oneshot};

struct Inner {
    transports: HashMap<String, Arc<dyn RuntimeTransport>>,
    pending: HashMap<String, oneshot::Sender<Result<(), String>>>,
}

/// Tracks the tool-call transport of each live runtime connection. The unit of
/// storage is `Arc<dyn RuntimeTransport>` so a future provider can register a
/// different transport impl (unix, tcp, in-container, …) without changing callers.
pub struct ConnectedRuntimeRegistry {
    inner: Mutex<Inner>,
}

impl Default for ConnectedRuntimeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectedRuntimeRegistry {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                transports: HashMap::new(),
                pending: HashMap::new(),
            }),
        }
    }

    /// Register a runtime's tool transport. Resolves any pending `notify_when_ready`
    /// waiter — callers register the transport *before* signaling ready, so
    /// `runtime_transport` is never `None` once the waiter fires.
    pub async fn register_transport(
        &self,
        runtime_id: String,
        transport: Arc<dyn RuntimeTransport>,
    ) {
        let mut inner = self.inner.lock().await;
        inner.transports.insert(runtime_id.clone(), transport);
        if let Some(tx) = inner.pending.remove(&runtime_id) {
            let _ = tx.send(Ok(()));
        }
    }

    /// Register a runtime's tool transport only if `runtime_id` isn't already
    /// live. Returns `false` (leaving the existing transport untouched) on a
    /// collision — used by vendors whose announced id is a caller-chosen
    /// label that could collide (unlike the unique per-attempt ids other
    /// vendors mint).
    pub async fn try_register_transport(
        &self,
        runtime_id: String,
        transport: Arc<dyn RuntimeTransport>,
    ) -> bool {
        let mut inner = self.inner.lock().await;
        if inner.transports.contains_key(&runtime_id) {
            return false;
        }
        inner.transports.insert(runtime_id.clone(), transport);
        if let Some(tx) = inner.pending.remove(&runtime_id) {
            let _ = tx.send(Ok(()));
        }
        true
    }

    /// Returns a receiver that resolves when `register_transport` is called for
    /// `runtime_id` (with `Ok`) or [`fail_pending`](Self::fail_pending) reports a
    /// provisioning failure (with `Err(message)`). Must be called BEFORE the
    /// process is spawned.
    pub async fn notify_when_ready(
        &self,
        runtime_id: &str,
    ) -> oneshot::Receiver<Result<(), String>> {
        let (tx, rx) = oneshot::channel();
        self.inner
            .lock()
            .await
            .pending
            .insert(runtime_id.to_string(), tx);
        rx
    }

    /// Resolve a pending `notify_when_ready` waiter with an error (e.g. the
    /// runtime reported failed provisioning and exited). No-op without a waiter.
    pub async fn fail_pending(&self, runtime_id: &str, message: String) {
        if let Some(tx) = self.inner.lock().await.pending.remove(runtime_id) {
            let _ = tx.send(Err(message));
        }
    }

    /// Look up a connected runtime's tool transport.
    pub async fn runtime_transport(&self, runtime_id: &str) -> Option<Arc<dyn RuntimeTransport>> {
        self.inner.lock().await.transports.get(runtime_id).cloned()
    }

    /// Remove a runtime (called when its connection drops or it is destroyed).
    pub async fn remove(&self, runtime_id: &str) {
        self.inner.lock().await.transports.remove(runtime_id);
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
    use horsie_runtime_client::MockTransport;

    #[tokio::test]
    async fn register_resolves_pending_waiter_and_stores_transport() {
        let reg = ConnectedRuntimeRegistry::new();
        let rx = reg.notify_when_ready("rt-1").await;
        assert!(reg.runtime_transport("rt-1").await.is_none());
        reg.register_transport("rt-1".into(), Arc::new(MockTransport::ok("")))
            .await;
        // The readiness waiter fired with success ...
        assert!(rx.await.unwrap().is_ok());
        // ... and the transport is retrievable.
        assert!(reg.runtime_transport("rt-1").await.is_some());
    }

    #[tokio::test]
    async fn try_register_transport_rejects_a_live_collision() {
        let reg = ConnectedRuntimeRegistry::new();
        let first: Arc<dyn RuntimeTransport> = Arc::new(MockTransport::ok("first"));
        assert!(
            reg.try_register_transport("rt-1".into(), first.clone())
                .await
        );
        let second: Arc<dyn RuntimeTransport> = Arc::new(MockTransport::ok("second"));
        assert!(
            !reg.try_register_transport("rt-1".into(), second).await,
            "a live collision must be rejected"
        );
        let still_registered = reg.runtime_transport("rt-1").await.expect("still registered");
        assert!(
            Arc::ptr_eq(&first, &still_registered),
            "collision must not disturb the original transport"
        );
    }

    #[tokio::test]
    async fn fail_pending_resolves_waiter_with_error() {
        let reg = ConnectedRuntimeRegistry::new();
        let rx = reg.notify_when_ready("rt-1").await;
        reg.fail_pending("rt-1", "git clone failed: boom".into())
            .await;
        let err = rx.await.unwrap().unwrap_err();
        assert!(err.contains("boom"));
        assert!(reg.runtime_transport("rt-1").await.is_none());
    }

    #[tokio::test]
    async fn runtime_transport_none_for_unknown() {
        let reg = ConnectedRuntimeRegistry::new();
        assert!(reg.runtime_transport("ghost").await.is_none());
    }

    #[tokio::test]
    async fn remove_clears_transport() {
        let reg = ConnectedRuntimeRegistry::new();
        reg.register_transport("rt-1".into(), Arc::new(MockTransport::ok("")))
            .await;
        reg.remove("rt-1").await;
        assert!(reg.runtime_transport("rt-1").await.is_none());
    }
}
