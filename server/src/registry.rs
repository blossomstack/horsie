use crate::error::ServerError;
use async_trait::async_trait;
use models::executor::{ExecutorCommand, ExecutorInboundMessage};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

#[async_trait]
pub(crate) trait CommandSink: Send + Sync {
    async fn send(&self, msg: ExecutorInboundMessage) -> Result<(), ServerError>;
}

pub(crate) struct ExecutorConn {
    pub executor_id: String,
    sink: Arc<dyn CommandSink>,
}

impl ExecutorConn {
    pub fn new(executor_id: String, sink: Arc<dyn CommandSink>) -> Self {
        Self { executor_id, sink }
    }

    pub async fn send_command(
        &self,
        request_id: String,
        command: ExecutorCommand,
    ) -> Result<(), ServerError> {
        self.sink
            .send(ExecutorInboundMessage {
                request_id,
                command,
            })
            .await
    }
}

pub(crate) struct ExecutorRegistry {
    conns: Mutex<HashMap<String, Arc<ExecutorConn>>>,
}

impl ExecutorRegistry {
    pub fn new() -> Self {
        Self {
            conns: Mutex::new(HashMap::new()),
        }
    }

    pub async fn register(&self, conn: Arc<ExecutorConn>) {
        self.conns
            .lock()
            .await
            .insert(conn.executor_id.clone(), conn);
    }

    pub async fn remove(&self, executor_id: &str) {
        self.conns.lock().await.remove(executor_id);
    }

    pub async fn send_command(
        &self,
        executor_id: &str,
        request_id: String,
        command: ExecutorCommand,
    ) -> Result<(), ServerError> {
        let conns = self.conns.lock().await;
        match conns.get(executor_id) {
            Some(conn) => conn.send_command(request_id, command).await,
            None => Err(ServerError::ExecutorNotFound(executor_id.to_string())),
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
    use models::executor::QueryRuntimesCmd;
    use tokio::sync::mpsc;

    struct MockSink {
        tx: mpsc::Sender<ExecutorInboundMessage>,
    }

    #[async_trait]
    impl CommandSink for MockSink {
        async fn send(&self, msg: ExecutorInboundMessage) -> Result<(), ServerError> {
            self.tx
                .send(msg)
                .await
                .map_err(|e| ServerError::SendFailed(e.to_string()))
        }
    }

    #[tokio::test]
    async fn test_register_and_send() {
        let registry = ExecutorRegistry::new();
        let (tx, mut rx) = mpsc::channel(8);
        let conn = Arc::new(ExecutorConn::new(
            "ex-1".to_string(),
            Arc::new(MockSink { tx }),
        ));
        registry.register(conn).await;

        registry
            .send_command(
                "ex-1",
                "req-1".to_string(),
                ExecutorCommand::QueryRuntimes(QueryRuntimesCmd {}),
            )
            .await
            .unwrap();

        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.request_id, "req-1");
        assert!(matches!(msg.command, ExecutorCommand::QueryRuntimes(_)));
    }

    #[tokio::test]
    async fn test_send_to_unknown_executor_fails() {
        let registry = ExecutorRegistry::new();
        let result = registry
            .send_command(
                "ghost",
                "req-1".to_string(),
                ExecutorCommand::QueryRuntimes(QueryRuntimesCmd {}),
            )
            .await;
        assert!(matches!(result, Err(ServerError::ExecutorNotFound(_))));
    }

    #[tokio::test]
    async fn test_remove_executor() {
        let registry = ExecutorRegistry::new();
        let (tx, _rx) = mpsc::channel(8);
        let conn = Arc::new(ExecutorConn::new(
            "ex-1".to_string(),
            Arc::new(MockSink { tx }),
        ));
        registry.register(conn).await;
        registry.remove("ex-1").await;
        let result = registry
            .send_command(
                "ex-1",
                "req-1".to_string(),
                ExecutorCommand::QueryRuntimes(QueryRuntimesCmd {}),
            )
            .await;
        assert!(matches!(result, Err(ServerError::ExecutorNotFound(_))));
    }
}
