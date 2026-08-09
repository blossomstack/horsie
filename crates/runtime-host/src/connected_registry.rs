use crate::RuntimeTransport;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, oneshot};

struct Inner {
    transports: HashMap<String, Arc<dyn RuntimeTransport>>,
    /// Every waiter for a runtime, not the latest one.
    ///
    /// A single slot looked sufficient while one acquisition owned a runtime's
    /// whole life, and stopped being so the moment a `get` could arrive while a
    /// `create` was still waiting: the second waiter evicted the first, whose
    /// background task then saw a cancelled channel, read it as "nobody wants
    /// this runtime any more", and destroyed the container the second waiter
    /// was about to be handed.
    pending: HashMap<String, Vec<oneshot::Sender<Result<(), String>>>>,
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
        Self::resolve(&mut inner, &runtime_id, &Ok(()));
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
        Self::resolve(&mut inner, &runtime_id, &Ok(()));
        true
    }

    /// Hand `outcome` to everyone waiting on `runtime_id` and forget them.
    fn resolve(inner: &mut Inner, runtime_id: &str, outcome: &Result<(), String>) {
        for tx in inner.pending.remove(runtime_id).unwrap_or_default() {
            let _ = tx.send(outcome.clone());
        }
    }

    /// Returns a receiver that resolves when `register_transport` is called for
    /// `runtime_id` (with `Ok`) or [`fail_pending`](Self::fail_pending) reports a
    /// provisioning failure (with `Err(message)`). Must be called BEFORE the
    /// process is spawned.
    ///
    /// Waiters accumulate: a second one never displaces a first, because a
    /// runtime can legitimately be awaited twice at once — an acquisition
    /// arriving while its create is still in flight — and both must be answered.
    pub async fn notify_when_ready(
        &self,
        runtime_id: &str,
    ) -> oneshot::Receiver<Result<(), String>> {
        let (tx, rx) = oneshot::channel();
        self.inner
            .lock()
            .await
            .pending
            .entry(runtime_id.to_string())
            .or_default()
            .push(tx);
        rx
    }

    /// Whether anything is already waiting for this runtime to dial back.
    ///
    /// What a vendor asks before deciding to (re)build: an acquisition that
    /// finds a create already waiting must join the wait, never start a second
    /// substrate object for the same runtime.
    pub async fn is_awaited(&self, runtime_id: &str) -> bool {
        self.inner.lock().await.pending.contains_key(runtime_id)
    }

    /// Resolve every pending `notify_when_ready` waiter with an error (e.g. the
    /// runtime reported failed provisioning and exited). No-op without waiters.
    pub async fn fail_pending(&self, runtime_id: &str, message: String) {
        let mut inner = self.inner.lock().await;
        Self::resolve(&mut inner, runtime_id, &Err(message));
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
    use crate::MockTransport;

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
        let still_registered = reg
            .runtime_transport("rt-1")
            .await
            .expect("still registered");
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

    /// A `get` arriving while a `create` is still waiting is ordinary, and both
    /// have to be answered. While the second waiter displaced the first, the
    /// create's background task saw a cancelled channel, concluded nobody
    /// wanted the runtime, and deleted the container out from under the
    /// acquisition that had just asked for it.
    #[tokio::test]
    async fn a_second_waiter_joins_the_first_rather_than_replacing_it() {
        let reg = ConnectedRuntimeRegistry::new();
        let first = reg.notify_when_ready("rt-1").await;
        let second = reg.notify_when_ready("rt-1").await;
        assert!(reg.is_awaited("rt-1").await);

        reg.register_transport("rt-1".into(), Arc::new(MockTransport::ok("")))
            .await;

        assert!(first.await.unwrap().is_ok(), "the first waiter was dropped");
        assert!(second.await.unwrap().is_ok());
        assert!(
            !reg.is_awaited("rt-1").await,
            "a resolved runtime is no longer awaited"
        );
    }

    #[tokio::test]
    async fn a_failure_reaches_every_waiter() {
        let reg = ConnectedRuntimeRegistry::new();
        let first = reg.notify_when_ready("rt-1").await;
        let second = reg.notify_when_ready("rt-1").await;
        reg.fail_pending("rt-1", "boom".into()).await;
        assert!(first.await.unwrap().unwrap_err().contains("boom"));
        assert!(second.await.unwrap().unwrap_err().contains("boom"));
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
