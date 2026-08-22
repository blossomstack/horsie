//! An authored plugin, from the rows an agent writes to the tree a runtime
//! reads.
//!
//! This is the leg that proves delivery is genuinely unchanged: the server
//! renders and packs, the runtime's own `PluginStore` fetches, verifies and
//! unpacks, and what comes out the far end is a skill directory indistinguishable
//! from one that arrived by clone. Nothing here is authored-specific except the
//! source of the bytes, which is the point.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use horsie_models::plugins::AuthoredSkillWriteInput;
use horsie_models::runtime::{BundleRef, BundleVersion};
use horsie_server::db::testing;
use horsie_server::plugins::authored::{AuthoredService, AuthoredStore};
use horsie_server::plugins::{
    ArtifactStore, MarketplaceStore, PluginProvisioner, PluginService, PluginStore,
};
use horsie_server::projects::ProjectId;
use std::sync::Arc;

/// Serves bundles straight out of the server's `package`, which is what the
/// HTTP route does once it has checked the caller.
struct DirectBundles(Arc<PluginService>);

#[async_trait::async_trait]
impl horsie_runtime::plugin_store::BundleSource for DirectBundles {
    async fn fetch(&self, bundle: &BundleRef) -> Result<Vec<u8>, String> {
        self.0.package(&bundle.name, &bundle.version).await
    }
}

async fn services() -> (AuthoredService, Arc<PluginService>, tempfile::TempDir) {
    let db = testing::db().await;
    let project = ProjectId::new("p");
    let tmp = tempfile::tempdir().unwrap();
    let plugins = Arc::new(PluginService::new(
        PluginStore::new(db.clone(), project.clone()),
        MarketplaceStore::new(db.clone(), project.clone()),
        Arc::new(ArtifactStore::new(tmp.path().join("artifacts"))),
        AuthoredStore::new(db.clone(), project.clone()),
    ));
    let authored = AuthoredService::new(AuthoredStore::new(db, project), plugins.clone());
    (authored, plugins, tmp)
}

#[tokio::test]
async fn a_skill_written_by_an_agent_lands_in_a_runtimes_tree() {
    let (authored, plugins, _tmp) = services().await;
    authored
        .write_plugin("field-notes", Some("what I worked out"))
        .await
        .unwrap();
    authored
        .write_skill(AuthoredSkillWriteInput {
            plugin: "field-notes".into(),
            name: "rolling-back".into(),
            description: Some("How to roll back a bad deploy".into()),
            body: Some("1. Stop the rollout.\n2. Re-point the alias.".into()),
            files: Some(vec![horsie_models::plugins::AuthoredFileView {
                path: "scripts/rollback.sh".into(),
                content: "#!/bin/sh\necho rolling back".into(),
            }]),
        })
        .await
        .unwrap();

    // Provisioning: the server names the bundle by generation and hands over a
    // digest of the bytes it will serve.
    let refs = plugins.resolve(&["field-notes".into()]).await.unwrap();
    assert_eq!(refs.len(), 1);
    assert!(
        matches!(refs[0].version, BundleVersion::Generation(_)),
        "an authored bundle is named by its generation"
    );

    // The runtime side, unmodified: fetch, verify against the digest, unpack,
    // link into this agent's tree.
    let root = tempfile::tempdir().unwrap();
    let store = horsie_runtime::plugin_store::PluginStore::new(root.path().to_path_buf());
    let dir = store
        .provision_agent("agent-1", &refs, &DirectBundles(plugins.clone()))
        .await
        .unwrap();

    let skill = dir.join("field-notes/skills/rolling-back/SKILL.md");
    let content =
        std::fs::read_to_string(&skill).unwrap_or_else(|e| panic!("{}: {e}", skill.display()));
    let (name, description) = horsie_support::plugin::skills::parse(&content)
        .expect("the runtime must be able to read what the server rendered");
    assert_eq!(name, "rolling-back");
    assert_eq!(description, "How to roll back a bad deploy");
    assert_eq!(
        std::fs::read_to_string(dir.join("field-notes/skills/rolling-back/scripts/rollback.sh"))
            .unwrap(),
        "#!/bin/sh\necho rolling back",
        "a skill's own files travel with it"
    );

    // The manifest is the portable dialect, so the same tree is readable by any
    // conformant client — not just by this one.
    let manifest = std::fs::read_to_string(dir.join("field-notes/plugin.json")).unwrap();
    assert!(
        manifest.contains("agent-plugins.org/schemas/1.0.0/plugin.schema.json"),
        "{manifest}"
    );
}

/// An edit changes the generation, so the tree is rebuilt rather than served
/// out of the entry the last generation left behind.
#[tokio::test]
async fn editing_a_skill_reprovisions_a_new_tree() {
    let (authored, plugins, _tmp) = services().await;
    authored.write_plugin("notes", None).await.unwrap();
    authored
        .write_skill(AuthoredSkillWriteInput {
            plugin: "notes".into(),
            name: "x".into(),
            description: Some("first".into()),
            body: Some("one".into()),
            files: None,
        })
        .await
        .unwrap();

    let root = tempfile::tempdir().unwrap();
    let store = horsie_runtime::plugin_store::PluginStore::new(root.path().to_path_buf());
    let source = DirectBundles(plugins.clone());

    let before = plugins.resolve(&["notes".into()]).await.unwrap();
    let dir = store
        .provision_agent("agent-1", &before, &source)
        .await
        .unwrap();
    assert!(
        std::fs::read_to_string(dir.join("notes/skills/x/SKILL.md"))
            .unwrap()
            .contains("one")
    );

    authored
        .write_skill(AuthoredSkillWriteInput {
            plugin: "notes".into(),
            name: "x".into(),
            description: None,
            body: Some("two".into()),
            files: None,
        })
        .await
        .unwrap();

    let after = plugins.resolve(&["notes".into()]).await.unwrap();
    assert_ne!(
        after[0].digest, before[0].digest,
        "different rows must render to different bytes"
    );
    let dir = store
        .provision_agent("agent-1", &after, &source)
        .await
        .unwrap();
    assert!(
        std::fs::read_to_string(dir.join("notes/skills/x/SKILL.md"))
            .unwrap()
            .contains("two"),
        "the agent reads the edit, not the tree the previous generation left"
    );
}
