//! `PluginService` ties the store + artifact store + token together: install
//! bundles from git, serve their artifacts, and resolve per-session selections.
//! Injected into `AppState` (CRUD routes) and `ServerDeps` (as a
//! `PluginProvisioner`, for `ensure_runtime`).

use super::artifact::ArtifactStore;
use super::ingest::{self, Ingested};
use super::store::{PluginRow, PluginStore};
use super::token;
use super::{PluginArtifactRef, PluginProvisioner};
use horsie_models::plugins::{PluginDefaultInput, PluginInstallInput, PluginView};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Capability-token lifetime; covers provisioning (incl. re-attach) with margin.
const TOKEN_TTL_SECS: u64 = 3600;

pub struct PluginService {
    store: PluginStore,
    artifacts: ArtifactStore,
    token_secret: Vec<u8>,
}

impl PluginService {
    pub fn new(store: PluginStore, artifacts: ArtifactStore, token_secret: Vec<u8>) -> Self {
        Self {
            store,
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

    /// Install a bundle from a git repo.
    pub async fn install(&self, input: PluginInstallInput) -> Result<PluginView, String> {
        let ing = clone_and_pack(input.source_url.clone(), input.source_ref.clone()).await?;
        self.persist(&input, ing, None).await
    }

    /// Re-clone a bundle from its remembered source and re-pack.
    pub async fn update(&self, name: &str) -> Result<PluginView, String> {
        let existing = self
            .store
            .get(name)
            .await?
            .ok_or_else(|| format!("no such bundle '{name}'"))?;
        let ing = clone_and_pack(existing.source_url.clone(), existing.source_ref.clone()).await?;
        let input = PluginInstallInput {
            source_url: existing.source_url.clone(),
            source_ref: existing.source_ref.clone(),
        };
        self.persist(&input, ing, Some(existing)).await
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

    /// Path to a hash's artifact (for the streaming route).
    pub fn artifact_path(&self, hash: &str) -> PathBuf {
        self.artifacts.path(hash)
    }

    /// Verify a fetch token authorizes `hash` (for the streaming route).
    pub fn verify_token(&self, tok: &str, hash: &str) -> Result<(), String> {
        token::verify(&self.token_secret, tok, hash)
    }

    async fn persist(
        &self,
        input: &PluginInstallInput,
        ing: Ingested,
        existing: Option<PluginRow>,
    ) -> Result<PluginView, String> {
        self.artifacts
            .write(&ing.hash, &ing.zip_bytes)
            .map_err(|e| e.to_string())?;
        let now = now_string();
        let row = PluginRow {
            name: ing.name,
            source_kind: "git".to_string(),
            source_url: input.source_url.trim().to_string(),
            source_ref: input
                .source_ref
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            version: ing.version,
            description: ing.description,
            skill_count: ing.skill_count,
            has_hooks: ing.has_hooks,
            artifact_hash: ing.hash,
            artifact_size: ing.zip_bytes.len() as u64,
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
    async fn resolve(
        &self,
        names: &[String],
        base_url: &str,
    ) -> Result<Vec<PluginArtifactRef>, String> {
        let base = base_url.trim_end_matches('/');
        let mut refs = Vec::with_capacity(names.len());
        for name in names {
            let row = self
                .store
                .get(name)
                .await?
                .ok_or_else(|| format!("no such bundle '{name}'"))?;
            let url = format!("{base}/api/plugin-artifacts/{}.zip", row.artifact_hash);
            refs.push(PluginArtifactRef {
                name: row.name,
                hash: row.artifact_hash,
                url,
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

/// Run the blocking git clone + pack off the async runtime.
async fn clone_and_pack(url: String, git_ref: Option<String>) -> Result<Ingested, String> {
    tokio::task::spawn_blocking(move || ingest::ingest_git(&url, git_ref.as_deref()))
        .await
        .map_err(|e| e.to_string())?
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
    use std::str::FromStr;

    async fn service() -> (PluginService, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}/t.db", tmp.path().display());
        let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
            .unwrap()
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePool::connect_with(opts).await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        let artifacts = ArtifactStore::new(tmp.path().join("artifacts"));
        let svc = PluginService::new(PluginStore::new(pool), artifacts, b"secret".to_vec());
        (svc, tmp)
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
        let (svc, tmp) = service().await;
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let url = fixture_repo(&repo);

        let view = svc
            .install(PluginInstallInput {
                source_url: url.clone(),
                source_ref: None,
            })
            .await
            .unwrap();
        assert_eq!(view.name, "demo");
        assert_eq!(view.skill_count, 1);

        // Artifact resolves + is fetchable-by-hash; token authorizes it.
        let refs = svc.resolve(&["demo".into()], "http://h:1/").await.unwrap();
        assert_eq!(refs.len(), 1);
        assert!(refs[0].url.starts_with("http://h:1/api/plugin-artifacts/"));
        assert!(svc.artifact_path(&refs[0].hash).is_file());
        let tok = svc.mint_token("s", &[refs[0].hash.clone()]);
        assert!(svc.verify_token(&tok, &refs[0].hash).is_ok());
        assert!(svc.verify_token(&tok, "deadbeef").is_err());

        // Unknown name errors.
        assert!(svc.resolve(&["nope".into()], "http://h:1").await.is_err());
    }

    #[tokio::test]
    async fn default_names_reflect_flag() {
        let (svc, tmp) = service().await;
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let url = fixture_repo(&repo);
        svc.install(PluginInstallInput {
            source_url: url,
            source_ref: None,
        })
        .await
        .unwrap();
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
}
