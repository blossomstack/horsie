//! Storage for the plugin-bundle library (`plugins` table), sharing the config
//! store's database. No secrets — bundles are public artifacts, so this is a
//! plain metadata store (mirrors `github::store` without the `Secret` wrapping).

use crate::db::Db;
use sqlx::Row;
use sqlx::any::AnyRow;
use std::collections::HashSet;

const COLS: &str = "name, source_kind, source_url, source_ref, source_subpath, version, \
     description, skill_count, has_hooks, artifact_hash, artifact_size, enabled_default, \
     marketplace, marketplace_entry, created_at, updated_at";

/// One row of the `plugins` table.
#[derive(Clone, Debug, PartialEq)]
pub struct PluginRow {
    pub name: String,
    pub source_kind: String,
    pub source_url: String,
    pub source_ref: Option<String>,
    /// Where inside the checkout the plugin root sat. `None` means the repo
    /// root, which is what a plain bundle repo means.
    pub source_subpath: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    pub skill_count: u32,
    pub has_hooks: bool,
    pub artifact_hash: String,
    pub artifact_size: u64,
    pub enabled_default: bool,
    /// The marketplace this bundle came through, and that marketplace's name
    /// for it. Both or neither.
    pub marketplace: Option<String>,
    pub marketplace_entry: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

pub struct PluginStore {
    db: Db,
}

impl PluginStore {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    pub async fn list(&self) -> Result<Vec<PluginRow>, String> {
        let statement = format!("SELECT {COLS} FROM plugins ORDER BY name");
        let sql = self.db.q(&statement);
        let rows = sqlx::query(&sql)
            .fetch_all(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        rows.iter().map(row_to_plugin).collect()
    }

    pub async fn get(&self, name: &str) -> Result<Option<PluginRow>, String> {
        let statement = format!("SELECT {COLS} FROM plugins WHERE name = ?");
        let sql = self.db.q(&statement);
        let row = sqlx::query(&sql)
            .bind(name)
            .fetch_optional(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_plugin).transpose()
    }

    /// Insert or replace a bundle by name.
    pub async fn upsert(&self, row: &PluginRow) -> Result<(), String> {
        let sql = self.db.q(
            "INSERT INTO plugins (name, source_kind, source_url, source_ref, source_subpath, \
             version, description, skill_count, has_hooks, artifact_hash, artifact_size, \
             enabled_default, marketplace, marketplace_entry, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(name) DO UPDATE SET source_kind = excluded.source_kind, \
             source_url = excluded.source_url, source_ref = excluded.source_ref, \
             source_subpath = excluded.source_subpath, \
             version = excluded.version, description = excluded.description, \
             skill_count = excluded.skill_count, has_hooks = excluded.has_hooks, \
             artifact_hash = excluded.artifact_hash, artifact_size = excluded.artifact_size, \
             enabled_default = excluded.enabled_default, marketplace = excluded.marketplace, \
             marketplace_entry = excluded.marketplace_entry, updated_at = excluded.updated_at",
        );
        sqlx::query(&sql)
            .bind(&row.name)
            .bind(&row.source_kind)
            .bind(&row.source_url)
            .bind(&row.source_ref)
            .bind(&row.source_subpath)
            .bind(&row.version)
            .bind(&row.description)
            .bind(i64::from(row.skill_count))
            .bind(i64::from(row.has_hooks))
            .bind(&row.artifact_hash)
            .bind(i64::try_from(row.artifact_size).unwrap_or(i64::MAX))
            .bind(i64::from(row.enabled_default))
            .bind(&row.marketplace)
            .bind(&row.marketplace_entry)
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
            .q("UPDATE plugins SET enabled_default = ? WHERE name = ?");
        sqlx::query(&sql)
            .bind(i64::from(enabled))
            .bind(name)
            .execute(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn delete(&self, name: &str) -> Result<(), String> {
        let sql = self.db.q("DELETE FROM plugins WHERE name = ?");
        sqlx::query(&sql)
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
             WHERE marketplace = ? AND marketplace_entry IS NOT NULL");
        let rows = sqlx::query(&sql)
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

    /// All artifact hashes still referenced by a row (for artifact GC).
    pub async fn referenced_hashes(&self) -> Result<HashSet<String>, String> {
        let sql = self.db.q("SELECT artifact_hash FROM plugins");
        let rows = sqlx::query(&sql)
            .fetch_all(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        rows.iter()
            .map(|r| {
                r.try_get::<String, _>("artifact_hash")
                    .map_err(|e| e.to_string())
            })
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
    Ok(PluginRow {
        name: get_s("name")?,
        source_kind: get_s("source_kind")?,
        source_url: get_s("source_url")?,
        source_ref: get_os("source_ref")?,
        source_subpath: get_os("source_subpath")?,
        version: get_os("version")?,
        description: get_os("description")?,
        skill_count: u32::try_from(get_i("skill_count")?).unwrap_or(0),
        has_hooks: get_i("has_hooks")? != 0,
        artifact_hash: get_s("artifact_hash")?,
        artifact_size: u64::try_from(get_i("artifact_size")?).unwrap_or(0),
        enabled_default: get_i("enabled_default")? != 0,
        marketplace: get_os("marketplace")?,
        marketplace_entry: get_os("marketplace_entry")?,
        created_at: get_s("created_at")?,
        updated_at: get_s("updated_at")?,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::db::testing;

    fn row(name: &str, hash: &str) -> PluginRow {
        PluginRow {
            name: name.into(),
            source_kind: "git".into(),
            source_url: "https://example.com/x".into(),
            source_ref: None,
            source_subpath: None,
            version: Some("1.0.0".into()),
            description: Some("d".into()),
            skill_count: 2,
            has_hooks: true,
            artifact_hash: hash.into(),
            artifact_size: 123,
            enabled_default: false,
            marketplace: None,
            marketplace_entry: None,
            created_at: "1".into(),
            updated_at: "1".into(),
        }
    }

    #[tokio::test]
    async fn upsert_get_list_default_delete_roundtrip() {
        {
            let s = PluginStore::new(testing::db().await);
            assert!(s.list().await.unwrap().is_empty());
            s.upsert(&row("demo", "h1")).await.unwrap();
            let got = s.get("demo").await.unwrap().unwrap();
            assert_eq!(got.skill_count, 2);
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
        let s = PluginStore::new(testing::db().await);
        let mut r = row("api-security-testing", "h1");
        r.marketplace = Some("official".into());
        // The index's name for an entry is not always the name it installs as.
        r.marketplace_entry = Some("42crunch-api-security-testing".into());
        r.source_subpath = Some("./plugins/api".into());
        s.upsert(&r).await.unwrap();
        s.upsert(&row("plain", "h2")).await.unwrap();

        let got = s.get("api-security-testing").await.unwrap().unwrap();
        assert_eq!(got.marketplace.as_deref(), Some("official"));
        assert_eq!(got.source_subpath.as_deref(), Some("./plugins/api"));

        let entries = s.installed_entries("official").await.unwrap();
        assert!(entries.contains("42crunch-api-security-testing"));
        assert_eq!(entries.len(), 1, "a plain install must not appear");
    }
}
