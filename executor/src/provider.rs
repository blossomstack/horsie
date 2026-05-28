use crate::error::RuntimeError;
use async_trait::async_trait;
use models::executor::RuntimeConfig;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub enum HealthStatus {
    Healthy,
    Unhealthy { reason: String },
}

#[async_trait]
pub trait RuntimeHandle: Send + Sync {
    async fn stop(&self) -> Result<(), RuntimeError>;
    async fn health_check(&self) -> Result<HealthStatus, RuntimeError>;
}

#[async_trait]
pub trait RuntimeProvider: Send + Sync {
    async fn create(
        &self,
        id: &str,
        config: &RuntimeConfig,
    ) -> Result<Arc<dyn RuntimeHandle>, RuntimeError>;
}
