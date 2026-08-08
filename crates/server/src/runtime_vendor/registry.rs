//! Tracks every connected vendor agent and mirrors it into the shared vendor
//! map sessions select from.
//!
//! This deliberately mirrors [`LocalDaemonRegistry`](crate::runtime_vendor::LocalDaemonRegistry):
//! the same `SharedVendors` map, the same publish-on-connect shape. The one
//! difference is what a reconnect means — see [`RuntimeVendorRegistry::register`].

use crate::runtime_vendor::RemoteRuntimeVendor;
use crate::sessions::spec::SharedVendors;
use std::sync::{Arc, PoisonError};

/// Why a registration was refused.
#[derive(Debug, PartialEq)]
pub enum RegisterError {
    /// A live link already answers to this name, and it belongs to another
    /// agent process.
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

impl RegisterError {
    /// What the refused agent is told.
    ///
    /// Deliberately not [`Display`](std::fmt::Display), which names the holder:
    /// that belongs in the server log, not in a message handed to whoever just
    /// dialled. A refused stranger learns that the name is in use and nothing
    /// about who is using it.
    #[must_use]
    pub fn client_reason(&self, name: &str) -> String {
        match self {
            Self::NameTaken { .. } => {
                format!("vendor name \"{name}\" is already in use by another agent")
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
    /// A name is held by the process that claimed it, for as long as that
    /// process is connected. The gates, in order:
    ///
    /// 1. Nobody holds the name — publish.
    /// 2. A *different principal* holds it — refuse. This outranks the instance
    ///    id on purpose: an instance id is announced by the client and is not a
    ///    secret, so knowing one must never buy a stranger someone else's name.
    /// 3. The *same process* holds it (matching instance id) — replace. The old
    ///    link is a dead socket, and keeping it would strand every session on a
    ///    transport that can never answer while `RuntimeClient` latched them
    ///    disconnected one by one. This is what makes a reconnect after a
    ///    network blip immediate rather than a wait for the idle timeout.
    /// 4. Another process holds it but its link is already dead — replace. A
    ///    corpse must not hold a name; this covers the window between a read
    ///    loop ending and [`publish`](Self::publish)'s eviction task removing
    ///    the entry.
    /// 5. Otherwise — refuse. Two live agents cannot share one name, whatever
    ///    principal they present.
    ///
    /// With authentication disabled every principal is `Anonymous`, so gate 2
    /// never fires and 3–5 carry the whole policy.
    pub fn register(&self, link: Arc<RemoteRuntimeVendor>) -> Result<(), RegisterError> {
        let name = link.vendor_name().to_string();
        let mut vendors = self.vendors.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(existing) = vendors.get(&name) {
            let taken = || RegisterError::NameTaken {
                by: existing.owner().to_db(),
            };
            if existing.owner() != link.owner() {
                return Err(taken());
            }
            if existing.instance_id() != link.instance_id() && existing.is_connected() {
                return Err(taken());
            }
        }
        vendors.insert(name, link);
        Ok(())
    }

    /// Register a link and, on success, arrange for it to be unpublished when
    /// its socket dies.
    ///
    /// Eviction is compare-and-remove on *link identity*, not on the name or
    /// the instance id: a reconnecting process replaces its own entry while
    /// carrying the same name and the same instance id, so anything coarser
    /// would let the dead socket's eviction take the live link down with it.
    pub fn publish(self: &Arc<Self>, link: Arc<RemoteRuntimeVendor>) -> Result<(), RegisterError> {
        self.register(link.clone())?;
        let registry = self.clone();
        tokio::spawn(async move {
            link.closed().await;
            if registry.evict(&link) {
                tracing::info!(
                    vendor = %link.vendor_name(),
                    "vendor agent disconnected, name released"
                );
            }
        });
        Ok(())
    }

    /// Remove this link's name if this exact link still holds it. Returns
    /// whether it did.
    fn evict(&self, link: &Arc<RemoteRuntimeVendor>) -> bool {
        let mut vendors = self.vendors.write().unwrap_or_else(PoisonError::into_inner);
        match vendors.get(link.vendor_name()) {
            Some(held) if Arc::ptr_eq(held, link) => {
                vendors.remove(link.vendor_name());
                true
            }
            Some(_) | None => false,
        }
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
    use crate::auth::{Principal, UserId};
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

        // The same process dialling again: same instance id, but its own
        // recorder, so "the dead link was not used" stays assertable.
        let second = FakeRuntimeVendor::builder("same-name")
            .instance_id(first.link().instance_id())
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
            .owned_by(Principal::User(UserId::new("1")))
            .serve_in_process()
            .await
            .expect("agent");
        registry.register(mine.link()).expect("first claim wins");

        // The hole this closes: before ownership, this silently replaced the
        // live link and started receiving its tool calls.
        let attacker = FakeRuntimeVendor::builder("my-laptop")
            .owned_by(Principal::User(UserId::new("2")))
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
        assert_eq!(mine.link().owner(), &Principal::User(UserId::new("1")));
    }

    #[tokio::test]
    async fn a_second_process_cannot_take_a_name_that_is_in_use() {
        let vendors = empty_vendors();
        let registry = RuntimeVendorRegistry::new(vendors.clone());

        let mine = FakeRuntimeVendor::builder("horsie-local")
            .owned_by(Principal::User(UserId::new("7")))
            .serve_in_process()
            .await
            .expect("agent");
        registry.register(mine.link()).expect("first claim wins");

        // Same human, same laptop, second `horsie connect`. Before this gate
        // the newcomer displaced the incumbent, whose agent then re-dialled and
        // displaced the newcomer, forever, in silence.
        let second = FakeRuntimeVendor::builder("horsie-local")
            .owned_by(Principal::User(UserId::new("7")))
            .serve_in_process()
            .await
            .expect("agent");
        assert_eq!(
            registry.register(second.link()),
            Err(RegisterError::NameTaken {
                by: "user:7".to_string()
            })
        );

        // The incumbent is untouched, and it is still the link a session would
        // be routed to.
        assert_eq!(registry.connected_names(), vec!["horsie-local".to_string()]);
        let published = vendors
            .read()
            .unwrap()
            .get("horsie-local")
            .cloned()
            .unwrap();
        assert_eq!(published.instance_id(), mine.link().instance_id());
    }

    #[tokio::test]
    async fn the_refusal_handed_to_the_agent_does_not_name_the_holder() {
        // The server log says who holds it; the dialer is told only that it is
        // held. A refused stranger must not learn whose laptop this is.
        let reason = RegisterError::NameTaken {
            by: "user:7".to_string(),
        }
        .client_reason("horsie-local");
        assert!(reason.contains("horsie-local"), "{reason}");
        assert!(!reason.contains("user:7"), "{reason}");
    }

    #[tokio::test]
    async fn a_dead_link_does_not_hold_its_name_against_another_process() {
        let vendors = empty_vendors();
        let registry = RuntimeVendorRegistry::new(vendors.clone());

        let first = FakeRuntimeVendor::builder("same-name")
            .serve_in_process()
            .await
            .expect("agent");
        registry.register(first.link()).expect("first");
        first.disconnect();
        // Wait for the link to actually notice, so this asserts the corpse gate
        // rather than a race with it.
        first.link().closed().await;

        let second = FakeRuntimeVendor::builder("same-name")
            .serve_in_process()
            .await
            .expect("agent");
        assert_ne!(second.link().instance_id(), first.link().instance_id());
        registry
            .register(second.link())
            .expect("a corpse must not hold a name");
        assert_eq!(registry.connected_names(), vec!["same-name".to_string()]);
    }

    #[tokio::test]
    async fn publishing_releases_the_name_when_the_socket_dies() {
        let vendors = empty_vendors();
        let registry = Arc::new(RuntimeVendorRegistry::new(vendors.clone()));

        let agent = FakeRuntimeVendor::builder("my-laptop")
            .serve_in_process()
            .await
            .expect("agent");
        registry.publish(agent.link()).expect("publishes");
        assert_eq!(registry.connected_names(), vec!["my-laptop".to_string()]);

        agent.disconnect();
        agent.link().closed().await;
        // The eviction runs on its own task; yield until it has.
        for _ in 0..100 {
            if registry.connected_names().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            registry.connected_names().is_empty(),
            "a disconnected agent must not keep its name"
        );
    }

    #[tokio::test]
    async fn a_replaced_link_does_not_evict_its_successor() {
        let vendors = empty_vendors();
        let registry = Arc::new(RuntimeVendorRegistry::new(vendors.clone()));

        let first = FakeRuntimeVendor::builder("same-name")
            .serve_in_process()
            .await
            .expect("first");
        registry.publish(first.link()).expect("publishes");

        // The same process comes back on a new socket before the old one is
        // reaped. When the old socket finally reports in, its eviction must not
        // take the live link with it.
        let second = FakeRuntimeVendor::builder("same-name")
            .resuming(&first)
            .serve_in_process()
            .await
            .expect("second");
        registry.publish(second.link()).expect("reclaims");

        first.disconnect();
        first.link().closed().await;
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            registry.connected_names(),
            vec!["same-name".to_string()],
            "the live link must still be published"
        );
    }

    #[tokio::test]
    async fn the_same_process_reconnecting_still_replaces_its_own_entry() {
        let vendors = empty_vendors();
        let registry = RuntimeVendorRegistry::new(vendors.clone());

        let first = FakeRuntimeVendor::builder("same-name")
            .owned_by(Principal::User(UserId::new("7")))
            .serve_in_process()
            .await
            .expect("agent");
        registry.register(first.link()).expect("first");

        let second = FakeRuntimeVendor::builder("same-name")
            .owned_by(Principal::User(UserId::new("7")))
            .resuming(&first)
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
        for (name, who) in [("laptop-a", "1"), ("laptop-b", "2")] {
            let agent = FakeRuntimeVendor::builder(name)
                .owned_by(Principal::User(UserId::new(who)))
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
