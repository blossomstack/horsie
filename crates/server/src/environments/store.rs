//! Storage for environments, sharing the config store's database.
//! List-typed columns are JSON; the types below are storage twins of the wire
//! `session_api::RepoConfig`, `executor::EnvVar`, and `executor::ProvisionStep`
//! (protocol types are not storage types).

use crate::db::Db;
use crate::projects::ProjectId;
use sqlx::Row;
use sqlx::any::AnyRow;

const COLS: &str = "name, description, vendor, repos, env_vars, provision, created_at, updated_at";

/// One repo to clone at provision time (storage twin of wire `RepoConfig`).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EnvironmentRepo {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

/// One plain-text env var (storage twin of wire `executor::EnvVar`).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EnvironmentEnvVar {
    pub name: String,
    pub value: String,
}

/// One key/value parameter of a provision step (storage twin of `StepParam`).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EnvironmentStepParam {
    pub key: String,
    pub value: String,
}

/// One setup step (storage twin of wire `executor::ProvisionStep`).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EnvironmentProvisionStep {
    pub name: String,
    pub uses: String,
    #[serde(default)]
    pub with: Vec<EnvironmentStepParam>,
}

/// One row of the `environments` table.
#[derive(Clone, Debug, PartialEq)]
pub struct EnvironmentRow {
    pub name: String,
    pub description: String,
    pub vendor: String,
    pub repos: Vec<EnvironmentRepo>,
    pub env_vars: Vec<EnvironmentEnvVar>,
    pub provision: Vec<EnvironmentProvisionStep>,
    pub created_at: String,
    pub updated_at: String,
}

pub struct EnvironmentStore {
    db: Db,
    /// Bound once, here, rather than passed per call.
    user: ProjectId,
}

impl EnvironmentStore {
    pub fn new(db: Db, user: ProjectId) -> Self {
        Self { db, user }
    }

    pub async fn list(&self) -> Result<Vec<EnvironmentRow>, String> {
        let rows = sqlx::query(&self.db.q(&format!(
            "SELECT {COLS} FROM environments WHERE project_id = ? ORDER BY name"
        )))
        .bind(self.user.as_str())
        .fetch_all(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        rows.iter().map(row_to_environment).collect()
    }

    pub async fn get(&self, name: &str) -> Result<Option<EnvironmentRow>, String> {
        let row = sqlx::query(&self.db.q(&format!(
            "SELECT {COLS} FROM environments WHERE project_id = ? AND name = ?"
        )))
        .bind(self.user.as_str())
        .bind(name)
        .fetch_optional(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_environment).transpose()
    }

    /// Insert; errs when the name is taken (no upsert -- a silent overwrite
    /// would discard the existing environment).
    pub async fn insert(&self, row: &EnvironmentRow) -> Result<(), String> {
        sqlx::query(&self.db.q(&format!(
            "INSERT INTO environments (project_id, {COLS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
        )))
        .bind(self.user.as_str())
        .bind(&row.name)
        .bind(&row.description)
        .bind(&row.vendor)
        .bind(to_json(&row.repos)?)
        .bind(to_json(&row.env_vars)?)
        .bind(to_json(&row.provision)?)
        .bind(&row.created_at)
        .bind(&row.updated_at)
        .execute(self.db.pool())
        .await
        .map_err(|e| format!("create environment '{}': {e}", row.name))?;
        Ok(())
    }

    /// Full replace. Returns false when no environment has that name.
    pub async fn replace(&self, row: &EnvironmentRow) -> Result<bool, String> {
        let res = sqlx::query(&self.db.q(
            "UPDATE environments SET description = ?, vendor = ?, repos = ?, \
             env_vars = ?, provision = ?, updated_at = ? WHERE project_id = ? AND name = ?",
        ))
        .bind(&row.description)
        .bind(&row.vendor)
        .bind(to_json(&row.repos)?)
        .bind(to_json(&row.env_vars)?)
        .bind(to_json(&row.provision)?)
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
                .q("DELETE FROM environments WHERE project_id = ? AND name = ?"),
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
    serde_json::from_str(&text).map_err(|e| format!("environments.{col}: {e}"))
}

fn row_to_environment(row: &AnyRow) -> Result<EnvironmentRow, String> {
    let get = |c: &str| row.try_get::<String, _>(c).map_err(|e| e.to_string());
    Ok(EnvironmentRow {
        name: get("name")?,
        description: get("description")?,
        vendor: get("vendor")?,
        repos: from_json("repos", get("repos")?)?,
        env_vars: from_json("env_vars", get("env_vars")?)?,
        provision: from_json("provision", get("provision")?)?,
        created_at: get("created_at")?,
        updated_at: get("updated_at")?,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    async fn store() -> (EnvironmentStore, Db) {
        let db = crate::db::testing::db().await;
        (
            EnvironmentStore::new(db.clone(), crate::projects::ProjectId::new("1")),
            db,
        )
    }

    fn row(name: &str) -> EnvironmentRow {
        EnvironmentRow {
            name: name.into(),
            description: "d".into(),
            vendor: "fly".into(),
            repos: vec![EnvironmentRepo {
                url: "https://github.com/o/api".into(),
                git_ref: Some("dev".into()),
                dir: None,
            }],
            env_vars: vec![EnvironmentEnvVar {
                name: "RUST_LOG".into(),
                value: "debug".into(),
            }],
            provision: vec![EnvironmentProvisionStep {
                name: "install deps".into(),
                uses: "run".into(),
                with: vec![EnvironmentStepParam {
                    key: "cmd".into(),
                    value: "make setup".into(),
                }],
            }],
            created_at: "1".into(),
            updated_at: "1".into(),
        }
    }

    #[tokio::test]
    async fn insert_get_list_roundtrip_including_json_columns() {
        let (s, _db) = store().await;
        s.insert(&row("a")).await.unwrap();
        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got, row("a"));
        assert_eq!(got.repos[0].git_ref.as_deref(), Some("dev"));
        assert_eq!(got.provision[0].with[0].key, "cmd");
        assert_eq!(s.list().await.unwrap().len(), 1);
        assert!(s.get("ghost").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn empty_lists_round_trip() {
        let (s, _db) = store().await;
        let mut r = row("a");
        r.repos = vec![];
        r.env_vars = vec![];
        r.provision = vec![];
        s.insert(&r).await.unwrap();
        let got = s.get("a").await.unwrap().unwrap();
        assert!(got.repos.is_empty() && got.env_vars.is_empty() && got.provision.is_empty());
    }

    #[tokio::test]
    async fn duplicate_insert_is_rejected() {
        let (s, _db) = store().await;
        s.insert(&row("a")).await.unwrap();
        assert!(s.insert(&row("a")).await.is_err());
    }

    #[tokio::test]
    async fn replace_updates_and_reports_misses() {
        let (s, _db) = store().await;
        assert!(!s.replace(&row("ghost")).await.unwrap());
        s.insert(&row("a")).await.unwrap();
        let mut r = row("a");
        r.vendor = "docker".into();
        r.updated_at = "2".into();
        assert!(s.replace(&r).await.unwrap());
        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got.vendor, "docker");
        assert_eq!(got.created_at, "1", "replace must not touch created_at");
    }

    #[tokio::test]
    async fn delete_reports_misses() {
        let (s, _db) = store().await;
        s.insert(&row("a")).await.unwrap();
        assert!(s.delete("a").await.unwrap());
        assert!(!s.delete("a").await.unwrap());
    }

    #[tokio::test]
    async fn a_corrupt_json_column_is_an_error_not_a_default() {
        let (s, db) = store().await;
        s.insert(&row("a")).await.unwrap();
        sqlx::query("UPDATE environments SET repos = 'not json' WHERE name = 'a'")
            .execute(db.pool())
            .await
            .unwrap();
        let err = s.get("a").await.unwrap_err();
        assert!(err.contains("repos"), "{err}");
    }
}
