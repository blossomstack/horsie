//! Reading Claude Code plugin packaging: `.claude-plugin/plugin.json`,
//! `.claude-plugin/marketplace.json`, and the skills and agents they point at.
//!
//! horsie reads this format and never writes it. See
//! `docs/superpowers/specs/2026-08-02-plugin-marketplace-design.md`.

pub mod agents;
pub mod builtins;
pub mod catalog;
#[cfg(feature = "git")]
pub mod checkout;
pub mod commands;
pub mod grants;
pub mod hooks;
pub mod layout;
pub mod manifest;
pub mod marketplace;
pub mod mcp;
pub mod skills;

#[cfg(feature = "git")]
pub use checkout::{Checkout, ensure_checkout, source_location};
pub use layout::PluginRoot;
pub use manifest::{ManifestDialect, PluginManifest};
pub use marketplace::{Marketplace, MarketplaceEntry, PluginSource};

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Join a manifest- or marketplace-declared relative path onto a root, dropping
/// the `./` prefix these fields conventionally carry.
///
/// `Path::join` keeps the `.` component verbatim, which is harmless to resolve
/// but leaks into anything that displays the result — the library's symlink
/// targets read as `…/sources/<key>/./plugin` without this.
pub fn join_declared(root: &Path, declared: &str) -> PathBuf {
    let mut rest = declared;
    while let Some(stripped) = rest.strip_prefix("./") {
        rest = stripped;
    }
    if rest.is_empty() || rest == "." {
        return root.to_path_buf();
    }
    root.join(rest)
}

/// The placeholder a plugin uses to name its own directory.
///
/// A plugin cannot know where it will be installed, so every path it ships —
/// a hook command, an MCP server's script, a resource a skill or agent points at
/// — is written against this. One constant, so the four places that resolve it
/// cannot disagree about its spelling.
pub const PLUGIN_ROOT_VAR: &str = "${CLAUDE_PLUGIN_ROOT}";

/// Resolve [`PLUGIN_ROOT_VAR`] against the directory a plugin is installed at.
#[must_use]
pub fn expand_plugin_root(text: &str, plugin_root: &Path) -> String {
    text.replace(PLUGIN_ROOT_VAR, &plugin_root.to_string_lossy())
}

/// Stable short key for a checkout of `(url, git_ref)`, used to name the shared
/// clone under `<data_dir>/sources/`. Keyed by source rather than by plugin name
/// so a marketplace declaring several plugins as paths into its own repo clones
/// once.
pub fn source_key(url: &str, git_ref: Option<&str>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(url.trim().as_bytes());
    hasher.update(b"\n");
    hasher.update(git_ref.unwrap_or("").as_bytes());
    hex::encode(hasher.finalize())[..16].to_string()
}
