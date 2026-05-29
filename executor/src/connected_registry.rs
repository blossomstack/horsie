use futures_util::SinkExt;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, oneshot};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

pub type RuntimeSink =
    Arc<Mutex<futures_util::stream::SplitSink<WebSocketStream<TcpStream>, Message>>>;

struct Inner {
    sinks: HashMap<String, RuntimeSink>,
    pending: HashMap<String, oneshot::Sender<()>>,
}

/// Tracks live WebSocket connections from runtime binaries.
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
                sinks: HashMap::new(),
                pending: HashMap::new(),
            }),
        }
    }

    /// Register a runtime's WS sink. Resolves any pending `notify_when_ready` waiter.
    pub async fn register(&self, runtime_id: String, sink: RuntimeSink) {
        let mut inner = self.inner.lock().await;
        inner.sinks.insert(runtime_id.clone(), sink);
        if let Some(tx) = inner.pending.remove(&runtime_id) {
            let _ = tx.send(());
        }
    }

    /// Returns a receiver that resolves when `register` is called for `runtime_id`.
    /// Must be called BEFORE the process is spawned.
    pub async fn notify_when_ready(&self, runtime_id: &str) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        self.inner
            .lock()
            .await
            .pending
            .insert(runtime_id.to_string(), tx);
        rx
    }

    /// Look up a connected runtime's sink.
    pub async fn get_sink(&self, runtime_id: &str) -> Option<RuntimeSink> {
        self.inner.lock().await.sinks.get(runtime_id).cloned()
    }

    /// Remove a runtime (called when its WS connection drops).
    pub async fn remove(&self, runtime_id: &str) {
        self.inner.lock().await.sinks.remove(runtime_id);
    }

    /// Send a serialized message to a connected runtime.
    pub async fn send_to(
        &self,
        runtime_id: &str,
        json: String,
    ) -> Result<(), crate::error::ExecutorError> {
        match self.get_sink(runtime_id).await {
            Some(sink) => sink
                .lock()
                .await
                .send(Message::Text(json.into()))
                .await
                .map_err(|e| crate::error::ExecutorError::SendFailed(e.to_string())),
            None => Err(crate::error::ExecutorError::SendFailed(format!(
                "runtime '{runtime_id}' not connected"
            ))),
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
    async fn notify_resolves_when_registered() {
        let reg = ConnectedRuntimeRegistry::new();
        let rx = reg.notify_when_ready("rt-1").await;
        let has_pending = reg.inner.lock().await.pending.contains_key("rt-1");
        assert!(has_pending);
        drop(rx);
    }

    #[tokio::test]
    async fn get_sink_returns_none_for_unknown() {
        let reg = ConnectedRuntimeRegistry::new();
        assert!(reg.get_sink("ghost").await.is_none());
    }

    #[tokio::test]
    async fn remove_does_not_panic_on_missing() {
        let reg = ConnectedRuntimeRegistry::new();
        reg.remove("ghost").await;
    }
}
