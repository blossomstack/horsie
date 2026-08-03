//! Tracks every connected vendor agent and mirrors it into the shared vendor
//! map sessions select from.
//!
//! This deliberately mirrors [`LocalDaemonRegistry`](crate::runtime_vendor::LocalDaemonRegistry):
//! the same `SharedVendors` map, the same publish-on-connect shape. The one
//! difference is what a reconnect means — see [`RuntimeVendorRegistry::register`].

use crate::runtime_vendor::RuntimeVendorLink;
use crate::sessions::spec::SharedVendors;
use std::sync::{Arc, PoisonError};

/// Why a registration was refused.
#[derive(Debug, PartialEq)]
pub enum RegisterError {
    /// A live link owned by someone else already answers to this name.
    NameTaken { by: String },
}

impl std::fmt::Display for RegisterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NameTaken { by } => {
                write!(f, "that vendor name is already held by {by}")
            }
        }
    }
}

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
    /// A *different* principal claiming a live name is refused: silently
    /// replacing it is how a stranger takes over someone's laptop and starts
    /// receiving their tool calls. The same principal reconnecting still
    /// replaces its own entry, so a dropped socket recovers, and with
    /// authentication disabled every principal is `Anonymous`, which preserves
    /// today's behaviour exactly.
    pub fn register(&self, link: Arc<RuntimeVendorLink>) -> Result<(), RegisterError> {
        let name = link.vendor_name().to_string();
        let mut vendors = self.vendors.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(existing) = vendors.get(&name)
            && existing.owner() != link.owner()
        {
            return Err(RegisterError::NameTaken {
                by: existing.owner().to_db(),
            });
        }
        vendors.insert(name, link);
        Ok(())
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
    use crate::auth::Principal;
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
        registry.register(agent.link()).expect("registers");

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
        registry.register(first.link()).expect("registers");
        first.disconnect();

        let second = FakeRuntimeVendor::builder("same-name")
            .serve_in_process()
            .await
            .expect("second agent");
        registry
            .register(second.link())
            .expect("reconnect replaces");

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

    #[tokio::test]
    async fn a_different_principal_cannot_take_over_a_live_vendor_name() {
        let vendors = empty_vendors();
        let registry = RuntimeVendorRegistry::new(vendors.clone());

        let mine = FakeRuntimeVendor::builder("my-laptop")
            .owned_by(Principal::User(1))
            .serve_in_process()
            .await
            .expect("agent");
        registry.register(mine.link()).expect("first claim wins");

        // The hole this closes: before ownership, this silently replaced the
        // live link and started receiving its tool calls.
        let attacker = FakeRuntimeVendor::builder("my-laptop")
            .owned_by(Principal::User(2))
            .serve_in_process()
            .await
            .expect("agent");
        assert_eq!(
            registry.register(attacker.link()),
            Err(RegisterError::NameTaken {
                by: "user:1".to_string()
            })
        );

        // ...and the original is untouched: still exactly one entry, still
        // owned by the principal that claimed it.
        assert_eq!(registry.connected_names(), vec!["my-laptop".to_string()]);
        assert_eq!(mine.link().owner(), &Principal::User(1));
    }

    #[tokio::test]
    async fn the_same_principal_reconnecting_still_replaces_its_own_entry() {
        let vendors = empty_vendors();
        let registry = RuntimeVendorRegistry::new(vendors.clone());

        let first = FakeRuntimeVendor::builder("same-name")
            .owned_by(Principal::User(7))
            .serve_in_process()
            .await
            .expect("agent");
        registry.register(first.link()).expect("first");

        let second = FakeRuntimeVendor::builder("same-name")
            .owned_by(Principal::User(7))
            .serve_in_process()
            .await
            .expect("agent");
        // A dropped socket must recover: the old link is dead, and keeping it
        // would strand every session on a transport that can never answer.
        registry
            .register(second.link())
            .expect("reconnect replaces");
        assert_eq!(registry.connected_names(), vec!["same-name".to_string()]);
    }

    #[tokio::test]
    async fn different_principals_may_hold_different_names() {
        let vendors = empty_vendors();
        let registry = RuntimeVendorRegistry::new(vendors.clone());
        for (name, who) in [("laptop-a", 1), ("laptop-b", 2)] {
            let agent = FakeRuntimeVendor::builder(name)
                .owned_by(Principal::User(who))
                .serve_in_process()
                .await
                .expect("agent");
            registry.register(agent.link()).expect("distinct names");
            std::mem::forget(agent);
        }
        let mut names = registry.connected_names();
        names.sort();
        assert_eq!(names, vec!["laptop-a".to_string(), "laptop-b".to_string()]);
    }
}
