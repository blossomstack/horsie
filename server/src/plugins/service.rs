//! `PluginService` ties the store + artifact store + token together: install
//! bundles from git, serve their artifacts, and resolve per-session selections.
//! Injected into `AppState` (CRUD routes) and `ServerDeps` (as a
//! `PluginProvisioner`, for `ensure_runtime`).

use super::artifact::ArtifactStore;
use super::ingest::{self, IngestTarget, Ingested, ParsedMarketplace, PluginBundle};
use super::marketplace_store::{MarketplaceRow, MarketplaceStore};
use super::store::{PluginRow, PluginStore};
use super::token;
use super::{PluginArtifactRef, PluginProvisioner};
use horsie_models::plugins::{
    InstallOutcome, MarketplacePluginView, MarketplaceView, PluginDefaultInput, PluginInstallInput,
    PluginView,
};
use horsie_support::plugin::source_location;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Capability-token lifetime; covers provisioning (incl. re-attach) with margin.
const TOKEN_TTL_SECS: u64 = 3600;

/// Where a bundle came from, for the `plugins` row. Both halves or neither: a
/// bundle either came through a catalogue or did not.
enum Provenance {
    Direct,
    FromMarketplace { name: String, entry: String },
}

pub struct PluginService {
    store: PluginStore,
    marketplaces: MarketplaceStore,
    /// Shared with every other account's service: artifacts are addressed by
    /// the hash of their own bytes, so there is one file per bundle version on
    /// the whole deployment.
    artifacts: Arc<ArtifactStore>,
    token_secret: Arc<Vec<u8>>,
}

impl PluginService {
    pub fn new(
        store: PluginStore,
        marketplaces: MarketplaceStore,
        artifacts: Arc<ArtifactStore>,
        token_secret: Arc<Vec<u8>>,
    ) -> Self {
        Self {
            store,
            marketplaces,
            artifacts,
            token_secret,
        }
    }

    pub async fn list(&self) -> Result<Vec<PluginView>, String> {
        Ok(self
            .store
            .list()
            .await?
            .into_iter()
            .map(row_to_view)
            .collect())
    }

    /// Install a bundle, or register the catalogue a URL turned out to be.
    ///
    /// One box: the caller does not classify the URL first, because the server
    /// has to clone the repo to find out either way.
    pub async fn install(&self, input: PluginInstallInput) -> Result<InstallOutcome, String> {
        let url = input
            .source_url
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let pair = match (input.marketplace.as_ref(), input.plugin_name.as_ref()) {
            (Some(m), Some(p)) => Some((m.clone(), p.clone())),
            (None, None) => None,
            _ => return Err("marketplace and plugin_name must be given together".to_string()),
        };
        match (url, pair) {
            (Some(_), Some(_)) => {
                Err("give either source_url or (marketplace, plugin_name), not both".to_string())
            }
            (None, None) => {
                Err("source_url, or a (marketplace, plugin_name) pair, is required".to_string())
            }
            (Some(url), None) => self.install_url(url, input.source_ref).await,
            (None, Some((market, plugin))) => self.install_entry(&market, &plugin).await,
        }
    }

    /// A pasted URL: clone once, and let what is there decide the outcome.
    async fn install_url(
        &self,
        url: String,
        git_ref: Option<String>,
    ) -> Result<InstallOutcome, String> {
        let target = IngestTarget::Url { url, git_ref };
        match blocking_ingest(target).await? {
            Ingested::Plugin(bundle) => Ok(InstallOutcome::Installed(
                self.persist(bundle, Provenance::Direct).await?,
            )),
            Ingested::Marketplace(parsed) => {
                Ok(InstallOutcome::Marketplace(self.record(parsed).await?))
            }
        }
    }

    /// A pick from a registered catalogue, resolved against the *cached* index —
    /// browsing and installing must not each pay for a clone of the marketplace.
    async fn install_entry(&self, market: &str, plugin: &str) -> Result<InstallOutcome, String> {
        let row = self
            .marketplaces
            .get(market)
            .await?
            .ok_or_else(|| format!("no such marketplace '{market}'"))?;
        let entry = row
            .entries
            .iter()
            .find(|e| e.name == plugin)
            .ok_or_else(|| {
                let names: Vec<&str> = row.entries.iter().map(|e| e.name.as_str()).collect();
                format!(
                    "marketplace '{market}' has no plugin '{plugin}'. Available: {}",
                    names.join(", ")
                )
            })?;
        let (url, git_ref, subpath) =
            source_location(&entry.source, &row.source_url, row.source_ref.as_deref());
        let entry_name = entry.name.clone();
        let bundle = match blocking_ingest(IngestTarget::Resolved {
            url,
            git_ref,
            subpath,
            name_hint: Some(entry_name.clone()),
        })
        .await?
        {
            Ingested::Plugin(b) => b,
            // Unreachable by construction: `Resolved` never classifies.
            Ingested::Marketplace(m) => {
                return Err(format!("'{}' resolved to a marketplace", m.url));
            }
        };
        let view = self
            .persist(
                bundle,
                Provenance::FromMarketplace {
                    name: market.to_string(),
                    entry: entry_name,
                },
            )
            .await?;
        Ok(InstallOutcome::Installed(view))
    }

    /// Re-clone a bundle. One installed through a marketplace re-resolves
    /// through the cached index first, so a catalogue that has since moved or
    /// re-pinned an entry is followed.
    pub async fn update(&self, name: &str) -> Result<PluginView, String> {
        let existing = self
            .store
            .get(name)
            .await?
            .ok_or_else(|| format!("no such bundle '{name}'"))?;
        let outcome = match (&existing.marketplace, &existing.marketplace_entry) {
            (Some(market), Some(entry)) => self.install_entry(market, entry).await?,
            _ => {
                let target = IngestTarget::Resolved {
                    url: existing.source_url.clone(),
                    git_ref: existing.source_ref.clone(),
                    subpath: existing.source_subpath.clone(),
                    name_hint: Some(existing.name.clone()),
                };
                match blocking_ingest(target).await? {
                    Ingested::Plugin(b) => {
                        InstallOutcome::Installed(self.persist(b, Provenance::Direct).await?)
                    }
                    Ingested::Marketplace(m) => {
                        return Err(format!("'{}' resolved to a marketplace", m.url));
                    }
                }
            }
        };
        match outcome {
            InstallOutcome::Installed(v) => Ok(v),
            InstallOutcome::Marketplace(m) => {
                Err(format!("'{}' resolved to a marketplace", m.source_url))
            }
        }
    }

    pub async fn list_marketplaces(&self) -> Result<Vec<MarketplaceView>, String> {
        let mut out = Vec::new();
        for row in self.marketplaces.list().await? {
            out.push(self.marketplace_view(row).await?);
        }
        Ok(out)
    }

    /// Re-clone and re-parse. Deliberately `read_marketplace` rather than
    /// `ingest_git`: a catalogue that has dropped to one entry is still a
    /// catalogue, and a refresh must not turn into an install.
    pub async fn refresh_marketplace(&self, name: &str) -> Result<MarketplaceView, String> {
        let row = self
            .marketplaces
            .get(name)
            .await?
            .ok_or_else(|| format!("no such marketplace '{name}'"))?;
        let url = row.source_url.clone();
        let git_ref = row.source_ref.clone();
        let parsed =
            tokio::task::spawn_blocking(move || ingest::read_marketplace(&url, git_ref.as_deref()))
                .await
                .map_err(|e| e.to_string())??;
        // The row keeps the name it was registered under: it is the primary key,
        // and installed bundles already record it as their provenance.
        let updated = MarketplaceRow {
            name: row.name,
            source_url: row.source_url,
            source_ref: row.source_ref,
            sha: parsed.sha,
            entries: parsed.entries,
            skipped: parsed.skipped,
            created_at: row.created_at,
            updated_at: now_string(),
        };
        self.marketplaces.upsert(&updated).await?;
        self.marketplace_view(updated).await
    }

    /// Drop the source. Bundles installed from it stay: dropping a source is not
    /// dropping the software, which is what `horsie marketplace remove` does too.
    pub async fn remove_marketplace(&self, name: &str) -> Result<(), String> {
        self.marketplaces.delete(name).await
    }

    /// Register a freshly-parsed catalogue, or refresh the row already holding
    /// its name.
    async fn record(&self, parsed: ParsedMarketplace) -> Result<MarketplaceView, String> {
        let existing = self.marketplaces.get(&parsed.name).await?;
        if let Some(prev) = existing.as_ref().filter(|p| p.source_url != parsed.url) {
            return Err(format!(
                "a marketplace named '{}' is already registered from {}",
                parsed.name, prev.source_url
            ));
        }
        let now = now_string();
        let row = MarketplaceRow {
            name: parsed.name,
            source_url: parsed.url,
            source_ref: parsed.git_ref,
            sha: parsed.sha,
            entries: parsed.entries,
            skipped: parsed.skipped,
            created_at: existing.map_or_else(|| now.clone(), |p| p.created_at),
            updated_at: now,
        };
        self.marketplaces.upsert(&row).await?;
        self.marketplace_view(row).await
    }

    async fn marketplace_view(&self, row: MarketplaceRow) -> Result<MarketplaceView, String> {
        let installed = self.store.installed_entries(&row.name).await?;
        Ok(MarketplaceView {
            plugin_count: u32::try_from(row.entries.len()).unwrap_or(u32::MAX),
            plugins: row
                .entries
                .iter()
                .map(|e| MarketplacePluginView {
                    name: e.name.clone(),
                    description: e.description.clone(),
                    version: e.version.clone(),
                    installed: installed.contains(&e.name),
                })
                .collect(),
            name: row.name,
            source_url: row.source_url,
            source_ref: row.source_ref,
            updated_at: row.updated_at,
            skipped: row.skipped,
        })
    }

    pub async fn set_default(
        &self,
        name: &str,
        input: PluginDefaultInput,
    ) -> Result<PluginView, String> {
        self.store
            .get(name)
            .await?
            .ok_or_else(|| format!("no such bundle '{name}'"))?;
        self.store.set_default(name, input.enabled_default).await?;
        let row = self
            .store
            .get(name)
            .await?
            .ok_or_else(|| "bundle missing after update".to_string())?;
        Ok(row_to_view(row))
    }

    pub async fn remove(&self, name: &str) -> Result<(), String> {
        self.store.delete(name).await?;
        self.gc().await
    }

    /// Write the artifact and the row. The source recorded is what ingest
    /// actually cloned, not what the caller asked for — a marketplace entry may
    /// name another repo, and `update` has to re-clone the same tree.
    async fn persist(
        &self,
        bundle: PluginBundle,
        provenance: Provenance,
    ) -> Result<PluginView, String> {
        let existing = self.store.get(&bundle.name).await?;
        self.artifacts
            .write(&bundle.hash, &bundle.zip_bytes)
            .map_err(|e| e.to_string())?;
        let (marketplace, marketplace_entry) = match provenance {
            Provenance::Direct => (None, None),
            Provenance::FromMarketplace { name, entry } => (Some(name), Some(entry)),
        };
        let now = now_string();
        let row = PluginRow {
            name: bundle.name,
            source_kind: "git".to_string(),
            source_url: bundle.url,
            source_ref: bundle.git_ref,
            source_subpath: bundle.subpath,
            version: bundle.version,
            description: bundle.description,
            skill_count: bundle.skill_count,
            has_hooks: bundle.has_hooks,
            artifact_hash: bundle.hash,
            artifact_size: bundle.zip_bytes.len() as u64,
            enabled_default: existing.as_ref().is_some_and(|e| e.enabled_default),
            marketplace,
            marketplace_entry,
            created_at: existing
                .as_ref()
                .map(|e| e.created_at.clone())
                .unwrap_or_else(|| now.clone()),
            updated_at: now,
        };
        self.store.upsert(&row).await?;
        self.gc().await?;
        Ok(row_to_view(row))
    }

    async fn gc(&self) -> Result<(), String> {
        let keep = self.store.referenced_hashes().await?;
        self.artifacts.gc(&keep).map_err(|e| e.to_string())
    }
}

#[async_trait::async_trait]
impl PluginProvisioner for PluginService {
    async fn resolve(&self, names: &[String]) -> Result<Vec<PluginArtifactRef>, String> {
        let mut refs = Vec::with_capacity(names.len());
        for name in names {
            let row = self
                .store
                .get(name)
                .await?
                .ok_or_else(|| format!("no such bundle '{name}'"))?;
            refs.push(PluginArtifactRef {
                name: row.name,
                hash: row.artifact_hash,
            });
        }
        Ok(refs)
    }

    fn mint_token(&self, session_id: &str, hashes: &[String]) -> String {
        token::sign(&self.token_secret, session_id, hashes, TOKEN_TTL_SECS)
    }

    async fn default_names(&self) -> Vec<String> {
        self.store
            .list()
            .await
            .map(|rows| {
                rows.into_iter()
                    .filter(|r| r.enabled_default)
                    .map(|r| r.name)
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Run the blocking clone + pack off the async runtime, warning about hooks
/// horsie cannot fire — a bundle whose skills are fine installs anyway, and
/// saying nothing would leave a guard that silently never runs, which is what
/// classifying events rather than ignoring them is for.
async fn blocking_ingest(target: IngestTarget) -> Result<Ingested, String> {
    let ingested = tokio::task::spawn_blocking(move || ingest::ingest_git(&target))
        .await
        .map_err(|e| e.to_string())??;
    if let Ingested::Plugin(b) = &ingested {
        for reason in &b.unsupported_hooks {
            tracing::warn!(
                plugin = b.name,
                reason,
                "plugin declares a hook horsie cannot run"
            );
        }
    }
    Ok(ingested)
}

fn row_to_view(row: PluginRow) -> PluginView {
    PluginView {
        name: row.name,
        description: row.description,
        version: row.version,
        source_url: row.source_url,
        source_ref: row.source_ref,
        skill_count: row.skill_count,
        has_hooks: row.has_hooks,
        enabled_default: row.enabled_default,
        artifact_size: row.artifact_size,
        marketplace: row.marketplace,
    }
}

fn now_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::path::Path;

    async fn service() -> (PluginService, Arc<ArtifactStore>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let db = crate::db::testing::db().await;
        let artifacts = Arc::new(ArtifactStore::new(tmp.path().join("artifacts")));
        let svc = PluginService::new(
            PluginStore::new(db.clone(), crate::auth::UserId::new("1")),
            MarketplaceStore::new(db, crate::auth::UserId::new("1")),
            artifacts.clone(),
            Arc::new(b"secret".to_vec()),
        );
        (svc, artifacts, tmp)
    }

    fn url_input(url: &str) -> PluginInstallInput {
        PluginInstallInput {
            source_url: Some(url.to_string()),
            source_ref: None,
            marketplace: None,
            plugin_name: None,
        }
    }

    fn pick(marketplace: &str, plugin: &str) -> PluginInstallInput {
        PluginInstallInput {
            source_url: None,
            source_ref: None,
            marketplace: Some(marketplace.to_string()),
            plugin_name: Some(plugin.to_string()),
        }
    }

    fn write_marketplace(root: &Path, json: &str) {
        let dir = root.join(".claude-plugin");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marketplace.json"), json).unwrap();
    }

    fn write_skill(dir: &Path, name: &str) {
        let s = dir.join("skills").join(name);
        std::fs::create_dir_all(&s).unwrap();
        std::fs::write(s.join("SKILL.md"), format!("---\nname: {name}\n---\nbody")).unwrap();
    }

    fn commit_repo(root: &Path) -> String {
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "t@t"]);
        git(root, &["config", "user.name", "t"]);
        git(root, &["add", "-A"]);
        git(root, &["commit", "-q", "-m", "init"]);
        format!("file://{}", root.display())
    }

    /// A two-entry catalogue at `dir`, returned as a `file://` URL.
    fn catalogue(tmp: &Path, dir: &str) -> String {
        let repo = tmp.join(dir);
        std::fs::create_dir_all(&repo).unwrap();
        write_marketplace(
            &repo,
            r#"{"name":"catalogue","plugins":[
                 {"name":"alpha","source":"./plugins/alpha"},
                 {"name":"beta","source":"./plugins/beta"}]}"#,
        );
        write_skill(&repo.join("plugins/alpha"), "a");
        write_skill(&repo.join("plugins/beta"), "b");
        commit_repo(&repo)
    }

    fn expect_installed(out: InstallOutcome) -> PluginView {
        match out {
            InstallOutcome::Installed(v) => v,
            InstallOutcome::Marketplace(m) => panic!("expected an install, got source {}", m.name),
        }
    }

    fn expect_source(out: InstallOutcome) -> MarketplaceView {
        match out {
            InstallOutcome::Marketplace(m) => m,
            InstallOutcome::Installed(v) => panic!("expected a source, got install {}", v.name),
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn fixture_repo(root: &Path) -> String {
        let cp = root.join(".claude-plugin");
        std::fs::create_dir_all(&cp).unwrap();
        std::fs::write(
            cp.join("plugin.json"),
            r#"{"name":"demo","version":"1.0.0"}"#,
        )
        .unwrap();
        let d = root.join("skills").join("a");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("SKILL.md"), "---\nname: a\n---\nx").unwrap();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "t@t"]);
        git(root, &["config", "user.name", "t"]);
        git(root, &["add", "-A"]);
        git(root, &["commit", "-q", "-m", "init"]);
        format!("file://{}", root.display())
    }

    #[tokio::test]
    async fn install_then_resolve_and_token() {
        let (svc, artifacts, tmp) = service().await;
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let url = fixture_repo(&repo);

        let view = expect_installed(svc.install(url_input(&url)).await.unwrap());
        assert_eq!(view.name, "demo");
        assert_eq!(view.skill_count, 1);

        // Artifact resolves + is fetchable-by-hash; token authorizes it.
        let refs = svc.resolve(&["demo".into()]).await.unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(
            refs[0].hash.len(),
            64,
            "the ref carries the content hash; the agent builds the URL from it"
        );
        assert!(artifacts.path(&refs[0].hash).is_file());
        let tok = svc.mint_token("s", &[refs[0].hash.clone()]);
        assert!(
            artifacts
                .verify_token(b"secret", &tok, &refs[0].hash)
                .is_ok()
        );
        assert!(artifacts.verify_token(b"secret", &tok, "deadbeef").is_err());

        // Unknown name errors.
        assert!(svc.resolve(&["nope".into()]).await.is_err());
    }

    #[tokio::test]
    async fn default_names_reflect_flag() {
        let (svc, _artifacts, tmp) = service().await;
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let url = fixture_repo(&repo);
        svc.install(url_input(&url)).await.unwrap();
        assert!(svc.default_names().await.is_empty());
        svc.set_default(
            "demo",
            PluginDefaultInput {
                enabled_default: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(svc.default_names().await, vec!["demo".to_string()]);
    }

    /// The one box: a URL that is a catalogue records a source and returns it,
    /// rather than erroring or installing something arbitrary.
    #[tokio::test]
    async fn a_catalogue_url_registers_a_marketplace() {
        let (svc, _artifacts, tmp) = service().await;
        let url = catalogue(tmp.path(), "market");

        let m = expect_source(svc.install(url_input(&url)).await.unwrap());
        assert_eq!(m.name, "catalogue");
        assert_eq!(m.plugin_count, 2);
        assert!(m.plugins.iter().all(|p| !p.installed));
        assert_eq!(svc.list_marketplaces().await.unwrap().len(), 1);
        assert!(
            svc.list().await.unwrap().is_empty(),
            "nothing was installed"
        );
    }

    /// The second half of the one box: picking an entry installs it through the
    /// CACHED index — no second clone of the marketplace.
    #[tokio::test]
    async fn installing_from_a_marketplace_uses_the_cached_index() {
        let (svc, _artifacts, tmp) = service().await;
        let url = catalogue(tmp.path(), "market");
        svc.install(url_input(&url)).await.unwrap();

        let v = expect_installed(svc.install(pick("catalogue", "beta")).await.unwrap());
        assert_eq!(v.name, "beta");
        assert_eq!(v.skill_count, 1);
        assert_eq!(v.marketplace.as_deref(), Some("catalogue"));

        // The picker now knows not to offer it again.
        let listed = svc.list_marketplaces().await.unwrap();
        let beta = listed[0].plugins.iter().find(|p| p.name == "beta").unwrap();
        assert!(beta.installed);
        let alpha = listed[0]
            .plugins
            .iter()
            .find(|p| p.name == "alpha")
            .unwrap();
        assert!(!alpha.installed);
    }

    /// An unknown entry names what is on offer, as the CLI does.
    #[tokio::test]
    async fn an_unknown_entry_names_the_alternatives() {
        let (svc, _artifacts, tmp) = service().await;
        let url = catalogue(tmp.path(), "market");
        svc.install(url_input(&url)).await.unwrap();

        let err = svc.install(pick("catalogue", "gamma")).await.unwrap_err();
        assert!(err.contains("alpha") && err.contains("beta"), "err: {err}");
    }

    /// Neither form given is a rejection, not a panic and not a clone of "".
    #[tokio::test]
    async fn an_empty_install_input_is_rejected() {
        let (svc, _artifacts, _tmp) = service().await;
        let err = svc
            .install(PluginInstallInput {
                source_url: None,
                source_ref: None,
                marketplace: None,
                plugin_name: None,
            })
            .await
            .unwrap_err();
        assert!(err.contains("source_url"), "err: {err}");
    }

    /// Removing a source is not removing the software.
    #[tokio::test]
    async fn removing_a_marketplace_leaves_its_bundles_installed() {
        let (svc, _artifacts, tmp) = service().await;
        let url = catalogue(tmp.path(), "market");
        svc.install(url_input(&url)).await.unwrap();
        svc.install(pick("catalogue", "alpha")).await.unwrap();

        svc.remove_marketplace("catalogue").await.unwrap();
        assert!(svc.list_marketplaces().await.unwrap().is_empty());
        assert_eq!(svc.list().await.unwrap().len(), 1, "the bundle stays");
    }

    /// Re-pasting a registered marketplace refreshes it rather than erroring:
    /// "add it again" and "refresh" are the same intent from the user's side.
    #[tokio::test]
    async fn re_pasting_a_registered_marketplace_refreshes_it() {
        let (svc, _artifacts, tmp) = service().await;
        let repo = tmp.path().join("market");
        let url = catalogue(tmp.path(), "market");
        svc.install(url_input(&url)).await.unwrap();

        write_marketplace(
            &repo,
            r#"{"name":"catalogue","plugins":[
                 {"name":"alpha","source":"./plugins/alpha"},
                 {"name":"beta","source":"./plugins/beta"},
                 {"name":"gamma","source":"./plugins/gamma"}]}"#,
        );
        write_skill(&repo.join("plugins/gamma"), "g");
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "add gamma"]);

        let m = expect_source(svc.install(url_input(&url)).await.unwrap());
        assert_eq!(m.plugin_count, 3);
        assert_eq!(
            svc.list_marketplaces().await.unwrap().len(),
            1,
            "not a second row"
        );
    }

    /// The refresh button does the same, and must not turn into an install when
    /// a catalogue drops to a single entry.
    #[tokio::test]
    async fn refresh_re_reads_the_index_without_installing() {
        let (svc, _artifacts, tmp) = service().await;
        let repo = tmp.path().join("market");
        let url = catalogue(tmp.path(), "market");
        svc.install(url_input(&url)).await.unwrap();

        write_marketplace(
            &repo,
            r#"{"name":"catalogue","plugins":[
                 {"name":"alpha","source":"./plugins/alpha"}]}"#,
        );
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "drop beta"]);

        let m = svc.refresh_marketplace("catalogue").await.unwrap();
        assert_eq!(m.plugin_count, 1);
        assert!(svc.list().await.unwrap().is_empty(), "still not an install");
    }

    /// Two different sources claiming one name is rejected, naming the
    /// incumbent — silently renaming one would break the provenance already
    /// recorded on installed bundles.
    #[tokio::test]
    async fn a_name_collision_between_two_sources_is_rejected() {
        let (svc, _artifacts, tmp) = service().await;
        let first = catalogue(tmp.path(), "one");
        svc.install(url_input(&first)).await.unwrap();

        let second = catalogue(tmp.path(), "two");
        let err = svc.install(url_input(&second)).await.unwrap_err();
        assert!(
            err.contains(&first),
            "must name the incumbent source: {err}"
        );
    }

    /// A bundle installed through a catalogue re-resolves through the index on
    /// update, so an entry the catalogue has moved is followed.
    #[tokio::test]
    async fn update_re_resolves_a_marketplace_bundle_through_the_index() {
        let (svc, _artifacts, tmp) = service().await;
        let repo = tmp.path().join("market");
        let url = catalogue(tmp.path(), "market");
        svc.install(url_input(&url)).await.unwrap();
        svc.install(pick("catalogue", "beta")).await.unwrap();

        // The catalogue moves `beta` to a different directory.
        write_skill(&repo.join("moved/beta"), "b2");
        write_marketplace(
            &repo,
            r#"{"name":"catalogue","plugins":[
                 {"name":"alpha","source":"./plugins/alpha"},
                 {"name":"beta","source":"./moved/beta"}]}"#,
        );
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "move beta"]);
        svc.refresh_marketplace("catalogue").await.unwrap();

        let v = svc.update("beta").await.unwrap();
        assert_eq!(v.marketplace.as_deref(), Some("catalogue"));
        assert_eq!(v.skill_count, 1);
    }
}
