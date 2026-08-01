use crate::{error::RuntimeError, provider::RuntimeHandle};
use horsie_models::executor::RuntimeState;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

struct RuntimeEntry {
    state: RuntimeState,
    handle: Option<Arc<dyn RuntimeHandle>>,
}

pub(crate) struct RuntimeRegistry {
    entries: Mutex<HashMap<String, RuntimeEntry>>,
}

impl RuntimeRegistry {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Insert a Creating entry; fails if id already exists.
    pub async fn begin_create(&self, id: &str) -> Result<(), RuntimeError> {
        let mut entries = self.entries.lock().await;
        if entries.contains_key(id) {
            return Err(RuntimeError::AlreadyExists(id.to_string()));
        }
        entries.insert(
            id.to_string(),
            RuntimeEntry {
                state: RuntimeState::Creating,
                handle: None,
            },
        );
        Ok(())
    }

    /// Transition Creating → Running; attach handle.
    pub async fn complete_create(
        &self,
        id: &str,
        handle: Arc<dyn RuntimeHandle>,
    ) -> Result<(), RuntimeError> {
        let mut entries = self.entries.lock().await;
        let entry = entries
            .get_mut(id)
            .ok_or_else(|| RuntimeError::NotFound(id.to_string()))?;
        entry.state = RuntimeState::Running;
        entry.handle = Some(handle);
        Ok(())
    }

    /// Transition any → Failed; clears handle.
    pub async fn mark_failed(&self, id: &str) -> Result<(), RuntimeError> {
        let mut entries = self.entries.lock().await;
        let entry = entries
            .get_mut(id)
            .ok_or_else(|| RuntimeError::NotFound(id.to_string()))?;
        entry.state = RuntimeState::Failed;
        entry.handle = None;
        Ok(())
    }

    /// Transition Running|Failed → Stopping; returns handle for cleanup.
    pub async fn begin_stop(
        &self,
        id: &str,
    ) -> Result<Option<Arc<dyn RuntimeHandle>>, RuntimeError> {
        let mut entries = self.entries.lock().await;
        let entry = entries
            .get_mut(id)
            .ok_or_else(|| RuntimeError::NotFound(id.to_string()))?;
        match entry.state.clone() {
            RuntimeState::Running | RuntimeState::Failed => {
                entry.state = RuntimeState::Stopping;
                Ok(entry.handle.take())
            }
            s @ RuntimeState::Creating | s @ RuntimeState::Stopping | s @ RuntimeState::Stopped => {
                Err(RuntimeError::InvalidTransition {
                    from: format!("{s:?}"),
                    action: "stop".to_string(),
                })
            }
        }
    }

    /// Remove entry after stop completes.
    pub async fn complete_stop(&self, id: &str) -> Result<(), RuntimeError> {
        self.entries
            .lock()
            .await
            .remove(id)
            .ok_or_else(|| RuntimeError::NotFound(id.to_string()))
            .map(|_| ())
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
    use crate::{error::RuntimeError, provider::HealthStatus};
    use async_trait::async_trait;

    struct NullHandle;

    #[async_trait]
    impl RuntimeHandle for NullHandle {
        async fn stop(&self) -> Result<(), RuntimeError> {
            Ok(())
        }
        async fn health_check(&self) -> Result<HealthStatus, RuntimeError> {
            Ok(HealthStatus::Healthy)
        }
    }

    #[tokio::test]
    async fn test_begin_create_inserts_creating_entry() {
        let r = RuntimeRegistry::new();
        r.begin_create("rt-1").await.unwrap();
        // Duplicate id is rejected while the entry exists.
        assert!(matches!(
            r.begin_create("rt-1").await,
            Err(RuntimeError::AlreadyExists(_))
        ));
    }

    #[tokio::test]
    async fn test_create_stop_lifecycle() {
        let r = RuntimeRegistry::new();
        r.begin_create("rt-1").await.unwrap();
        r.complete_create("rt-1", Arc::new(NullHandle))
            .await
            .unwrap();
        let handle = r.begin_stop("rt-1").await.unwrap();
        assert!(handle.is_some());
        r.complete_stop("rt-1").await.unwrap();
        // Entry gone: stopping again is a NotFound.
        assert!(matches!(
            r.begin_stop("rt-1").await,
            Err(RuntimeError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn test_begin_stop_from_creating_fails() {
        let r = RuntimeRegistry::new();
        r.begin_create("rt-1").await.unwrap();
        let result = r.begin_stop("rt-1").await;
        assert!(matches!(
            result,
            Err(RuntimeError::InvalidTransition { .. })
        ));
    }

    #[tokio::test]
    async fn test_mark_failed_allows_stop() {
        let r = RuntimeRegistry::new();
        r.begin_create("rt-1").await.unwrap();
        r.complete_create("rt-1", Arc::new(NullHandle))
            .await
            .unwrap();
        r.mark_failed("rt-1").await.unwrap();
        // A failed runtime is still stoppable (cleanup path).
        assert!(r.begin_stop("rt-1").await.unwrap().is_none());
    }
}
