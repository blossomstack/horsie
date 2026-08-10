//! Storage for agent presets, sharing the config store's database.
//! List-typed columns are JSON.

use crate::auth::UserId;
use crate::db::Db;
use sqlx::Row;
use sqlx::any::AnyRow;

const COLS: &str = "name, description, instructions, model, plugins, \
                    mcp_servers, memory_spaces, thinking_effort, created_at, updated_at";

/// One row of the `agents` table.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentRow {
    pub name: String,
    pub description: String,
    /// Standing instructions the preset's agent runs under. `None` and empty
    /// mean the same thing — no section is added to the system prompt.
    pub instructions: Option<String>,
    pub model: String,
    pub plugins: Vec<String>,
    pub mcp_servers: Vec<String>,
    pub memory_spaces: Vec<String>,
    pub thinking_effort: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

pub struct AgentStore {
    db: Db,
    /// Bound once, here, rather than passed per call.
    user: UserId,
}

impl AgentStore {
    pub fn new(db: Db, user: UserId) -> Self {
        Self { db, user }
    }

    pub async fn list(&self) -> Result<Vec<AgentRow>, String> {
        let rows = sqlx::query(&self.db.q(&format!(
            "SELECT {COLS} FROM agents WHERE user_id = ? ORDER BY name"
        )))
        .bind(self.user.as_str())
        .fetch_all(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        rows.iter().map(row_to_agent).collect()
    }

    pub async fn get(&self, name: &str) -> Result<Option<AgentRow>, String> {
        let row = sqlx::query(&self.db.q(&format!(
            "SELECT {COLS} FROM agents WHERE user_id = ? AND name = ?"
        )))
        .bind(self.user.as_str())
        .bind(name)
        .fetch_optional(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_agent).transpose()
    }

    /// Insert; errs when the name is taken (no upsert -- a silent overwrite
    /// would discard the existing preset).
    pub async fn insert(&self, row: &AgentRow) -> Result<(), String> {
        sqlx::query(&self.db.q(&format!(
            "INSERT INTO agents (user_id, {COLS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        )))
        .bind(self.user.as_str())
        .bind(&row.name)
        .bind(&row.description)
        .bind(&row.instructions)
        .bind(&row.model)
        .bind(to_json(&row.plugins)?)
        .bind(to_json(&row.mcp_servers)?)
        .bind(to_json(&row.memory_spaces)?)
        .bind(&row.thinking_effort)
        .bind(&row.created_at)
        .bind(&row.updated_at)
        .execute(self.db.pool())
        .await
        .map_err(|e| format!("create agent '{}': {e}", row.name))?;
        Ok(())
    }

    /// Full replace. Returns false when no agent has that name.
    pub async fn replace(&self, row: &AgentRow) -> Result<bool, String> {
        let res = sqlx::query(&self.db.q(
            "UPDATE agents SET description = ?, instructions = ?, model = ?, \
             plugins = ?, mcp_servers = ?, memory_spaces = ?, thinking_effort = ?, \
             updated_at = ? WHERE user_id = ? AND name = ?",
        ))
        .bind(&row.description)
        .bind(&row.instructions)
        .bind(&row.model)
        .bind(to_json(&row.plugins)?)
        .bind(to_json(&row.mcp_servers)?)
        .bind(to_json(&row.memory_spaces)?)
        .bind(&row.thinking_effort)
        .bind(&row.updated_at)
        .bind(self.user.as_str())
        .bind(&row.name)
        .execute(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn delete(&self, name: &str) -> Result<bool, String> {
        let res = sqlx::query(
            &self
                .db
                .q("DELETE FROM agents WHERE user_id = ? AND name = ?"),
        )
        .bind(self.user.as_str())
        .bind(name)
        .execute(self.db.pool())
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

fn row_to_agent(row: &AnyRow) -> Result<AgentRow, String> {
    let get = |c: &str| row.try_get::<String, _>(c).map_err(|e| e.to_string());
    let get_opt = |c: &str| {
        row.try_get::<Option<String>, _>(c)
            .map_err(|e| e.to_string())
    };
    Ok(AgentRow {
        name: get("name")?,
        description: get("description")?,
        instructions: get_opt("instructions")?,
        model: get("model")?,
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

    async fn store() -> (AgentStore, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let pool = crate::db::testing::db().await;
        (AgentStore::new(pool, crate::auth::UserId::new("1")), tmp)
    }

    fn row(name: &str) -> AgentRow {
        AgentRow {
            name: name.into(),
            description: "d".into(),
            instructions: None,
            model: "sonnet".into(),
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
    async fn null_effort_round_trips() {
        let (s, _t) = store().await;
        let mut r = row("a");
        r.thinking_effort = None;
        s.insert(&r).await.unwrap();
        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got.thinking_effort, None);
    }
}
