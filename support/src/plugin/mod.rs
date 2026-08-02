//! Reading Claude Code plugin packaging: `.claude-plugin/plugin.json`,
//! `.claude-plugin/marketplace.json`, and the skills they point at.
//!
//! horsie reads this format and never writes it. See
//! `docs/superpowers/specs/2026-08-02-plugin-marketplace-design.md`.

pub mod manifest;

pub use manifest::PluginManifest;

use sha2::{Digest, Sha256};

/// Stable short key for a checkout of `(url, git_ref)`, used to name the shared
/// clone under `<data_dir>/sources/`. Keyed by source rather than by plugin name
/// so a marketplace declaring several plugins as paths into its own repo clones
/// once.
pub fn source_key(url: &str, git_ref: Option<&str>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(url.trim().as_bytes());
    hasher.update(b"\n");
    hasher.update(git_ref.unwrap_or("").as_bytes());
    format!("{:x}", hasher.finalize())[..16].to_string()
}
