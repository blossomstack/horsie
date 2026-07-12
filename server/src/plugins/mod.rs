//! DB-managed plugin-bundle library: install bundles from git, serve their zip
//! artifacts to runtimes, and resolve per-session selections at provisioning.
//! Mirrors the `github` module's store/service split and shares the config
//! store's SqlitePool. The runtime's plugin machinery (scan, hooks,
//! `horsie_shared`) is unchanged — this only manages bundles and delivers their
//! bytes to a plugins dir the runtime fetches into.

mod artifact;
mod ingest;
mod service;
mod store;
mod token;

pub use artifact::ArtifactStore;
pub use service::PluginService;
pub use store::{PluginRow, PluginStore};

use serde::Serialize;

/// A resolved bundle the runtime should fetch: canonical name, content hash,
/// and the authed URL to GET its zip from. Serialized into the runtime env as
/// the `HORSIE_PLUGIN_MANIFEST` JSON array.
#[derive(Clone, Debug, Serialize)]
pub struct PluginArtifactRef {
    pub name: String,
    pub hash: String,
    pub url: String,
}

/// The subset of plugin operations the session layer needs at provisioning:
/// resolve selected bundle names to fetchable refs, mint a capability token,
/// and fall back to the default-enabled set. Injected into `ServerDeps`.
#[async_trait::async_trait]
pub trait PluginProvisioner: Send + Sync {
    /// Resolve bundle `names` to `{name, hash, url}` refs against `base_url`
    /// (the vendor's artifact base). Errs if any name is unknown.
    async fn resolve(
        &self,
        names: &[String],
        base_url: &str,
    ) -> Result<Vec<PluginArtifactRef>, String>;

    /// Mint a short-lived bearer scoped to a session id and the given hashes.
    fn mint_token(&self, session_id: &str, hashes: &[String]) -> String;

    /// Bundle names flagged `enabled_default` — used when a session selects none.
    async fn default_names(&self) -> Vec<String>;
}
