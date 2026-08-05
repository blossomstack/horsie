//! Storage for registered marketplaces (`marketplaces` table), sharing the
//! config store's database. The parsed index is cached in `entries` as JSON:
//! browsing a 276-entry catalogue is then a local read, and a refresh is what
//! puts a git clone back on the path.

use crate::db::Db;
use horsie_support::plugin::MarketplaceEntry;
use sqlx::Row;
use sqlx::any::AnyRow;

const COLS: &str = "name, source_url, source_ref, sha, entries, skipped, created_at, updated_at";

/// One row of the `marketplaces` table, with `entries`/`skipped` already parsed.
#[derive(Clone, Debug)]
pub struct MarketplaceRow {
    pub name: String,
    pub source_url: String,
    pub source_ref: Option<String>,
    /// HEAD when the index was last read.
    pub sha: Option<String>,
    pub entries: Vec<MarketplaceEntry>,
    /// Human-readable reasons for entries that could not be understood.
    pub skipped: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

pub struct MarketplaceStore {
    db: Db,
}

impl MarketplaceStore {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    pub async fn list(&self) -> Result<Vec<MarketplaceRow>, String> {
        let sql = self
            .db
            .q(&format!("SELECT {COLS} FROM marketplaces ORDER BY name"));
        let rows = sqlx::query(&sql)
            .fetch_all(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        rows.iter().map(row_to_marketplace).collect()
    }

    pub async fn get(&self, name: &str) -> Result<Option<MarketplaceRow>, String> {
        let sql = self
            .db
            .q(&format!("SELECT {COLS} FROM marketplaces WHERE name = ?"));
        let row = sqlx::query(&sql)
            .bind(name)
            .fetch_optional(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_marketplace).transpose()
    }

    /// Insert or replace a marketplace by name.
    pub async fn upsert(&self, row: &MarketplaceRow) -> Result<(), String> {
        let entries = serde_json::to_string(&row.entries).map_err(|e| e.to_string())?;
        let skipped = serde_json::to_string(&row.skipped).map_err(|e| e.to_string())?;
        let sql = self.db.q(
            "INSERT INTO marketplaces (name, source_url, source_ref, sha, entries, skipped, \
             created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(name) DO UPDATE SET source_url = excluded.source_url, \
             source_ref = excluded.source_ref, sha = excluded.sha, \
             entries = excluded.entries, skipped = excluded.skipped, \
             updated_at = excluded.updated_at",
        );
        sqlx::query(&sql)
            .bind(&row.name)
            .bind(&row.source_url)
            .bind(&row.source_ref)
            .bind(&row.sha)
            .bind(&entries)
            .bind(&skipped)
            .bind(&row.created_at)
            .bind(&row.updated_at)
            .execute(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn delete(&self, name: &str) -> Result<(), String> {
        let sql = self.db.q("DELETE FROM marketplaces WHERE name = ?");
        sqlx::query(&sql)
            .bind(name)
            .execute(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// A cache this parser cannot read is reported *on the row* rather than failing
/// the read: one stale row must not take the marketplace list down with it, and
/// the operator needs to be told that the fix is a refresh.
fn row_to_marketplace(row: &AnyRow) -> Result<MarketplaceRow, String> {
    let get_s = |c: &str| row.try_get::<String, _>(c).map_err(|e| e.to_string());
    let get_os = |c: &str| {
        row.try_get::<Option<String>, _>(c)
            .map_err(|e| e.to_string())
    };
    let raw_entries = get_s("entries")?;
    let mut skipped: Vec<String> = serde_json::from_str(&get_s("skipped")?).unwrap_or_default();
    let entries = match serde_json::from_str::<Vec<MarketplaceEntry>>(&raw_entries) {
        Ok(e) => e,
        Err(e) => {
            skipped.push(format!(
                "the cached index could not be read ({e}) — refresh this marketplace"
            ));
            Vec::new()
        }
    };
    Ok(MarketplaceRow {
        name: get_s("name")?,
        source_url: get_s("source_url")?,
        source_ref: get_os("source_ref")?,
        sha: get_os("sha")?,
        entries,
        skipped,
        created_at: get_s("created_at")?,
        updated_at: get_s("updated_at")?,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::db::testing;
    use horsie_support::plugin::PluginSource;

    fn fixture() -> MarketplaceRow {
        MarketplaceRow {
            name: "official".into(),
            source_url: "https://example.com/market.git".into(),
            source_ref: None,
            sha: Some("abc123".into()),
            entries: vec![
                MarketplaceEntry {
                    name: "alpha".into(),
                    description: Some("the first".into()),
                    version: None,
                    source: PluginSource::Path("./plugins/alpha".into()),
                },
                MarketplaceEntry {
                    name: "beta".into(),
                    description: None,
                    version: Some("2.0".into()),
                    source: PluginSource::Git {
                        url: "https://example.com/beta.git".into(),
                        path: None,
                        git_ref: Some("v2".into()),
                    },
                },
            ],
            skipped: vec!["entry 2: missing 'source'".into()],
            created_at: "1".into(),
            updated_at: "1".into(),
        }
    }

    #[tokio::test]
    async fn entries_round_trip_through_json() {
        let s = MarketplaceStore::new(testing::db().await);
        assert!(s.list().await.unwrap().is_empty());
        s.upsert(&fixture()).await.unwrap();
        let got = s.get("official").await.unwrap().unwrap();
        assert_eq!(got.entries.len(), 2);
        assert_eq!(got.entries[1].name, "beta");
        assert_eq!(
            got.entries[0].source,
            PluginSource::Path("./plugins/alpha".into()),
            "the source shape must survive the cache, not just the name"
        );
        assert_eq!(got.skipped, vec!["entry 2: missing 'source'".to_string()]);

        s.delete("official").await.unwrap();
        assert!(s.get("official").await.unwrap().is_none());
    }

    /// A cache written by an older parser must not brick the list endpoint.
    #[tokio::test]
    async fn an_unreadable_cache_reports_itself_instead_of_failing() {
        let db = testing::db().await;
        let s = MarketplaceStore::new(db.clone());
        s.upsert(&fixture()).await.unwrap();
        sqlx::query(&db.q("UPDATE marketplaces SET entries = ? WHERE name = ?"))
            .bind("{not json")
            .bind("official")
            .execute(db.pool())
            .await
            .unwrap();

        let got = s.get("official").await.unwrap().unwrap();
        assert!(got.entries.is_empty());
        assert!(
            got.skipped.iter().any(|s| s.contains("refresh")),
            "must tell the operator what to do: {:?}",
            got.skipped
        );
    }
}
