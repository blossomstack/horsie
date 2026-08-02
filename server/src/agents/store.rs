//! SQLite storage for agent presets, sharing the config store's pool.
//! List-typed columns are JSON; `AgentRepo` is the storage twin of the wire
//! `session_api::RepoConfig` (protocol types are not storage types).

use sqlx::Row;
use sqlx::sqlite::{SqlitePool, SqliteRow};

const COLS: &str = "name, description, vendor, model, repos, plugins, \
                    mcp_servers, memory_spaces, thinking_effort, created_at, updated_at";

/// One repo to clone at provision time (storage twin of wire `RepoConfig`).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AgentRepo {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

/// One row of the `agents` table.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentRow {
    pub name: String,
    pub description: String,
    pub vendor: Option<String>,
    pub model: String,
    pub repos: Vec<AgentRepo>,
    pub plugins: Vec<String>,
    pub mcp_servers: Vec<String>,
    pub memory_spaces: Vec<String>,
    pub thinking_effort: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

pub struct AgentStore {
    pool: SqlitePool,
}

impl AgentStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn list(&self) -> Result<Vec<AgentRow>, String> {
        let rows = sqlx::query(&format!("SELECT {COLS} FROM agents ORDER BY name"))
            .fetch_all(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        rows.iter().map(row_to_agent).collect()
    }

    pub async fn get(&self, name: &str) -> Result<Option<AgentRow>, String> {
        let row = sqlx::query(&format!("SELECT {COLS} FROM agents WHERE name = ?"))
            .bind(name)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_agent).transpose()
    }

    /// Insert; errs when the name is taken (no upsert -- a silent overwrite
    /// would discard the existing preset).
    pub async fn insert(&self, row: &AgentRow) -> Result<(), String> {
        sqlx::query(&format!(
            "INSERT INTO agents ({COLS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        ))
        .bind(&row.name)
        .bind(&row.description)
        .bind(&row.vendor)
        .bind(&row.model)
        .bind(to_json(&row.repos)?)
        .bind(to_json(&row.plugins)?)
        .bind(to_json(&row.mcp_servers)?)
        .bind(to_json(&row.memory_spaces)?)
        .bind(&row.thinking_effort)
        .bind(&row.created_at)
        .bind(&row.updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("create agent '{}': {e}", row.name))?;
        Ok(())
    }

    /// Full replace. Returns false when no agent has that name.
    pub async fn replace(&self, row: &AgentRow) -> Result<bool, String> {
        let res = sqlx::query(
            "UPDATE agents SET description = ?, vendor = ?, model = ?, repos = ?, \
             plugins = ?, mcp_servers = ?, memory_spaces = ?, thinking_effort = ?, \
             updated_at = ? WHERE name = ?",
        )
        .bind(&row.description)
        .bind(&row.vendor)
        .bind(&row.model)
        .bind(to_json(&row.repos)?)
        .bind(to_json(&row.plugins)?)
        .bind(to_json(&row.mcp_servers)?)
        .bind(to_json(&row.memory_spaces)?)
        .bind(&row.thinking_effort)
        .bind(&row.updated_at)
        .bind(&row.name)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn delete(&self, name: &str) -> Result<bool, String> {
        let res = sqlx::query("DELETE FROM agents WHERE name = ?")
            .bind(name)
            .execute(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(res.rows_affected() > 0)
    }
}

fn to_json<T: serde::Serialize>(v: &T) -> Result<String, String> {
    serde_json::to_string(v).map_err(|e| e.to_string())
}

fn from_json<T: serde::de::DeserializeOwned>(col: &str, text: String) -> Result<T, String> {
    serde_json::from_str(&text).map_err(|e| format!("agents.{col}: {e}"))
}

fn row_to_agent(row: &SqliteRow) -> Result<AgentRow, String> {
    let get = |c: &str| row.try_get::<String, _>(c).map_err(|e| e.to_string());
    let get_opt = |c: &str| row.try_get::<Option<String>, _>(c).map_err(|e| e.to_string());
    Ok(AgentRow {
        name: get("name")?,
        description: get("description")?,
        vendor: get_opt("vendor")?,
        model: get("model")?,
        repos: from_json("repos", get("repos")?)?,
        plugins: from_json("plugins", get("plugins")?)?,
        mcp_servers: from_json("mcp_servers", get("mcp_servers")?)?,
        memory_spaces: from_json("memory_spaces", get("memory_spaces")?)?,
        thinking_effort: get_opt("thinking_effort")?,
        created_at: get("created_at")?,
        updated_at: get("updated_at")?,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::str::FromStr;

    async fn store() -> (AgentStore, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}/t.db", tmp.path().display());
        let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
            .unwrap()
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePool::connect_with(opts).await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        (AgentStore::new(pool), tmp)
    }

    fn row(name: &str) -> AgentRow {
        AgentRow {
            name: name.into(),
            description: "d".into(),
            vendor: Some("local".into()),
            model: "sonnet".into(),
            repos: vec![AgentRepo {
                url: "https://github.com/o/api".into(),
                git_ref: Some("dev".into()),
                dir: None,
            }],
            plugins: vec!["superpowers".into()],
            mcp_servers: vec![],
            memory_spaces: vec!["default".into()],
            thinking_effort: Some("high".into()),
            created_at: "1".into(),
            updated_at: "1".into(),
        }
    }

    #[tokio::test]
    async fn insert_get_list_roundtrip_including_json_columns() {
        let (s, _t) = store().await;
        s.insert(&row("a")).await.unwrap();
        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got, row("a"));
        assert_eq!(got.repos[0].git_ref.as_deref(), Some("dev"));
        assert_eq!(s.list().await.unwrap().len(), 1);
        assert!(s.get("ghost").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn duplicate_insert_is_rejected() {
        let (s, _t) = store().await;
        s.insert(&row("a")).await.unwrap();
        assert!(s.insert(&row("a")).await.is_err());
    }

    #[tokio::test]
    async fn replace_updates_and_reports_misses() {
        let (s, _t) = store().await;
        assert!(!s.replace(&row("ghost")).await.unwrap());
        s.insert(&row("a")).await.unwrap();
        let mut r = row("a");
        r.description = "new".into();
        r.updated_at = "2".into();
        assert!(s.replace(&r).await.unwrap());
        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got.description, "new");
        assert_eq!(got.updated_at, "2");
        assert_eq!(got.created_at, "1", "replace must not touch created_at");
    }

    #[tokio::test]
    async fn delete_reports_misses() {
        let (s, _t) = store().await;
        s.insert(&row("a")).await.unwrap();
        assert!(s.delete("a").await.unwrap());
        assert!(!s.delete("a").await.unwrap());
    }

    #[tokio::test]
    async fn null_vendor_and_effort_round_trip() {
        let (s, _t) = store().await;
        let mut r = row("a");
        r.vendor = None;
        r.thinking_effort = None;
        s.insert(&r).await.unwrap();
        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got.vendor, None);
        assert_eq!(got.thinking_effort, None);
    }
}
