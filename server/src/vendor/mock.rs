//! A signal-recording [`RuntimeVendor`] for tests: every lifecycle call is
//! appended to a shared log (`create:<id>` / `attach:<id>` / `stop:<id>` /
//! `delete:<id>`), so tests can assert the invariant that every user action on a
//! session emits exactly the specified vendor signal.

use crate::vendor::{RuntimeSpec, RuntimeVendor, VendorError, VendorRuntime, VendorRuntimeHandle};
use async_trait::async_trait;
use horsie_runtime_client::{MockTransport, RuntimeClient};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct MockVendor {
    signals: Arc<Mutex<Vec<String>>>,
    fail_attach: Arc<Mutex<u32>>,
    fail_create: bool,
}

impl Default for MockVendor {
    fn default() -> Self {
        Self::new()
    }
}

impl MockVendor {
    pub fn new() -> Self {
        Self {
            signals: Arc::new(Mutex::new(Vec::new())),
            fail_attach: Arc::new(Mutex::new(0)),
            fail_create: false,
        }
    }

    /// Make the next `n` attach calls fail with [`VendorError::Attach`].
    #[must_use]
    pub fn fail_attach_times(self, n: u32) -> Self {
        if let Ok(mut guard) = self.fail_attach.lock() {
            *guard = n;
        }
        self
    }

    /// Make every create call fail with [`VendorError::Provision`].
    #[must_use]
    pub fn fail_create(mut self) -> Self {
        self.fail_create = true;
        self
    }

    /// Every signal recorded so far, in order.
    pub fn signals(&self) -> Vec<String> {
        self.signals
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    fn record(&self, signal: String) {
        match self.signals.lock() {
            Ok(mut g) => g.push(signal),
            Err(e) => e.into_inner().push(signal),
        }
    }

    fn runtime(&self, runtime_id: &str) -> VendorRuntime {
        VendorRuntime {
            runtime_client: RuntimeClient::new(MockTransport::ok("")),
            handle: Arc::new(MockHandle {
                signals: self.signals.clone(),
                runtime_id: runtime_id.to_string(),
            }),
        }
    }
}

#[async_trait]
impl RuntimeVendor for MockVendor {
    fn name(&self) -> &'static str {
        "mock"
    }

    async fn create(
        &self,
        runtime_id: &str,
        _spec: &RuntimeSpec,
    ) -> Result<VendorRuntime, VendorError> {
        self.record(format!("create:{runtime_id}"));
        if self.fail_create {
            return Err(VendorError::Provision("mock create failure".to_string()));
        }
        Ok(self.runtime(runtime_id))
    }

    async fn attach(
        &self,
        runtime_id: &str,
        _spec: &RuntimeSpec,
    ) -> Result<VendorRuntime, VendorError> {
        self.record(format!("attach:{runtime_id}"));
        let should_fail = {
            match self.fail_attach.lock() {
                Ok(mut guard) => {
                    if *guard > 0 {
                        *guard -= 1;
                        true
                    } else {
                        false
                    }
                }
                Err(_) => false,
            }
        };
        if should_fail {
            return Err(VendorError::Attach("mock attach failure".to_string()));
        }
        Ok(self.runtime(runtime_id))
    }

    async fn delete(&self, runtime_id: &str) {
        self.record(format!("delete:{runtime_id}"));
    }
}

struct MockHandle {
    signals: Arc<Mutex<Vec<String>>>,
    runtime_id: String,
}

#[async_trait]
impl VendorRuntimeHandle for MockHandle {
    async fn stop(&self) {
        let signal = format!("stop:{}", self.runtime_id);
        match self.signals.lock() {
            Ok(mut g) => g.push(signal),
            Err(e) => e.into_inner().push(signal),
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

    fn test_spec() -> RuntimeSpec {
        RuntimeSpec {
            workspaces: vec![],
            capabilities_file: std::env::temp_dir().join("caps.json"),
            plugins_dir: None,
            hook_path: vec![],
        }
    }

    #[tokio::test]
    async fn mock_vendor_records_signals_and_fails_attach_on_demand() {
        let v = MockVendor::new().fail_attach_times(1);
        let spec = test_spec();
        assert!(v.create("s1", &spec).await.is_ok());
        assert!(v.attach("s1", &spec).await.is_err()); // first attach fails
        assert!(v.attach("s1", &spec).await.is_ok()); // then succeeds
        v.delete("s1").await;
        assert_eq!(
            v.signals(),
            vec!["create:s1", "attach:s1", "attach:s1", "delete:s1"]
        );
    }

    #[tokio::test]
    async fn handle_stop_records_signal() {
        let v = MockVendor::new();
        let rt = v.create("s2", &test_spec()).await.unwrap();
        rt.handle.stop().await;
        assert_eq!(v.signals(), vec!["create:s2", "stop:s2"]);
    }

    #[tokio::test]
    async fn fail_create_fails_every_create() {
        let v = MockVendor::new().fail_create();
        assert!(v.create("s3", &test_spec()).await.is_err());
    }
}
