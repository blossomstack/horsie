//! Authoring: the write side of authored plugins, and the packaging that turns
//! their rows into the same kind of bundle a clone produces.
//!
//! # Why the generation and the digest are different things
//!
//! The generation names the *rows*. It advances when someone edits a skill, and
//! it is what a runtime fetches by. The digest names the *bytes*. It advances
//! whenever the renderer's output would differ — which includes a server
//! upgrade that changed the renderer, with no row having moved at all.
//!
//! Keeping them separate is what removes a whole failure mode. The digest is
//! recomputed every time a bundle is resolved for provisioning, so the value a
//! runtime is told to check against is always the value of the bytes that
//! server will serve. The copy in the `plugins` row is a cache for display; it
//! is never what anybody verifies against.

use super::render::{self, RenderedSkill};
use super::store::{AuthoredFile, AuthoredPluginRow, AuthoredSkillRow, AuthoredStore};
use crate::plugins::PluginService;
use crate::plugins::ingest::{PluginBundle, bundle_from_dir};
use horsie_models::plugins::{
    AuthoredFileView, AuthoredPluginView, AuthoredRevisionView, AuthoredSkillRestoreInput,
    AuthoredSkillSummary, AuthoredSkillView, AuthoredSkillWriteInput,
};
use std::sync::Arc;

/// Render and pack one authored plugin as it currently stands.
///
/// Free rather than a method so `PluginService` can call it while resolving a
/// bundle without owning the write side — provisioning needs the bytes, not the
/// ability to change them.
pub async fn pack(store: &AuthoredStore, name: &str) -> Result<(PluginBundle, u64), String> {
    let plugin = store
        .get_plugin(name)
        .await?
        .ok_or_else(|| format!("no authored plugin '{name}'"))?;
    let mut skills = Vec::new();
    for skill in store.list_skills(Some(name)).await? {
        let files = store.files_for(name, &skill.name).await?;
        skills.push(RenderedSkill {
            name: skill.name,
            description: skill.description,
            body: skill.body,
            files: files.into_iter().map(|f| (f.path, f.content)).collect(),
        });
    }
    let generation = plugin.generation;
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    render::render(
        dir.path(),
        &plugin.name,
        plugin.description.as_deref(),
        generation,
        &skills,
    )?;
    // Through the same reader a clone goes through, so an authored bundle
    // cannot be described differently from a cloned one holding the same skill.
    let bundle = bundle_from_dir(dir.path(), &plugin.name, Some(format!("0.0.{generation}")))?;
    Ok((bundle, generation))
}

pub struct AuthoredService {
    store: AuthoredStore,
    plugins: Arc<PluginService>,
}

impl AuthoredService {
    pub fn new(store: AuthoredStore, plugins: Arc<PluginService>) -> Self {
        Self { store, plugins }
    }

    pub async fn list(&self) -> Result<Vec<AuthoredPluginView>, String> {
        let mut out = Vec::new();
        for plugin in self.store.list_plugins().await? {
            let skills = self.store.list_skills(Some(&plugin.name)).await?;
            out.push(to_plugin_view(plugin, skills));
        }
        Ok(out)
    }

    pub async fn get(&self, name: &str) -> Result<AuthoredPluginView, String> {
        let plugin = self
            .store
            .get_plugin(name)
            .await?
            .ok_or_else(|| format!("no authored plugin '{name}'"))?;
        let skills = self.store.list_skills(Some(name)).await?;
        Ok(to_plugin_view(plugin, skills))
    }

    /// Create an authored plugin, or change its description.
    ///
    /// The name is held to the Agent Plugins grammar because it is rendered
    /// into a `plugin.json` any conformant client has to be able to read — and
    /// because it becomes a directory name in every runtime's plugin tree.
    pub async fn write_plugin(
        &self,
        name: &str,
        description: Option<&str>,
    ) -> Result<AuthoredPluginView, String> {
        let name = name.trim();
        horsie_support::plugin::manifest::validate_name(name)?;
        self.refuse_if_external(name).await?;
        self.store.upsert_plugin(name, description, &now()).await?;
        self.republish(name).await?;
        self.get(name).await
    }

    pub async fn delete_plugin(&self, name: &str) -> Result<(), String> {
        self.require_authored(name).await?;
        self.store.delete_plugin(name).await?;
        self.plugins.remove(name).await
    }

    pub async fn list_skills(
        &self,
        plugin: Option<&str>,
    ) -> Result<Vec<AuthoredSkillSummary>, String> {
        Ok(self
            .store
            .list_skills(plugin)
            .await?
            .into_iter()
            .map(to_summary)
            .collect())
    }

    pub async fn get_skill(&self, plugin: &str, name: &str) -> Result<AuthoredSkillView, String> {
        let row = self
            .store
            .get_skill(plugin, name)
            .await?
            .ok_or_else(|| format!("no skill '{name}' in '{plugin}'"))?;
        let files = self.store.files_for(plugin, name).await?;
        Ok(to_skill_view(row, files))
    }

    /// Create or replace one skill, then re-render the package.
    ///
    /// Omitted fields keep their current value, so fixing a typo in a
    /// description does not mean resending the body. Creating requires both:
    /// a skill with no description is invisible to every reader horsie has,
    /// including its own.
    pub async fn write_skill(
        &self,
        input: AuthoredSkillWriteInput,
    ) -> Result<AuthoredSkillView, String> {
        let plugin = input.plugin.trim().to_string();
        let name = input.name.trim().to_string();
        horsie_support::plugin::manifest::validate_name(&name)?;
        self.require_authored(&plugin).await?;

        let existing = self.store.get_skill(&plugin, &name).await?;
        let description = match (input.description, &existing) {
            (Some(d), _) => d,
            (None, Some(e)) => e.description.clone(),
            (None, None) => {
                return Err(format!(
                    "creating skill '{name}' needs a description — it is what a picker \
                     shows and what the model chooses from"
                ));
            }
        };
        let body = match (input.body, &existing) {
            (Some(b), _) => b,
            (None, Some(e)) => e.body.clone(),
            (None, None) => return Err(format!("creating skill '{name}' needs a body")),
        };
        if description.trim().is_empty() {
            return Err("a skill's description must not be empty".to_string());
        }
        let files: Vec<AuthoredFile> = match input.files {
            Some(files) => files
                .into_iter()
                .map(|f| AuthoredFile {
                    path: f.path,
                    content: f.content,
                })
                .collect(),
            None => self.store.files_for(&plugin, &name).await?,
        };
        for file in &files {
            render::validate_file_path(&file.path)?;
        }

        let row = AuthoredSkillRow {
            plugin: plugin.clone(),
            name: name.clone(),
            description,
            body,
            // Assigned by the store inside the transaction; this is the value
            // it replaces.
            revision: 0,
            updated_at: now(),
        };
        self.store.save_skill(&row, &files, &now()).await?;
        self.republish(&plugin).await?;
        self.get_skill(&plugin, &name).await
    }

    pub async fn delete_skill(&self, plugin: &str, name: &str) -> Result<(), String> {
        self.require_authored(plugin).await?;
        if self.store.get_skill(plugin, name).await?.is_none() {
            return Err(format!("no skill '{name}' in '{plugin}'"));
        }
        self.store.delete_skill(plugin, name, &now()).await?;
        self.republish(plugin).await
    }

    pub async fn revisions(
        &self,
        plugin: &str,
        skill: &str,
    ) -> Result<Vec<AuthoredRevisionView>, String> {
        Ok(self
            .store
            .revisions(plugin, skill)
            .await?
            .into_iter()
            .map(|r| AuthoredRevisionView {
                revision: r.revision,
                description: r.description,
                body: r.body,
                files: r.files.into_iter().map(to_file_view).collect(),
                deleted: r.deleted,
                created_at: r.created_at,
            })
            .collect())
    }

    /// Put a skill back to one of its own past revisions.
    ///
    /// The restore is itself a new revision, so undoing loses nothing — and
    /// restoring a tombstone is a delete, which is the only reading of it that
    /// does not silently resurrect something.
    pub async fn restore_skill(
        &self,
        input: AuthoredSkillRestoreInput,
    ) -> Result<AuthoredSkillView, String> {
        self.require_authored(&input.plugin).await?;
        let target = self
            .store
            .revision(&input.plugin, &input.name, input.revision)
            .await?
            .ok_or_else(|| {
                format!(
                    "'{}/{}' has no revision {}",
                    input.plugin, input.name, input.revision
                )
            })?;
        if target.deleted {
            self.delete_skill(&input.plugin, &input.name).await?;
            return Err(format!(
                "revision {} of '{}/{}' is its deletion, so it has been deleted again",
                input.revision, input.plugin, input.name
            ));
        }
        self.write_skill(AuthoredSkillWriteInput {
            plugin: input.plugin,
            name: input.name,
            description: Some(target.description),
            body: Some(target.body),
            files: Some(target.files.into_iter().map(to_file_view).collect()),
        })
        .await
    }

    /// Re-render and re-record the `plugins` row this plugin publishes through.
    ///
    /// A plugin with no skills publishes nothing: the reader refuses a tree
    /// that offers nothing runnable, so the row is removed rather than left
    /// pointing at a package that cannot be installed.
    async fn republish(&self, name: &str) -> Result<(), String> {
        match pack(&self.store, name).await {
            Ok((bundle, generation)) => {
                self.plugins.persist_authored(bundle, generation).await?;
                Ok(())
            }
            Err(e) if e.contains("not a plugin bundle") => {
                self.plugins.remove_if_authored(name).await
            }
            Err(e) => Err(e),
        }
    }

    /// Refuse to touch a bundle that came from a clone.
    ///
    /// Authoring may only ever write rows it owns. Without this an agent could
    /// overwrite a plugin an operator installed and every other session loads.
    async fn require_authored(&self, name: &str) -> Result<AuthoredPluginRow, String> {
        self.refuse_if_external(name).await?;
        self.store
            .get_plugin(name)
            .await?
            .ok_or_else(|| format!("no authored plugin '{name}'"))
    }

    async fn refuse_if_external(&self, name: &str) -> Result<(), String> {
        if let Some(row) = self.plugins.row(name).await?
            && !crate::plugins::kind::is_authored(&row.kind)
        {
            return Err(format!(
                "'{name}' was installed from a source outside this server, so it \
                 cannot be edited here. Pick another name."
            ));
        }
        Ok(())
    }
}

fn to_plugin_view(plugin: AuthoredPluginRow, skills: Vec<AuthoredSkillRow>) -> AuthoredPluginView {
    AuthoredPluginView {
        name: plugin.name,
        description: plugin.description,
        generation: plugin.generation,
        skills: skills.into_iter().map(to_summary).collect(),
    }
}

fn to_summary(row: AuthoredSkillRow) -> AuthoredSkillSummary {
    AuthoredSkillSummary {
        plugin: row.plugin,
        name: row.name,
        description: row.description,
        revision: row.revision,
        updated_at: row.updated_at,
    }
}

fn to_skill_view(row: AuthoredSkillRow, files: Vec<AuthoredFile>) -> AuthoredSkillView {
    AuthoredSkillView {
        plugin: row.plugin,
        name: row.name,
        description: row.description,
        body: row.body,
        files: files.into_iter().map(to_file_view).collect(),
        revision: row.revision,
        updated_at: row.updated_at,
    }
}

fn to_file_view(file: AuthoredFile) -> AuthoredFileView {
    AuthoredFileView {
        path: file.path,
        content: file.content,
    }
}

/// Unix epoch millis, as every other row in this schema stores time.
fn now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().to_string())
        .unwrap_or_else(|_| "0".to_string())
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
    use crate::plugins::{ArtifactStore, MarketplaceStore, PluginProvisioner, PluginStore};
    use crate::projects::ProjectId;

    async fn service() -> (AuthoredService, Arc<PluginService>) {
        let db = crate::db::testing::db().await;
        let project = ProjectId::new("1");
        let tmp = tempfile::tempdir().unwrap();
        let plugins = Arc::new(PluginService::new(
            PluginStore::new(db.clone(), project.clone()),
            MarketplaceStore::new(db.clone(), project.clone()),
            Arc::new(ArtifactStore::new(tmp.path().join("artifacts"))),
            AuthoredStore::new(db.clone(), project.clone()),
        ));
        let authored = AuthoredService::new(AuthoredStore::new(db, project), plugins.clone());
        (authored, plugins)
    }

    fn write(plugin: &str, name: &str, description: &str, body: &str) -> AuthoredSkillWriteInput {
        AuthoredSkillWriteInput {
            plugin: plugin.to_string(),
            name: name.to_string(),
            description: Some(description.to_string()),
            body: Some(body.to_string()),
            files: None,
        }
    }

    /// The whole loop: a plugin, a skill in it, and a `plugins` row that a
    /// session can select — with no clone anywhere.
    #[tokio::test]
    async fn writing_a_skill_publishes_a_bundle() {
        let (authored, plugins) = service().await;
        authored
            .write_plugin("notes", Some("things I learnt"))
            .await
            .unwrap();
        authored
            .write_skill(write("notes", "deploying", "how to deploy", "Step 1."))
            .await
            .unwrap();

        let row = plugins.row("notes").await.unwrap().expect("published");
        assert!(crate::plugins::kind::is_authored(&row.kind));
        assert_eq!(
            row.catalog
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            vec!["deploying"],
            "the published bundle offers the skill that was written"
        );
        assert_eq!(row.version.as_deref(), Some("0.0.2"));
    }

    /// A plugin with nothing in it renders a tree the reader refuses, so it
    /// must not sit in the library as an entry that cannot be installed.
    #[tokio::test]
    async fn an_empty_plugin_publishes_nothing() {
        let (authored, plugins) = service().await;
        authored.write_plugin("empty", None).await.unwrap();
        assert!(plugins.row("empty").await.unwrap().is_none());
    }

    /// Supplied fields replace, omitted ones are left alone — the difference
    /// between fixing a typo and having to resend the body.
    #[tokio::test]
    async fn an_omitted_field_keeps_its_value() {
        let (authored, _) = service().await;
        authored.write_plugin("notes", None).await.unwrap();
        authored
            .write_skill(write("notes", "x", "first", "body one"))
            .await
            .unwrap();
        let view = authored
            .write_skill(AuthoredSkillWriteInput {
                plugin: "notes".into(),
                name: "x".into(),
                description: Some("second".into()),
                body: None,
                files: None,
            })
            .await
            .unwrap();
        assert_eq!(view.description, "second");
        assert_eq!(view.body, "body one", "an omitted body is not cleared");
        assert_eq!(view.revision, 2);
    }

    /// Creating needs both halves: the reader refuses a skill without a
    /// description at both ends, so accepting one here would write something
    /// nothing can load.
    #[tokio::test]
    async fn creating_without_a_description_is_refused() {
        let (authored, _) = service().await;
        authored.write_plugin("notes", None).await.unwrap();
        let err = authored
            .write_skill(AuthoredSkillWriteInput {
                plugin: "notes".into(),
                name: "x".into(),
                description: None,
                body: Some("b".into()),
                files: None,
            })
            .await
            .unwrap_err();
        assert!(err.contains("description"), "{err}");
    }

    /// Deleting keeps the history, and restoring brings the skill back as a
    /// revision of its own rather than by rewinding the log.
    #[tokio::test]
    async fn delete_keeps_history_and_restore_is_a_new_revision() {
        let (authored, plugins) = service().await;
        authored.write_plugin("notes", None).await.unwrap();
        authored
            .write_skill(write("notes", "x", "first", "one"))
            .await
            .unwrap();
        authored
            .write_skill(write("notes", "x", "second", "two"))
            .await
            .unwrap();
        authored.delete_skill("notes", "x").await.unwrap();

        assert!(
            plugins.row("notes").await.unwrap().is_none(),
            "the last skill leaving takes the published bundle with it"
        );
        let history = authored.revisions("notes", "x").await.unwrap();
        assert_eq!(history.len(), 3, "two writes and the delete");
        assert!(history[0].deleted);

        let restored = authored
            .restore_skill(AuthoredSkillRestoreInput {
                plugin: "notes".into(),
                name: "x".into(),
                revision: 1,
            })
            .await
            .unwrap();
        assert_eq!(restored.body, "one");
        assert_eq!(
            restored.revision, 4,
            "restoring appends, it does not rewind"
        );
        assert!(plugins.row("notes").await.unwrap().is_some());
    }

    /// The guard that keeps authoring from being a way to rewrite a plugin an
    /// operator installed and every other session loads.
    #[tokio::test]
    async fn a_cloned_bundle_cannot_be_edited_here() {
        let (authored, plugins) = service().await;
        let row = crate::plugins::PluginRow {
            name: "installed".into(),
            kind: horsie_models::plugins::PluginKind::Claude(
                horsie_models::plugins::ExternalOrigin {
                    url: "https://example.com/x.git".into(),
                    git_ref: None,
                    subpath: None,
                    marketplace: None,
                    marketplace_entry: None,
                },
            ),
            version: None,
            description: None,
            catalog: Vec::new(),
            has_hooks: false,
            digest: "h".into(),
            artifact_size: 1,
            enabled_default: false,
            created_at: "1".into(),
            updated_at: "1".into(),
        };
        plugins.upsert_for_test(&row).await.unwrap();

        let err = authored.write_plugin("installed", None).await.unwrap_err();
        assert!(err.contains("outside this server"), "{err}");
        let err = authored
            .write_skill(write("installed", "x", "d", "b"))
            .await
            .unwrap_err();
        assert!(err.contains("outside this server"), "{err}");
    }

    /// A path that would write outside the skill's directory is refused before
    /// anything reaches the disk.
    #[tokio::test]
    async fn a_traversing_file_is_refused() {
        let (authored, _) = service().await;
        authored.write_plugin("notes", None).await.unwrap();
        let err = authored
            .write_skill(AuthoredSkillWriteInput {
                plugin: "notes".into(),
                name: "x".into(),
                description: Some("d".into()),
                body: Some("b".into()),
                files: Some(vec![AuthoredFileView {
                    path: "../escape.sh".into(),
                    content: "rm -rf /".into(),
                }]),
            })
            .await
            .unwrap_err();
        assert!(err.contains("file path"), "{err}");
    }

    /// The ref a runtime is handed names the rows by generation and carries a
    /// digest of the bytes the server will actually render — computed here, not
    /// read from the row, so a renderer that has moved cannot leave the two
    /// disagreeing.
    #[tokio::test]
    async fn resolve_names_the_generation_and_digests_the_rendered_bytes() {
        let (authored, plugins) = service().await;
        authored.write_plugin("notes", None).await.unwrap();
        authored
            .write_skill(write("notes", "x", "d", "b"))
            .await
            .unwrap();

        let refs = plugins.resolve(&["notes".into()]).await.unwrap();
        let generation = match &refs[0].version {
            horsie_models::runtime::BundleVersion::Generation(g) => g.generation,
            other => panic!("expected a generation, got {other:?}"),
        };
        assert_eq!(generation, 2, "the plugin, then the skill");

        let bytes = plugins.package("notes", &refs[0].version).await.unwrap();
        assert_eq!(
            crate::plugins::sha256_hex_for_test(&bytes),
            refs[0].digest,
            "the digest a runtime checks against is the digest of what it is served"
        );

        // A generation this plugin is not at is refused rather than quietly
        // answered with the current one, whose digest the caller was never told.
        let stale = horsie_models::runtime::BundleVersion::Generation(
            horsie_models::runtime::BundleGeneration { generation: 1 },
        );
        assert!(plugins.package("notes", &stale).await.is_err());
    }
}
