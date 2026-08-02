//! `.claude-plugin/marketplace.json` — an index of plugins. Entries mostly point
//! *outward* at other repos: of the 276 entries in `claude-plugins-public`, 223
//! are external. Four `source` shapes occur in the wild; all normalise to
//! [`PluginSource`].
//!
//! A malformed entry is skipped rather than failing the whole marketplace: one
//! bad row must not brick a 276-entry index.

use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Where a marketplace entry's plugin tree comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginSource {
    /// A path inside the marketplace repo itself.
    Path(String),
    /// Another git repo, optionally a subdirectory of it, optionally pinned.
    Git {
        url: String,
        path: Option<String>,
        git_ref: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct MarketplaceEntry {
    pub name: String,
    pub description: Option<String>,
    pub version: Option<String>,
    pub source: PluginSource,
}

#[derive(Debug, Clone)]
pub struct Marketplace {
    pub name: Option<String>,
    pub plugins: Vec<MarketplaceEntry>,
    /// Human-readable reasons for entries that could not be understood.
    pub skipped: Vec<String>,
}

#[derive(Deserialize)]
struct RawMarketplace {
    name: Option<String>,
    #[serde(default)]
    plugins: Vec<Value>,
}

impl Marketplace {
    /// `<repo_root>/.claude-plugin/marketplace.json`.
    pub fn path(repo_root: &Path) -> PathBuf {
        repo_root.join(".claude-plugin").join("marketplace.json")
    }

    /// `Ok(None)` when absent; `Err` when present but malformed at the top level.
    pub fn read(repo_root: &Path) -> Result<Option<Marketplace>, String> {
        let path = Self::path(repo_root);
        if !path.is_file() {
            return Ok(None);
        }
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let raw: RawMarketplace =
            serde_json::from_str(&text).map_err(|e| format!("marketplace.json: {e}"))?;

        let mut plugins = Vec::new();
        let mut skipped = Vec::new();
        for (i, entry) in raw.plugins.iter().enumerate() {
            match parse_entry(entry) {
                Ok(e) => plugins.push(e),
                Err(why) => skipped.push(format!("entry {i}: {why}")),
            }
        }
        Ok(Some(Marketplace {
            name: raw.name,
            plugins,
            skipped,
        }))
    }

    pub fn find(&self, name: &str) -> Option<&MarketplaceEntry> {
        self.plugins.iter().find(|p| p.name == name)
    }

    pub fn names(&self) -> Vec<&str> {
        self.plugins.iter().map(|p| p.name.as_str()).collect()
    }
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn parse_entry(v: &Value) -> Result<MarketplaceEntry, String> {
    let name = str_field(v, "name").ok_or("missing 'name'")?;
    let source = v.get("source").ok_or("missing 'source'")?;
    let source = parse_source(source)?;
    Ok(MarketplaceEntry {
        name,
        description: str_field(v, "description"),
        version: str_field(v, "version"),
        source,
    })
}

/// Normalise the four `source` shapes seen in the wild.
///
/// `sha` is deliberately not read: it is an integrity digest over a packaging
/// horsie does not reproduce, so honouring it would claim a verification we do
/// not perform. `ref`/`commit` carry the pinning.
fn parse_source(v: &Value) -> Result<PluginSource, String> {
    if let Some(path) = v.as_str() {
        if path.is_empty() {
            return Err("empty path source".to_string());
        }
        return Ok(PluginSource::Path(path.to_string()));
    }
    let kind = str_field(v, "source").ok_or("source object missing 'source' kind")?;
    match kind.as_str() {
        "git-subdir" | "url" | "git" => {
            let url = str_field(v, "url").ok_or("git source missing 'url'")?;
            Ok(PluginSource::Git {
                url,
                path: str_field(v, "path"),
                git_ref: str_field(v, "ref"),
            })
        }
        "github" => {
            let repo = str_field(v, "repo").ok_or("github source missing 'repo'")?;
            Ok(PluginSource::Git {
                url: format!("https://github.com/{repo}.git"),
                path: str_field(v, "path"),
                git_ref: str_field(v, "commit").or_else(|| str_field(v, "ref")),
            })
        }
        other => Err(format!("unsupported source kind '{other}'")),
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
    use tempfile::TempDir;

    fn write_marketplace(root: &Path, json: &str) {
        let dir = root.join(".claude-plugin");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marketplace.json"), json).unwrap();
    }

    #[test]
    fn absent_marketplace_is_ok_none() {
        let dir = TempDir::new().unwrap();
        assert!(Marketplace::read(dir.path()).unwrap().is_none());
    }

    #[test]
    fn relative_path_source() {
        let dir = TempDir::new().unwrap();
        write_marketplace(
            dir.path(),
            r#"{"name":"impeccable","plugins":[
                 {"name":"impeccable","description":"d","version":"4.0.4","source":"./plugin"}]}"#,
        );
        let m = Marketplace::read(dir.path()).unwrap().unwrap();
        assert_eq!(m.name.as_deref(), Some("impeccable"));
        assert_eq!(m.plugins.len(), 1);
        let e = &m.plugins[0];
        assert_eq!(e.name, "impeccable");
        assert_eq!(e.version.as_deref(), Some("4.0.4"));
        assert_eq!(e.source, PluginSource::Path("./plugin".into()));
    }

    #[test]
    fn git_subdir_source() {
        let dir = TempDir::new().unwrap();
        write_marketplace(
            dir.path(),
            r#"{"plugins":[{"name":"p","source":{"source":"git-subdir",
                 "url":"https://github.com/o/r.git","path":"plugins/p","ref":"v1.5.5",
                 "sha":"deadbeef"}}]}"#,
        );
        let m = Marketplace::read(dir.path()).unwrap().unwrap();
        assert_eq!(
            m.plugins[0].source,
            PluginSource::Git {
                url: "https://github.com/o/r.git".into(),
                path: Some("plugins/p".into()),
                git_ref: Some("v1.5.5".into()),
            }
        );
    }

    #[test]
    fn url_source_with_and_without_path() {
        let dir = TempDir::new().unwrap();
        write_marketplace(
            dir.path(),
            r#"{"plugins":[
                 {"name":"a","source":{"source":"url","url":"https://x/a.git","sha":"s"}},
                 {"name":"b","source":{"source":"url","url":"https://x/b.git","path":"sub/b"}}]}"#,
        );
        let m = Marketplace::read(dir.path()).unwrap().unwrap();
        assert_eq!(
            m.plugins[0].source,
            PluginSource::Git {
                url: "https://x/a.git".into(),
                path: None,
                git_ref: None,
            }
        );
        assert_eq!(
            m.plugins[1].source,
            PluginSource::Git {
                url: "https://x/b.git".into(),
                path: Some("sub/b".into()),
                git_ref: None,
            }
        );
    }

    #[test]
    fn github_source_expands_to_a_url_and_pins_the_commit() {
        let dir = TempDir::new().unwrap();
        write_marketplace(
            dir.path(),
            r#"{"plugins":[{"name":"p","source":{"source":"github",
                 "repo":"fullstorydev/fullstory-skills","commit":"1ec5865"}}]}"#,
        );
        let m = Marketplace::read(dir.path()).unwrap().unwrap();
        assert_eq!(
            m.plugins[0].source,
            PluginSource::Git {
                url: "https://github.com/fullstorydev/fullstory-skills.git".into(),
                path: None,
                git_ref: Some("1ec5865".into()),
            }
        );
    }

    #[test]
    fn malformed_entry_is_skipped_not_fatal() {
        let dir = TempDir::new().unwrap();
        write_marketplace(
            dir.path(),
            r#"{"plugins":[
                 {"name":"good","source":"./a"},
                 {"name":"nosource"},
                 {"source":"./noname"},
                 {"name":"badkind","source":{"source":"carrier-pigeon"}}]}"#,
        );
        let m = Marketplace::read(dir.path()).unwrap().unwrap();
        assert_eq!(m.names(), vec!["good"]);
        assert_eq!(m.skipped.len(), 3, "skipped: {:?}", m.skipped);
    }

    #[test]
    fn find_and_names() {
        let dir = TempDir::new().unwrap();
        write_marketplace(
            dir.path(),
            r#"{"plugins":[{"name":"a","source":"./a"},{"name":"b","source":"./b"}]}"#,
        );
        let m = Marketplace::read(dir.path()).unwrap().unwrap();
        assert_eq!(m.names(), vec!["a", "b"]);
        assert!(m.find("a").is_some());
        assert!(m.find("zzz").is_none());
    }

    #[test]
    fn malformed_json_is_err() {
        let dir = TempDir::new().unwrap();
        write_marketplace(dir.path(), "{nope");
        assert!(Marketplace::read(dir.path()).is_err());
    }
}
