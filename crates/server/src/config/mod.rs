//! The session server's runtime-editable configuration: providers, models,
//! vendors, and the default vendor, stored in a database. Read whole over
//! `GET /api/config`, mutated one resource at a time under `/api/config/*`.
//! This is the app config the Settings UI owns —
//! distinct from, and never synced with, the deployment/bootstrap config the
//! host reads from `config.json`/env.

pub(crate) mod store;

pub mod chatgpt_login;
pub mod model_cards;

use async_trait::async_trait;
use horsie_models::settings::{ModelInput, ModelView, ProviderInput, ProviderView, SettingsView};

pub use store::{DEFAULT_MAX_CONNECTIONS, DbConfigStore, OpenedConfig, StoreDeps, dial_secret_of};

/// Read + mutate the runtime-editable configuration, redacting secrets.
#[async_trait]
pub trait ConfigStore: Send + Sync {
    /// A redacted snapshot of the current settings, or an error if the backing
    /// store can't be read.
    async fn view(&self) -> Result<SettingsView, String>;

    /// Set the vendor new sessions default to, persisting and live-applying it.
    async fn set_default_vendor(&self, vendor: &str) -> Result<SettingsView, String>;

    /// Forget the default-vendor preference, falling back to the built-in
    /// `local`. Distinct from setting it to an empty string, which is refused.
    async fn clear_default_vendor(&self) -> Result<SettingsView, String>;

    /// Rebuild the live provider registry from what is stored.
    ///
    /// For credentials that arrive outside this store — a ChatGPT sign-in is
    /// the only one today. The provider was unbuildable a moment ago and
    /// nothing about providers or models changed, so there is nothing to
    /// persist, only to re-derive.
    async fn rebuild_registry(&self) -> Result<(), String>;

    /// Create or replace one model provider, leaving every other row alone.
    ///
    /// `api_key` follows `ProviderInput`'s rule: omitted keeps the stored key,
    /// `""` clears it. A `chatgpt` provider stores no key at all.
    async fn upsert_provider(&self, input: ProviderInput) -> Result<ProviderView, String>;

    /// Remove one model provider. Rejected while any model still routes to it,
    /// because a dangling `models.provider` is what the registry refuses to
    /// build from.
    async fn delete_provider(&self, name: &str) -> Result<(), String>;

    /// Create or replace one model alias, leaving every other row alone.
    async fn upsert_model(&self, input: ModelInput) -> Result<ModelView, String>;

    /// Remove one model alias.
    async fn delete_model(&self, alias: &str) -> Result<(), String>;

    /// The vendor a create request defaults to when it omits one. Read on the
    /// hot path, so it stays synchronous and cheap.
    fn default_vendor(&self) -> String;
}
