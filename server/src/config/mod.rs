//! The session server's runtime-editable configuration: providers, models,
//! vendors, and the default vendor, stored in a database and served over
//! `GET`/`PUT /api/config`. This is the app config the Settings UI owns —
//! distinct from, and never synced with, the deployment/bootstrap config the
//! host reads from `config.json`/env.

mod store;

use async_trait::async_trait;
use horsie_models::settings::{SettingsUpdate, SettingsView, VendorTestResult};

pub use store::{DbConfigStore, OpenedConfig, StoreDeps};

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

    /// The vendor a create request defaults to when it omits one. Read on the
    /// hot path, so it stays synchronous and cheap.
    fn default_vendor(&self) -> String;

    /// An on-demand connection check for a configurable vendor (currently
    /// velos only): is it reachable, and does its stored token still work.
    /// Read-only — never mutates `active`/`error`/persisted state. Errs only
    /// when `name` doesn't refer to a testable vendor.
    async fn test_vendor(&self, name: &str) -> Result<VendorTestResult, String>;
}
