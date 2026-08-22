//! `PluginService` ties the store + artifact store + token together: install
//! bundles from git, serve their artifacts, and resolve per-session selections.
//! Injected into `AppState` (CRUD routes) and `ServerDeps` (as a
//! `PluginProvisioner`, for `ensure_runtime`).

use super::artifact::ArtifactStore;
use super::ingest::{self, IngestTarget, Ingested, ParsedMarketplace, PluginBundle};
use super::marketplace_store::{MarketplaceRow, MarketplaceStore};
use super::store::{PluginRow, PluginStore};
use super::{PluginProvisioner, kind};
use horsie_models::plugins::PluginKind;
use horsie_models::plugins::{
    CatalogEntryView, InstallOutcome, MarketplacePluginView, MarketplaceView, PluginDefaultInput,
    PluginInstallInput, PluginView,
};
use horsie_models::runtime::{BundleGeneration, BundleHash, BundleRef, BundleVersion};
use horsie_support::plugin::source_location;
use horsie_support::remote_url::redact_url_credentials;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct PluginService {
    store: PluginStore,
    marketplaces: MarketplaceStore,
    /// Shared with every other account's service: artifacts are addressed by
    /// the hash of their own bytes, so there is one file per bundle version on
    /// the whole deployment.
    artifacts: Arc<ArtifactStore>,
    /// The authored source of truth, for reading only. Provisioning has to be
    /// able to render an authored package; changing one is the authoring
    /// service's job, and keeping the write side out of here is what stops the
    /// two services depending on each other.
    authored: super::authored::AuthoredStore,
}

impl PluginService {
    pub fn new(
        store: PluginStore,
        marketplaces: MarketplaceStore,
        artifacts: Arc<ArtifactStore>,
        authored: super::authored::AuthoredStore,
    ) -> Self {
        Self {
            store,
            marketplaces,
            artifacts,
            authored,
        }
    }

    /// Write a row directly. Test-only: every real path goes through
    /// `persist`, which derives the row from bytes it packed itself.
    #[cfg(test)]
    pub async fn upsert_for_test(&self, row: &PluginRow) -> Result<(), String> {
        self.store.upsert(row).await
    }

    /// The stored row for `name`, for callers that need the kind rather than
    /// the view.
    pub async fn row(&self, name: &str) -> Result<Option<PluginRow>, String> {
        self.store.get(name).await
    }

    /// Record a freshly rendered authored package.
    ///
    /// No artifact is written: the bytes are reproducible from the tables, so a
    /// file on disk would be a cache with nothing to invalidate it. The digest
    /// and size are stored for display; what a runtime verifies against is
    /// recomputed at resolve time.
    pub async fn persist_authored(
        &self,
        bundle: PluginBundle,
        generation: u64,
    ) -> Result<PluginView, String> {
        let kind = PluginKind::Authored(horsie_models::plugins::AuthoredOrigin { generation });
        self.persist(bundle, kind).await
    }

    /// Drop the published row for an authored plugin that no longer renders
    /// anything installable. Silent when there is nothing to remove, and never
    /// touches a bundle that came from a clone.
    pub async fn remove_if_authored(&self, name: &str) -> Result<(), String> {
        match self.store.get(name).await? {
            Some(row) if kind::is_authored(&row.kind) => self.remove(name).await,
            _ => Ok(()),
        }
    }

    /// The zip a runtime fetches, for either kind of bundle.
    ///
    /// An external bundle is read from the artifact store by its hash. An
    /// authored one is rendered from the tables — which is why a stale
    /// generation is refused rather than served: the rows only hold their
    /// current state, so there is no revision of the package to hand back but
    /// the current one, and quietly substituting it would give the caller
    /// bytes whose digest it was never told.
    pub async fn package(&self, name: &str, version: &BundleVersion) -> Result<Vec<u8>, String> {
        let row = self
            .store
            .get(name)
            .await?
            .ok_or_else(|| format!("no such bundle '{name}'"))?;
        match (version, &row.kind) {
            (BundleVersion::Hash(h), _) => {
                std::fs::read(self.artifacts.path(&h.hash)).map_err(|e| e.to_string())
            }
            (BundleVersion::Generation(g), PluginKind::Authored(current)) => {
                if g.generation != current.generation {
                    return Err(format!(
                        "'{name}' is at generation {}, not {} — provision again",
                        current.generation, g.generation
                    ));
                }
                let (bundle, _) = super::authored::pack(&self.authored, name).await?;
                Ok(bundle.zip_bytes)
            }
            (BundleVersion::Generation(_), _) => Err(format!(
                "'{name}' is not an authored bundle, so it has no generations"
            )),
        }
    }

    /// Every artifact hash this account has installed.
    ///
    /// The artifact route's authorization check. Deliberately the *account's*
    /// rows rather than the deployment's: `ArtifactStore` is shared by content
    /// across every account, so asking it what exists would answer for all of
    /// them at once.
    pub async fn installed_hashes(&self) -> Result<std::collections::HashSet<String>, String> {
        Ok(self
            .store
            .list()
            .await?
            .into_iter()
            .filter(|row| !kind::is_authored(&row.kind))
            .map(|row| row.digest)
            .collect())
    }

    pub async fn list(&self) -> Result<Vec<PluginView>, String> {
        let mut rows = self.store.list().await?;
        for row in &mut rows {
            if row.catalog.is_empty() {
                self.backfill(row).await;
            }
        }
        Ok(rows.into_iter().map(row_to_view).collect())
    }

    /// Re-derive a bundle's catalogue from the artifact it was installed from.
    ///
    /// A row installed before catalogues existed has none, and SQL cannot open
    /// a zip — but the bytes are still on disk, addressed by a hash the row
    /// already carries. Doing it here rather than in a migration also
    /// self-heals a column that was somehow lost, and costs one unzip per
    /// bundle per server lifetime.
    ///
    /// Best-effort throughout: a bundle whose artifact has been collected stays
    /// empty rather than failing a list nobody could then repair.
    async fn backfill(&self, row: &mut PluginRow) {
        let path = self.artifacts.path(&row.digest);
        let Ok(bytes) = std::fs::read(&path) else {
            tracing::warn!(plugin = %row.name, "no artifact to catalogue from");
            return;
        };
        let Ok(tmp) = tempfile::tempdir() else {
            return;
        };
        if let Err(e) = super::ingest::unpack_zip(&bytes, tmp.path()) {
            tracing::warn!(plugin = %row.name, error = %e, "unreadable artifact");
            return;
        }
        let Ok(root) = horsie_support::plugin::PluginRoot::inspect(tmp.path()) else {
            return;
        };
        let catalog = horsie_support::plugin::catalog::build(&root);
        if catalog.is_empty() {
            return;
        }
        row.catalog = catalog;
        if let Err(e) = self.store.upsert(row).await {
            // The read still succeeds; only the cache of it failed.
            tracing::warn!(plugin = %row.name, error = %e, "could not persist catalogue");
        }
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
            Ingested::Plugin(bundle, origin) => {
                let kind = kind::from_dialect(bundle.dialect, origin);
                Ok(InstallOutcome::Installed(self.persist(bundle, kind).await?))
            }
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
        let (bundle, mut origin) = match blocking_ingest(IngestTarget::Resolved {
            url,
            git_ref,
            subpath,
            name_hint: Some(entry_name.clone()),
        })
        .await?
        {
            Ingested::Plugin(b, o) => (b, o),
            // Unreachable by construction: `Resolved` never classifies.
            Ingested::Marketplace(m) => {
                return Err(format!("'{}' resolved to a marketplace", m.url));
            }
        };
        origin.marketplace = Some(market.to_string());
        origin.marketplace_entry = Some(entry_name);
        let kind = kind::from_dialect(bundle.dialect, origin);
        let view = self.persist(bundle, kind).await?;
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
        // An authored bundle has no upstream. Re-cloning it is not "no-op", it
        // is meaningless — which the union makes a match arm rather than a
        // guard someone has to remember to write.
        let Some(origin) = kind::external(&existing.kind) else {
            return Err(format!(
                "'{name}' was authored here, so there is nothing to update it from. \
                 Edit its skills instead."
            ));
        };
        let outcome = match (&origin.marketplace, &origin.marketplace_entry) {
            (Some(market), Some(entry)) => self.install_entry(market, entry).await?,
            _ => {
                let target = IngestTarget::Resolved {
                    url: origin.url.clone(),
                    git_ref: origin.git_ref.clone(),
                    subpath: origin.subpath.clone(),
                    name_hint: Some(existing.name.clone()),
                };
                match blocking_ingest(target).await? {
                    Ingested::Plugin(b, o) => {
                        let kind = kind::from_dialect(b.dialect, o);
                        InstallOutcome::Installed(self.persist(b, kind).await?)
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
                parsed.name,
                redact_url_credentials(&prev.source_url)
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
            // A clone URL may carry a credential and this view is read by the
            // browser. The row keeps the real one — refresh needs it.
            source_url: redact_url_credentials(&row.source_url),
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

    /// Write the artifact and the row.
    ///
    /// The kind is what ingest actually resolved, not what the caller asked
    /// for — a marketplace entry may name another repo, and `update` has to
    /// re-clone the same tree.
    ///
    /// An authored bundle writes no artifact. Its package is rendered from the
    /// tables on demand, so a file on disk would be a cache with no one to
    /// invalidate it.
    async fn persist(&self, bundle: PluginBundle, kind: PluginKind) -> Result<PluginView, String> {
        let existing = self.store.get(&bundle.name).await?;
        if !kind::is_authored(&kind) {
            self.artifacts
                .write(&bundle.hash, &bundle.zip_bytes)
                .map_err(|e| e.to_string())?;
        }
        let now = now_string();
        let row = PluginRow {
            name: bundle.name,
            kind,
            version: bundle.version,
            description: bundle.description,
            catalog: bundle.catalog,
            has_hooks: bundle.has_hooks,
            digest: bundle.hash,
            artifact_size: bundle.zip_bytes.len() as u64,
            enabled_default: existing.as_ref().is_some_and(|e| e.enabled_default),
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
    async fn resolve(&self, names: &[String]) -> Result<Vec<BundleRef>, String> {
        let mut refs = Vec::with_capacity(names.len());
        for name in names {
            let row = self
                .store
                .get(name)
                .await?
                .ok_or_else(|| format!("no such bundle '{name}'"))?;
            let mut bundle = bundle_ref(&row);
            // An authored package is rendered, so its bytes are a function of
            // this server's renderer as well as of its rows. Recomputing the
            // digest here means the value a runtime is told to check against is
            // always the value of the bytes this server will serve it — an
            // upgrade that changed the renderer cannot leave the two disagreeing.
            if kind::is_authored(&row.kind) {
                let (rendered, _) = super::authored::pack(&self.authored, name).await?;
                if rendered.hash != row.digest {
                    tracing::info!(
                        plugin = %name,
                        was = %row.digest,
                        now = %rendered.hash,
                        "authored package re-rendered to different bytes"
                    );
                }
                bundle.digest = rendered.hash;
            }
            refs.push(bundle);
        }
        Ok(refs)
    }

    async fn catalog(
        &self,
        names: &[String],
    ) -> Vec<horsie_support::plugin::catalog::CatalogEntry> {
        let selected: Vec<String> = if names.is_empty() {
            self.default_names().await
        } else {
            names.to_vec()
        };
        // Sorted, so "first wins" is a rule rather than whatever order the
        // caller happened to pass.
        let mut wanted = selected;
        wanted.sort();
        let mut seen: std::collections::HashMap<(String, char), String> =
            std::collections::HashMap::new();
        let mut out = Vec::new();
        for name in &wanted {
            let Ok(Some(mut row)) = self.store.get(name).await else {
                continue;
            };
            if row.catalog.is_empty() {
                self.backfill(&mut row).await;
            }
            for entry in row.catalog {
                // Keyed by name *and* sigil: `/review` and `@review` are two
                // different things a user can type, so they do not collide.
                let key = (entry.name.clone(), entry.kind.sigil());
                match seen.get(&key) {
                    Some(kept) => tracing::warn!(
                        plugin = %row.name,
                        kept = %kept,
                        name = %entry.name,
                        "duplicate catalogue entry; keeping first"
                    ),
                    None => {
                        seen.insert(key, row.name.clone());
                        out.push(entry);
                    }
                }
            }
        }
        out
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
    if let Ingested::Plugin(b, _) = &ingested {
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

/// How a runtime names one revision of this bundle, and what it checks the
/// bytes against once it has them.
fn bundle_ref(row: &PluginRow) -> BundleRef {
    let version = match kind::generation(&row.kind) {
        Some(generation) => BundleVersion::Generation(BundleGeneration { generation }),
        None => BundleVersion::Hash(BundleHash {
            hash: row.digest.clone(),
        }),
    };
    BundleRef {
        name: row.name.clone(),
        version,
        digest: row.digest.clone(),
    }
}

fn row_to_view(row: PluginRow) -> PluginView {
    PluginView {
        name: row.name,
        description: row.description,
        version: row.version,
        kind: redact_kind(row.kind),
        catalog: row.catalog.into_iter().map(entry_to_view).collect(),
        has_hooks: row.has_hooks,
        enabled_default: row.enabled_default,
        artifact_size: row.artifact_size,
    }
}

/// See `marketplace_view`: a stored URL can hold a credential, and this is the
/// shape `GET /api/plugins` returns to a browser.
fn redact_kind(kind: PluginKind) -> PluginKind {
    match kind {
        PluginKind::Claude(mut e) => {
            e.url = redact_url_credentials(&e.url);
            PluginKind::Claude(e)
        }
        PluginKind::AgentPlugin(mut e) => {
            e.url = redact_url_credentials(&e.url);
            PluginKind::AgentPlugin(e)
        }
        // Nothing to redact: an authored bundle has no remote at all.
        PluginKind::Authored(a) => PluginKind::Authored(a),
    }
}

/// Drop the template on the way out. The server expands, so no client needs a
/// command's body — and `code-review.md` alone runs past a page.
fn entry_to_view(entry: horsie_support::plugin::catalog::CatalogEntry) -> CatalogEntryView {
    CatalogEntryView {
        kind: match entry.kind {
            horsie_support::plugin::catalog::CatalogKind::Command => "command",
            horsie_support::plugin::catalog::CatalogKind::Skill => "skill",
            horsie_support::plugin::catalog::CatalogKind::Agent => "agent",
        }
        .to_string(),
        name: entry.name,
        description: entry.description,
        argument_hint: entry.argument_hint,
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

    /// The catalogue a view says it came through, whichever external arm it is.
    fn marketplace_of(view: &PluginView) -> Option<String> {
        kind::external(&view.kind).and_then(|e| e.marketplace.clone())
    }

    async fn service() -> (PluginService, Arc<ArtifactStore>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let db = crate::db::testing::db().await;
        let artifacts = Arc::new(ArtifactStore::new(tmp.path().join("artifacts")));
        let svc = PluginService::new(
            PluginStore::new(db.clone(), crate::projects::ProjectId::new("1")),
            MarketplaceStore::new(db.clone(), crate::projects::ProjectId::new("1")),
            artifacts.clone(),
            crate::plugins::authored::AuthoredStore::new(db, crate::projects::ProjectId::new("1")),
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
        std::fs::write(
            s.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: d\n---\nbody"),
        )
        .unwrap();
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
        std::fs::write(d.join("SKILL.md"), "---\nname: a\ndescription: d\n---\nx").unwrap();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "t@t"]);
        git(root, &["config", "user.name", "t"]);
        git(root, &["add", "-A"]);
        git(root, &["commit", "-q", "-m", "init"]);
        format!("file://{}", root.display())
    }

    /// A bundle's entries reach the client; a command's body does not. The
    /// server expands, so shipping templates would be waste — `code-review.md`
    /// alone runs past a page.
    #[tokio::test]
    async fn a_view_carries_the_catalogue_without_templates() {
        let (svc, _artifacts, tmp) = service().await;
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let url = fixture_repo(&repo);
        std::fs::create_dir_all(repo.join("commands")).unwrap();
        std::fs::write(
            repo.join("commands/review.md"),
            "---\ndescription: reviews a file\nargument-hint: <path>\n---\nReview $1 for bugs",
        )
        .unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "cmd"]);

        expect_installed(svc.install(url_input(&url)).await.unwrap());
        let listed = svc.list().await.unwrap();
        let view = listed.iter().find(|v| v.name == "demo").unwrap();

        let review = view.catalog.iter().find(|e| e.name == "review").unwrap();
        assert_eq!(review.kind, "command");
        assert_eq!(review.description, "reviews a file");
        assert_eq!(review.argument_hint.as_deref(), Some("<path>"));
        let json = serde_json::to_string(&view).unwrap();
        assert!(
            !json.contains("Review $1 for bugs"),
            "the template must not reach a client: {json}"
        );
    }

    /// A row installed before catalogues existed has none, and SQL cannot open
    /// a zip. The bytes are still on disk, so reading the list re-derives it.
    #[tokio::test]
    async fn a_null_catalogue_is_backfilled_from_the_artifact() {
        let (svc, _artifacts, tmp) = service().await;
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let url = fixture_repo(&repo);
        expect_installed(svc.install(url_input(&url)).await.unwrap());

        // Forget it, the way an older row would have arrived.
        let mut row = svc.store.get("demo").await.unwrap().unwrap();
        row.catalog = Vec::new();
        svc.store.upsert(&row).await.unwrap();
        assert!(
            svc.store
                .get("demo")
                .await
                .unwrap()
                .unwrap()
                .catalog
                .is_empty()
        );

        let listed = svc.list().await.unwrap();
        let view = listed.iter().find(|v| v.name == "demo").unwrap();
        assert_eq!(
            view.catalog.iter().filter(|e| e.kind == "skill").count(),
            1,
            "the list re-derives what the column lost"
        );
        assert!(
            !svc.store
                .get("demo")
                .await
                .unwrap()
                .unwrap()
                .catalog
                .is_empty(),
            "and persists it, so the next read is free"
        );
    }

    #[tokio::test]
    async fn install_then_resolve_and_token() {
        let (svc, artifacts, tmp) = service().await;
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let url = fixture_repo(&repo);

        let view = expect_installed(svc.install(url_input(&url)).await.unwrap());
        assert_eq!(view.name, "demo");
        assert_eq!(view.catalog.iter().filter(|e| e.kind == "skill").count(), 1);

        // Artifact resolves + is fetchable-by-hash, and the account's installed
        // set is what the bundle route authorizes against.
        let refs = svc.resolve(&["demo".into()]).await.unwrap();
        assert_eq!(refs.len(), 1);
        assert!(
            matches!(&refs[0].version, BundleVersion::Hash(h) if h.hash.len() == 64),
            "a cloned bundle is named by its content hash, not a generation"
        );
        assert_eq!(
            refs[0].digest.len(),
            64,
            "the ref carries the digest the runtime checks the bytes against"
        );
        assert!(artifacts.path(&refs[0].digest).is_file());
        let installed = svc.installed_hashes().await.unwrap();
        assert!(installed.contains(&refs[0].digest));
        assert!(!installed.contains("deadbeef"));

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
        assert_eq!(v.catalog.iter().filter(|e| e.kind == "skill").count(), 1);
        assert_eq!(marketplace_of(&v).as_deref(), Some("catalogue"));

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
        assert_eq!(marketplace_of(&v).as_deref(), Some("catalogue"));
        assert_eq!(v.catalog.iter().filter(|e| e.kind == "skill").count(), 1);
    }
}
