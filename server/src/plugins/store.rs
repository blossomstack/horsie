//! SQLite storage for the plugin-bundle library (`plugins` table), sharing the
//! config store's pool. No secrets — bundles are public artifacts, so this is a
//! plain metadata store (mirrors `github::store` without the `Secret` wrapping).

use sqlx::Row;
use sqlx::sqlite::{SqlitePool, SqliteRow};
use std::collections::HashSet;

const COLS: &str = "name, source_kind, source_url, source_ref, version, description, \
     skill_count, has_hooks, artifact_hash, artifact_size, enabled_default, created_at, updated_at";

/// One row of the `plugins` table.
#[derive(Clone, Debug, PartialEq)]
pub struct PluginRow {
    pub name: String,
    pub source_kind: String,
    pub source_url: String,
    pub source_ref: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    pub skill_count: u32,
    pub has_hooks: bool,
    pub artifact_hash: String,
    pub artifact_size: u64,
    pub enabled_default: bool,
    pub created_at: String,
    pub updated_at: String,
}

pub struct PluginStore {
    pool: SqlitePool,
}

impl PluginStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn list(&self) -> Result<Vec<PluginRow>, String> {
        let rows = sqlx::query(&format!("SELECT {COLS} FROM plugins ORDER BY name"))
            .fetch_all(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        rows.iter().map(row_to_plugin).collect()
    }

    pub async fn get(&self, name: &str) -> Result<Option<PluginRow>, String> {
        let row = sqlx::query(&format!("SELECT {COLS} FROM plugins WHERE name = ?"))
            .bind(name)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_plugin).transpose()
    }

    /// Insert or replace a bundle by name.
    pub async fn upsert(&self, row: &PluginRow) -> Result<(), String> {
        sqlx::query(
            "INSERT INTO plugins (name, source_kind, source_url, source_ref, version, description, \
             skill_count, has_hooks, artifact_hash, artifact_size, enabled_default, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(name) DO UPDATE SET source_kind = excluded.source_kind, \
             source_url = excluded.source_url, source_ref = excluded.source_ref, \
             version = excluded.version, description = excluded.description, \
             skill_count = excluded.skill_count, has_hooks = excluded.has_hooks, \
             artifact_hash = excluded.artifact_hash, artifact_size = excluded.artifact_size, \
             enabled_default = excluded.enabled_default, updated_at = excluded.updated_at",
        )
        .bind(&row.name)
        .bind(&row.source_kind)
        .bind(&row.source_url)
        .bind(&row.source_ref)
        .bind(&row.version)
        .bind(&row.description)
        .bind(i64::from(row.skill_count))
        .bind(i64::from(row.has_hooks))
        .bind(&row.artifact_hash)
        .bind(i64::try_from(row.artifact_size).unwrap_or(i64::MAX))
        .bind(i64::from(row.enabled_default))
        .bind(&row.created_at)
        .bind(&row.updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn set_default(&self, name: &str, enabled: bool) -> Result<(), String> {
        sqlx::query("UPDATE plugins SET enabled_default = ? WHERE name = ?")
            .bind(i64::from(enabled))
            .bind(name)
            .execute(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn delete(&self, name: &str) -> Result<(), String> {
        sqlx::query("DELETE FROM plugins WHERE name = ?")
            .bind(name)
            .execute(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// All artifact hashes still referenced by a row (for artifact GC).
    pub async fn referenced_hashes(&self) -> Result<HashSet<String>, String> {
        let rows = sqlx::query("SELECT artifact_hash FROM plugins")
            .fetch_all(&self.pool)
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

fn row_to_plugin(row: &SqliteRow) -> Result<PluginRow, String> {
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
        version: get_os("version")?,
        description: get_os("description")?,
        skill_count: u32::try_from(get_i("skill_count")?).unwrap_or(0),
        has_hooks: get_i("has_hooks")? != 0,
        artifact_hash: get_s("artifact_hash")?,
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
    use std::str::FromStr;

    async fn store() -> (PluginStore, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}/t.db", tmp.path().display());
        let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
            .unwrap()
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePool::connect_with(opts).await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        (PluginStore::new(pool), tmp)
    }

    fn row(name: &str, hash: &str) -> PluginRow {
        PluginRow {
            name: name.into(),
            source_kind: "git".into(),
            source_url: "https://example.com/x".into(),
            source_ref: None,
            version: Some("1.0.0".into()),
            description: Some("d".into()),
            skill_count: 2,
            has_hooks: true,
            artifact_hash: hash.into(),
            artifact_size: 123,
            enabled_default: false,
            created_at: "1".into(),
            updated_at: "1".into(),
        }
    }

    #[tokio::test]
    async fn upsert_get_list_default_delete_roundtrip() {
        let (s, _t) = store().await;
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
