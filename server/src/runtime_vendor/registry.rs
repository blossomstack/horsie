//! Tracks every connected vendor agent and mirrors it into the shared vendor
//! map sessions select from.
//!
//! This deliberately mirrors [`LocalDaemonRegistry`](crate::runtime_vendor::LocalDaemonRegistry):
//! the same `SharedVendors` map, the same publish-on-connect shape. The one
//! difference is what a reconnect means — see [`RuntimeVendorRegistry::register`].

use crate::runtime_vendor::RuntimeVendorLink;
use crate::sessions::spec::SharedVendors;
use std::sync::{Arc, PoisonError};

pub struct RuntimeVendorRegistry {
    vendors: SharedVendors,
}

impl RuntimeVendorRegistry {
    #[must_use]
    pub fn new(vendors: SharedVendors) -> Self {
        Self { vendors }
    }

    /// Publish a freshly handshaken link under the name it announced.
    ///
    /// A reconnecting agent *replaces* its own previous entry, unlike the local
    /// daemon registry which preserves vendor object identity across
    /// reconnects. The reason is that the old link is a dead socket whose
    /// runtimes are gone: keeping it would strand every session on a transport
    /// that can never answer, and `RuntimeClient` would latch each of them
    /// disconnected in turn.
    pub fn register(&self, link: Arc<RuntimeVendorLink>) {
        let name = link.vendor_name().to_string();
        self.vendors
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(name, link);
    }

    /// Names currently published. Used by tests today; the settings view's
    /// live-vendor list reads the same map.
    #[must_use]
    pub fn connected_names(&self) -> Vec<String> {
        self.vendors
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
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
    use crate::runtime_vendor::fake::FakeRuntimeVendor;
    use std::collections::HashMap;
    use std::sync::RwLock;

    fn empty_vendors() -> SharedVendors {
        Arc::new(RwLock::new(HashMap::new()))
    }

    #[tokio::test]
    async fn register_publishes_the_agent_under_its_announced_name() {
        let vendors = empty_vendors();
        let registry = RuntimeVendorRegistry::new(vendors.clone());
        let agent = FakeRuntimeVendor::builder("my-laptop")
            .supports_provisioning(false)
            .serve_in_process()
            .await
            .expect("agent");
        registry.register(agent.link());

        assert_eq!(registry.connected_names(), vec!["my-laptop".to_string()]);
        let published = vendors.read().unwrap();
        let vendor = published.get("my-laptop").expect("published");
        assert!(
            !vendor.capabilities().supports_provisioning,
            "the published vendor must carry what the agent announced"
        );
    }

    #[tokio::test]
    async fn a_reconnecting_agent_replaces_its_dead_link() {
        let vendors = empty_vendors();
        let registry = RuntimeVendorRegistry::new(vendors.clone());

        let first = FakeRuntimeVendor::builder("same-name")
            .serve_in_process()
            .await
            .expect("first agent");
        registry.register(first.link());
        first.disconnect();

        let second = FakeRuntimeVendor::builder("same-name")
            .serve_in_process()
            .await
            .expect("second agent");
        registry.register(second.link());

        // The published vendor must be the live one: a create routed through it
        // reaches the second agent, not the corpse of the first.
        let vendor = vendors.read().unwrap().get("same-name").cloned().unwrap();
        vendor
            .create(
                "rt-1",
                &crate::runtime_vendor::fake::runtime_spec_fixture("main"),
            )
            .await
            .expect("create must reach the live agent");
        assert_eq!(second.signals(), vec!["create:rt-1".to_string()]);
        assert!(first.signals().is_empty(), "the dead link must not be used");
    }
}
