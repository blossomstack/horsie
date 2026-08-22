//! DB-managed plugin-bundle library: install bundles from git, serve their zip
//! artifacts to runtimes, and resolve per-session selections at provisioning.
//! Mirrors the `github` module's store/service split and shares the config
//! store's SqlitePool. The runtime's plugin machinery (scan, hooks,
//! `horsie_shared`) is unchanged — this only manages bundles and delivers their
//! bytes to a plugins dir the runtime fetches into.

pub mod authored;
pub mod kind;
mod artifact;
pub(crate) mod ingest;
mod marketplace_store;
mod service;
mod store;

pub use artifact::ArtifactStore;
pub use marketplace_store::{MarketplaceRow, MarketplaceStore};
pub use service::PluginService;
pub use store::{PluginRow, PluginStore};

use horsie_models::runtime::BundleRef;

/// The subset of plugin operations the session layer needs at provisioning:
/// resolve selected bundle names to fetchable refs, and fall back to the
/// default-enabled set. Injected into `ServerDeps`.
#[async_trait::async_trait]
pub trait PluginProvisioner: Send + Sync {
    /// Resolve bundle `names` to the refs a runtime fetches by. Errs if any
    /// name is unknown.
    ///
    /// No base URL: the agent that runs the runtime knows the address its
    /// runtimes can reach the server at, and builds the fetch URL from the ref
    /// itself. The server has no opinion about it.
    async fn resolve(&self, names: &[String]) -> Result<Vec<BundleRef>, String>;

    /// Bundle names flagged `enabled_default` — used when a session selects none.
    async fn default_names(&self) -> Vec<String>;

    /// Everything the named bundles offer, merged. Empty `names` resolves to
    /// the default-enabled set, mirroring what provisioning does with the same
    /// input.
    ///
    /// A database read: the catalogue was derived when the bundle was
    /// installed, so answering "is `/commit` a command?" costs no runtime and
    /// no filesystem. That is the whole point — the seam runs on the way into
    /// every turn, and a prompt that merely starts with a slash must not pay
    /// for a scan.
    ///
    /// A name two bundles both declare goes to the first alphabetically; the
    /// loser is logged. The rule skills and agents already use.
    async fn catalog(&self, names: &[String])
    -> Vec<horsie_support::plugin::catalog::CatalogEntry>;
}

/// sha256 of some bytes, for tests that need to check a served package against
/// the digest its ref carried.
#[cfg(test)]
#[must_use]
pub fn sha256_hex_for_test(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}
