use crate::{error::RuntimeError, provider::RuntimeHandle};
use models::executor::{RuntimeConfig, RuntimeInfo, RuntimeState};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

struct RuntimeEntry {
    id: String,
    state: RuntimeState,
    config: RuntimeConfig,
    restart_count: u32,
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
    pub async fn begin_create(&self, id: &str, config: RuntimeConfig) -> Result<(), RuntimeError> {
        let mut entries = self.entries.lock().await;
        if entries.contains_key(id) {
            return Err(RuntimeError::AlreadyExists(id.to_string()));
        }
        entries.insert(
            id.to_string(),
            RuntimeEntry {
                id: id.to_string(),
                state: RuntimeState::Creating,
                config,
                restart_count: 0,
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

    /// Transition Running|Failed → Creating (restart in place); increments restart_count.
    /// Returns old handle for cleanup.
    pub async fn begin_restart(
        &self,
        id: &str,
    ) -> Result<Option<Arc<dyn RuntimeHandle>>, RuntimeError> {
        let mut entries = self.entries.lock().await;
        let entry = entries
            .get_mut(id)
            .ok_or_else(|| RuntimeError::NotFound(id.to_string()))?;
        match entry.state.clone() {
            RuntimeState::Running | RuntimeState::Failed => {
                entry.state = RuntimeState::Creating;
                entry.restart_count += 1;
                Ok(entry.handle.take())
            }
            s @ RuntimeState::Creating | s @ RuntimeState::Stopping | s @ RuntimeState::Stopped => {
                Err(RuntimeError::InvalidTransition {
                    from: format!("{s:?}"),
                    action: "restart".to_string(),
                })
            }
        }
    }

    /// Snapshot all entries as RuntimeInfo.
    pub async fn list(&self) -> Vec<RuntimeInfo> {
        self.entries
            .lock()
            .await
            .values()
            .map(|e| RuntimeInfo {
                runtime_id: e.id.clone(),
                state: e.state.clone(),
                restart_count: e.restart_count,
            })
            .collect()
    }

    /// IDs and handles of all Running runtimes (for health check).
    pub async fn running_handles(&self) -> Vec<(String, Arc<dyn RuntimeHandle>)> {
        self.entries
            .lock()
            .await
            .values()
            .filter_map(|e| {
                if matches!(e.state, RuntimeState::Running) {
                    e.handle.clone().map(|h| (e.id.clone(), h))
                } else {
                    None
                }
            })
            .collect()
    }

    pub async fn get_config(&self, id: &str) -> Option<RuntimeConfig> {
        self.entries.lock().await.get(id).map(|e| e.config.clone())
    }

    pub async fn get_restart_count(&self, id: &str) -> Option<u32> {
        self.entries.lock().await.get(id).map(|e| e.restart_count)
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

    fn cfg() -> RuntimeConfig {
        RuntimeConfig {
            working_dir: "/tmp".to_string(),
        }
    }

    #[tokio::test]
    async fn test_begin_create_inserts_creating_entry() {
        let r = RuntimeRegistry::new();
        r.begin_create("rt-1", cfg()).await.unwrap();
        let list = r.list().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].runtime_id, "rt-1");
        assert!(matches!(list[0].state, RuntimeState::Creating));
    }

    #[tokio::test]
    async fn test_begin_create_fails_on_duplicate() {
        let r = RuntimeRegistry::new();
        r.begin_create("rt-1", cfg()).await.unwrap();
        let result = r.begin_create("rt-1", cfg()).await;
        assert!(matches!(result, Err(RuntimeError::AlreadyExists(_))));
    }

    #[tokio::test]
    async fn test_complete_create_transitions_to_running() {
        let r = RuntimeRegistry::new();
        r.begin_create("rt-1", cfg()).await.unwrap();
        r.complete_create("rt-1", Arc::new(NullHandle))
            .await
            .unwrap();
        let list = r.list().await;
        assert!(matches!(list[0].state, RuntimeState::Running));
    }

    #[tokio::test]
    async fn test_begin_stop_transitions_to_stopping() {
        let r = RuntimeRegistry::new();
        r.begin_create("rt-1", cfg()).await.unwrap();
        r.complete_create("rt-1", Arc::new(NullHandle))
            .await
            .unwrap();
        let handle = r.begin_stop("rt-1").await.unwrap();
        assert!(handle.is_some());
        let list = r.list().await;
        assert!(matches!(list[0].state, RuntimeState::Stopping));
    }

    #[tokio::test]
    async fn test_begin_stop_from_creating_fails() {
        let r = RuntimeRegistry::new();
        r.begin_create("rt-1", cfg()).await.unwrap();
        let result = r.begin_stop("rt-1").await;
        assert!(matches!(
            result,
            Err(RuntimeError::InvalidTransition { .. })
        ));
    }

    #[tokio::test]
    async fn test_complete_stop_removes_entry() {
        let r = RuntimeRegistry::new();
        r.begin_create("rt-1", cfg()).await.unwrap();
        r.complete_create("rt-1", Arc::new(NullHandle))
            .await
            .unwrap();
        r.begin_stop("rt-1").await.unwrap();
        r.complete_stop("rt-1").await.unwrap();
        assert!(r.list().await.is_empty());
    }

    #[tokio::test]
    async fn test_begin_restart_increments_restart_count() {
        let r = RuntimeRegistry::new();
        r.begin_create("rt-1", cfg()).await.unwrap();
        r.complete_create("rt-1", Arc::new(NullHandle))
            .await
            .unwrap();
        r.begin_restart("rt-1").await.unwrap();
        assert_eq!(r.get_restart_count("rt-1").await, Some(1));
        assert!(matches!(r.list().await[0].state, RuntimeState::Creating));
    }

    #[tokio::test]
    async fn test_mark_failed_clears_handle() {
        let r = RuntimeRegistry::new();
        r.begin_create("rt-1", cfg()).await.unwrap();
        r.complete_create("rt-1", Arc::new(NullHandle))
            .await
            .unwrap();
        r.mark_failed("rt-1").await.unwrap();
        let list = r.list().await;
        assert!(matches!(list[0].state, RuntimeState::Failed));
        assert!(r.running_handles().await.is_empty());
    }

    #[tokio::test]
    async fn test_running_handles_returns_only_running() {
        let r = RuntimeRegistry::new();
        r.begin_create("rt-1", cfg()).await.unwrap();
        r.complete_create("rt-1", Arc::new(NullHandle))
            .await
            .unwrap();
        r.begin_create("rt-2", cfg()).await.unwrap(); // still Creating
        let handles = r.running_handles().await;
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].0, "rt-1");
    }
}
