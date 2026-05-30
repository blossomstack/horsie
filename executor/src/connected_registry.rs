use runtime_client::RuntimeTransport;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, oneshot};

struct Inner {
    transports: HashMap<String, Arc<dyn RuntimeTransport>>,
    pending: HashMap<String, oneshot::Sender<()>>,
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
            let _ = tx.send(());
        }
    }

    /// Returns a receiver that resolves when `register_transport` is called for
    /// `runtime_id`. Must be called BEFORE the process is spawned.
    pub async fn notify_when_ready(&self, runtime_id: &str) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        self.inner
            .lock()
            .await
            .pending
            .insert(runtime_id.to_string(), tx);
        rx
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
    use runtime_client::MockTransport;

    #[tokio::test]
    async fn register_resolves_pending_waiter_and_stores_transport() {
        let reg = ConnectedRuntimeRegistry::new();
        let rx = reg.notify_when_ready("rt-1").await;
        assert!(reg.runtime_transport("rt-1").await.is_none());
        reg.register_transport("rt-1".into(), Arc::new(MockTransport::ok("")))
            .await;
        // The readiness waiter fired ...
        rx.await.unwrap();
        // ... and the transport is retrievable.
        assert!(reg.runtime_transport("rt-1").await.is_some());
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
