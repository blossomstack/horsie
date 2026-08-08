//! The session server's runtime-editable configuration: providers, models,
//! vendors, and the default vendor, stored in a database and served over
//! `GET`/`PUT /api/config`. This is the app config the Settings UI owns —
//! distinct from, and never synced with, the deployment/bootstrap config the
//! host reads from `config.json`/env.

pub(crate) mod store;

pub mod chatgpt_login;
pub mod model_cards;

use async_trait::async_trait;
use horsie_models::settings::{
    ModelInput, ModelView, ProviderInput, ProviderView, SettingsUpdate, SettingsView,
};

pub use store::{DEFAULT_MAX_CONNECTIONS, DbConfigStore, OpenedConfig, StoreDeps};

/// Read + mutate the runtime-editable configuration, redacting secrets.
#[async_trait]
pub trait ConfigStore: Send + Sync {
    /// A redacted snapshot of the current settings, or an error if the backing
    /// store can't be read.
    async fn view(&self) -> Result<SettingsView, String>;

    /// Validate, persist, and live-apply an update. Returns the new view, or a
    /// human-readable error when the update is rejected (nothing is persisted
    /// or applied on error).
    async fn update(&self, update: SettingsUpdate) -> Result<SettingsView, String>;

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
