//! Reading and writing [`PluginKind`] as table columns.
//!
//! The union is the model; the `plugins` row is three nullable column groups
//! and a discriminant. These are the only two places that translation happens,
//! so a fourth kind means adding an arm here and nowhere else.

use horsie_models::plugins::{AuthoredOrigin, ExternalOrigin, PluginKind};

/// The `source_kind` discriminant. Stable strings — they are on disk.
pub const CLAUDE: &str = "claude";
pub const AGENT_PLUGIN: &str = "agent_plugin";
pub const AUTHORED: &str = "authored";

#[must_use]
pub fn tag(kind: &PluginKind) -> &'static str {
    match kind {
        PluginKind::Claude(_) => CLAUDE,
        PluginKind::AgentPlugin(_) => AGENT_PLUGIN,
        PluginKind::Authored(_) => AUTHORED,
    }
}

/// The clone this bundle came from, when it came from one.
#[must_use]
pub fn external(kind: &PluginKind) -> Option<&ExternalOrigin> {
    match kind {
        PluginKind::Claude(e) | PluginKind::AgentPlugin(e) => Some(e),
        PluginKind::Authored(_) => None,
    }
}

/// The generation an authored bundle is at.
#[must_use]
pub fn generation(kind: &PluginKind) -> Option<u64> {
    match kind {
        PluginKind::Authored(a) => Some(a.generation),
        PluginKind::Claude(_) | PluginKind::AgentPlugin(_) => None,
    }
}

#[must_use]
pub fn is_authored(kind: &PluginKind) -> bool {
    matches!(kind, PluginKind::Authored(_))
}

/// The dialect an external bundle's tree follows, so a re-clone can be
/// classified the same way the first one was.
#[must_use]
pub fn from_dialect(
    dialect: horsie_support::plugin::ManifestDialect,
    origin: ExternalOrigin,
) -> PluginKind {
    match dialect {
        horsie_support::plugin::ManifestDialect::Claude => PluginKind::Claude(origin),
        horsie_support::plugin::ManifestDialect::AgentPlugin => PluginKind::AgentPlugin(origin),
    }
}

/// Rebuild the union from the columns. An unknown discriminant is an error
/// rather than a default: a row horsie cannot classify is one it must not
/// silently treat as a clone and try to re-fetch.
pub fn from_columns(
    source_kind: &str,
    url: Option<String>,
    git_ref: Option<String>,
    subpath: Option<String>,
    marketplace: Option<String>,
    marketplace_entry: Option<String>,
    generation: Option<u64>,
) -> Result<PluginKind, String> {
    let origin = || ExternalOrigin {
        // A row whose kind says "cloned" but whose URL is NULL is corrupt, not
        // a new case. Empty is the honest rendering of it, and `update` will
        // fail loudly on it rather than cloning something surprising.
        url: url.clone().unwrap_or_default(),
        git_ref: git_ref.clone(),
        subpath: subpath.clone(),
        marketplace: marketplace.clone(),
        marketplace_entry: marketplace_entry.clone(),
    };
    match source_kind {
        CLAUDE => Ok(PluginKind::Claude(origin())),
        AGENT_PLUGIN => Ok(PluginKind::AgentPlugin(origin())),
        AUTHORED => Ok(PluginKind::Authored(AuthoredOrigin {
            generation: generation.unwrap_or_default(),
        })),
        other => Err(format!("unknown plugin source_kind '{other}'")),
    }
}
