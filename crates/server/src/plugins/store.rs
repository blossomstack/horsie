//! Storage for the plugin-bundle library (`plugins` table), sharing the config
//! store's database. No secrets — bundles are public artifacts, so this is a
//! plain metadata store (mirrors `github::store` without the `Secret` wrapping).

use crate::db::Db;
use crate::plugins::kind;
use horsie_models::plugins::PluginKind;
use crate::projects::ProjectId;
use sqlx::Row;
use sqlx::any::AnyRow;
use std::collections::HashSet;

const COLS: &str = "name, source_kind, source_url, source_ref, source_subpath, generation, \
     version, description, catalog, has_hooks, digest, artifact_size, enabled_default, \
     marketplace, marketplace_entry, created_at, updated_at";

/// One row of the `plugins` table.
#[derive(Clone, Debug, PartialEq)]
pub struct PluginRow {
    pub name: String,
    /// What this bundle is: cloned in one of two layouts, or authored here.
    /// Stored as a discriminant plus the column group its arm uses.
    pub kind: PluginKind,
    pub version: Option<String>,
    pub description: Option<String>,
    /// Everything this bundle offers, derived at ingest. Empty when the column
    /// is NULL (a row installed before catalogues existed) or unreadable — the
    /// service backfills the first, and the second must degrade to "no entries"
    /// rather than fail a list nobody could then repair.
    pub catalog: Vec<horsie_support::plugin::catalog::CatalogEntry>,
    pub has_hooks: bool,
    /// sha256 of the packed bytes. For an external bundle this is also its
    /// identity — it is what the artifact on disk is named by. For an authored
    /// one it is only an integrity check on a package the server re-renders.
    pub digest: String,
    pub artifact_size: u64,
    pub enabled_default: bool,
    pub created_at: String,
    pub updated_at: String,
}

pub struct PluginStore {
    db: Db,
    /// Bound once, here, rather than passed per call.
    user: ProjectId,
}

impl PluginStore {
    pub fn new(db: Db, user: ProjectId) -> Self {
        Self { db, user }
    }

    pub async fn list(&self) -> Result<Vec<PluginRow>, String> {
        let statement = format!("SELECT {COLS} FROM plugins WHERE project_id = ? ORDER BY name");
        let sql = self.db.q(&statement);
        let rows = sqlx::query(&sql)
            .bind(self.user.as_str())
            .fetch_all(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        rows.iter().map(row_to_plugin).collect()
    }

    pub async fn get(&self, name: &str) -> Result<Option<PluginRow>, String> {
        let statement = format!("SELECT {COLS} FROM plugins WHERE project_id = ? AND name = ?");
        let sql = self.db.q(&statement);
        let row = sqlx::query(&sql)
            .bind(self.user.as_str())
            .bind(name)
            .fetch_optional(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_plugin).transpose()
    }

    /// Insert or replace a bundle by name.
    pub async fn upsert(&self, row: &PluginRow) -> Result<(), String> {
        let sql = self.db.q(
            "INSERT INTO plugins (project_id, name, source_kind, source_url, source_ref, source_subpath, \
             generation, version, description, catalog, has_hooks, digest, artifact_size, \
             enabled_default, marketplace, marketplace_entry, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(project_id, name) DO UPDATE SET source_kind = excluded.source_kind, \
             source_url = excluded.source_url, source_ref = excluded.source_ref, \
             source_subpath = excluded.source_subpath, generation = excluded.generation, \
             version = excluded.version, description = excluded.description, \
             catalog = excluded.catalog, has_hooks = excluded.has_hooks, \
             digest = excluded.digest, artifact_size = excluded.artifact_size, \
             enabled_default = excluded.enabled_default, marketplace = excluded.marketplace, \
             marketplace_entry = excluded.marketplace_entry, updated_at = excluded.updated_at",
        );
        let ext = kind::external(&row.kind);
        sqlx::query(&sql)
            .bind(self.user.as_str())
            .bind(&row.name)
            .bind(kind::tag(&row.kind))
            .bind(ext.map(|e| e.url.clone()))
            .bind(ext.and_then(|e| e.git_ref.clone()))
            .bind(ext.and_then(|e| e.subpath.clone()))
            .bind(kind::generation(&row.kind).map(|g| i64::try_from(g).unwrap_or(i64::MAX)))
            .bind(&row.version)
            .bind(&row.description)
            .bind(serde_json::to_string(&row.catalog).unwrap_or_else(|_| "[]".to_string()))
            .bind(i64::from(row.has_hooks))
            .bind(&row.digest)
            .bind(i64::try_from(row.artifact_size).unwrap_or(i64::MAX))
            .bind(i64::from(row.enabled_default))
            .bind(ext.and_then(|e| e.marketplace.clone()))
            .bind(ext.and_then(|e| e.marketplace_entry.clone()))
            .bind(&row.created_at)
            .bind(&row.updated_at)
            .execute(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn set_default(&self, name: &str, enabled: bool) -> Result<(), String> {
        let sql = self
            .db
            .q("UPDATE plugins SET enabled_default = ? WHERE project_id = ? AND name = ?");
        sqlx::query(&sql)
            .bind(i64::from(enabled))
            .bind(self.user.as_str())
            .bind(name)
            .execute(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn delete(&self, name: &str) -> Result<(), String> {
        let sql = self
            .db
            .q("DELETE FROM plugins WHERE project_id = ? AND name = ?");
        sqlx::query(&sql)
            .bind(self.user.as_str())
            .bind(name)
            .execute(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Entry names of bundles installed from `marketplace`, so the picker can
    /// mark them rather than offering them again.
    pub async fn installed_entries(&self, marketplace: &str) -> Result<HashSet<String>, String> {
        let sql = self.db.q("SELECT marketplace_entry FROM plugins \
             WHERE project_id = ? AND marketplace = ? AND marketplace_entry IS NOT NULL");
        let rows = sqlx::query(&sql)
            .bind(self.user.as_str())
            .bind(marketplace)
            .fetch_all(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                r.try_get::<Option<String>, _>("marketplace_entry")
                    .ok()
                    .flatten()
            })
            .collect())
    }

    /// Every artifact digest any account still references.
    ///
    /// Deliberately NOT scoped, and it must stay that way. Artifacts are
    /// content-addressed and therefore shared between accounts, so a scoped
    /// keep-set would make GC delete bundle bytes another account is still
    /// using. On the scope audit's allowlist for exactly this reason.
    ///
    /// Authored bundles are excluded: they have no artifact on disk at all, so
    /// naming their digests here would be claiming a file that does not exist.
    pub async fn referenced_hashes(&self) -> Result<HashSet<String>, String> {
        let sql = self
            .db
            .q("SELECT digest FROM plugins WHERE source_kind != ?");
        let rows = sqlx::query(&sql)
            .bind(kind::AUTHORED)
            .fetch_all(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        rows.iter()
            .map(|r| r.try_get::<String, _>("digest").map_err(|e| e.to_string()))
            .collect()
    }
}

fn row_to_plugin(row: &AnyRow) -> Result<PluginRow, String> {
    let get_s = |c: &str| row.try_get::<String, _>(c).map_err(|e| e.to_string());
    let get_os = |c: &str| {
        row.try_get::<Option<String>, _>(c)
            .map_err(|e| e.to_string())
    };
    let get_i = |c: &str| row.try_get::<i64, _>(c).map_err(|e| e.to_string());
    let get_oi = |c: &str| row.try_get::<Option<i64>, _>(c).map_err(|e| e.to_string());
    Ok(PluginRow {
        name: get_s("name")?,
        kind: kind::from_columns(
            &get_s("source_kind")?,
            get_os("source_url")?,
            get_os("source_ref")?,
            get_os("source_subpath")?,
            get_os("marketplace")?,
            get_os("marketplace_entry")?,
            get_oi("generation")?.map(|g| u64::try_from(g).unwrap_or_default()),
        )?,
        version: get_os("version")?,
        description: get_os("description")?,
        catalog: get_os("catalog")?
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default(),
        has_hooks: get_i("has_hooks")? != 0,
        digest: get_s("digest")?,
        artifact_size: u64::try_from(get_i("artifact_size")?).unwrap_or(0),
        enabled_default: get_i("enabled_default")? != 0,
        created_at: get_s("created_at")?,
        updated_at: get_s("updated_at")?,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::db::testing;
    use horsie_support::plugin::catalog::{CatalogEntry, CatalogKind};

    fn entry(kind: CatalogKind, name: &str) -> CatalogEntry {
        CatalogEntry {
            kind,
            name: name.into(),
            description: "d".into(),
            argument_hint: None,
            template: (kind == CatalogKind::Command).then(|| "body".to_string()),
        }
    }

    fn row(name: &str, hash: &str) -> PluginRow {
        PluginRow {
            name: name.into(),
            kind: horsie_models::plugins::PluginKind::Claude(
                horsie_models::plugins::ExternalOrigin {
                    url: "https://example.com/x.git".into(),
                    git_ref: None,
                    subpath: None,
                    marketplace: None,
                    marketplace_entry: None,
                },
            ),
            version: Some("1.0.0".into()),
            description: Some("d".into()),
            catalog: vec![
                entry(CatalogKind::Command, "commit"),
                entry(CatalogKind::Skill, "tdd"),
            ],
            has_hooks: true,
            digest: hash.into(),
            artifact_size: 123,
            enabled_default: false,
            created_at: "1".into(),
            updated_at: "1".into(),
        }
    }

    #[tokio::test]
    async fn upsert_get_list_default_delete_roundtrip() {
        {
            let s = PluginStore::new(testing::db().await, ProjectId::new("1"));
            assert!(s.list().await.unwrap().is_empty());
            s.upsert(&row("demo", "h1")).await.unwrap();
            let got = s.get("demo").await.unwrap().unwrap();
            assert_eq!(
                got.catalog,
                row("demo", "h1").catalog,
                "the catalogue survives the round trip"
            );
            assert!(got.has_hooks);
            assert!(!got.enabled_default);

            s.set_default("demo", true).await.unwrap();
            assert!(s.get("demo").await.unwrap().unwrap().enabled_default);

            s.upsert(&row("other", "h2")).await.unwrap();
            let hashes = s.referenced_hashes().await.unwrap();
            assert!(hashes.contains("h1") && hashes.contains("h2"));

            s.delete("demo").await.unwrap();
            assert!(s.get("demo").await.unwrap().is_none());
            assert_eq!(s.list().await.unwrap().len(), 1);
        }
    }

    /// Provenance survives a round trip, and only marketplace-installed bundles
    /// show up as installed entries — a plain install must not mark a catalogue
    /// entry it never came from.
    #[tokio::test]
    async fn provenance_survives_and_lists_installed_entries() {
        let s = PluginStore::new(testing::db().await, ProjectId::new("1"));
        let mut r = row("api-security-testing", "h1");
        r.kind = horsie_models::plugins::PluginKind::Claude(
            horsie_models::plugins::ExternalOrigin {
                url: "https://example.com/x.git".into(),
                git_ref: None,
                subpath: Some("./plugins/api".into()),
                marketplace: Some("official".into()),
                // The index's name for an entry is not always the name it
                // installs as.
                marketplace_entry: Some("42crunch-api-security-testing".into()),
            },
        );
        s.upsert(&r).await.unwrap();
        s.upsert(&row("plain", "h2")).await.unwrap();

        let got = s.get("api-security-testing").await.unwrap().unwrap();
        let origin = crate::plugins::kind::external(&got.kind).expect("a cloned bundle");
        assert_eq!(origin.marketplace.as_deref(), Some("official"));
        assert_eq!(origin.subpath.as_deref(), Some("./plugins/api"));

        let entries = s.installed_entries("official").await.unwrap();
        assert!(entries.contains("42crunch-api-security-testing"));
        assert_eq!(entries.len(), 1, "a plain install must not appear");
    }
}
