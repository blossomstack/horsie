//! Storage for workflow definitions, sharing the config store's database.
//!
//! The graph lives in one JSON column: a definition is only ever read and
//! written whole, so rows per step would buy joins nobody performs.

use crate::db::Db;
use horsie_models::workflow::WorkflowStepDef;
use sqlx::Row;
use sqlx::any::AnyRow;

const COLS: &str = "name, description, start, steps, created_at, updated_at";

/// One row of the `workflows` table.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowRow {
    pub name: String,
    pub description: String,
    pub start: String,
    pub steps: Vec<WorkflowStepDef>,
    pub created_at: String,
    pub updated_at: String,
}

pub struct WorkflowStore {
    db: Db,
}

impl WorkflowStore {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    pub async fn list(&self) -> Result<Vec<WorkflowRow>, String> {
        let rows = sqlx::query(
            &self
                .db
                .q(&format!("SELECT {COLS} FROM workflows ORDER BY name")),
        )
        .fetch_all(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        rows.iter().map(row_to_workflow).collect()
    }

    pub async fn get(&self, name: &str) -> Result<Option<WorkflowRow>, String> {
        let row = sqlx::query(
            &self
                .db
                .q(&format!("SELECT {COLS} FROM workflows WHERE name = ?")),
        )
        .bind(name)
        .fetch_optional(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_workflow).transpose()
    }

    /// Insert; errs when the name is taken (no upsert — a silent overwrite
    /// would discard the existing graph).
    pub async fn insert(&self, row: &WorkflowRow) -> Result<(), String> {
        sqlx::query(&self.db.q(&format!(
            "INSERT INTO workflows ({COLS}) VALUES (?, ?, ?, ?, ?, ?)"
        )))
        .bind(&row.name)
        .bind(&row.description)
        .bind(&row.start)
        .bind(to_json(&row.steps)?)
        .bind(&row.created_at)
        .bind(&row.updated_at)
        .execute(self.db.pool())
        .await
        .map_err(|e| format!("create workflow '{}': {e}", row.name))?;
        Ok(())
    }

    /// Full replace. Returns false when no workflow has that name.
    pub async fn replace(&self, row: &WorkflowRow) -> Result<bool, String> {
        let res = sqlx::query(&self.db.q(
            "UPDATE workflows SET description = ?, start = ?, steps = ?, updated_at = ? \
             WHERE name = ?",
        ))
        .bind(&row.description)
        .bind(&row.start)
        .bind(to_json(&row.steps)?)
        .bind(&row.updated_at)
        .bind(&row.name)
        .execute(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn delete(&self, name: &str) -> Result<bool, String> {
        let res = sqlx::query(&self.db.q("DELETE FROM workflows WHERE name = ?"))
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

fn row_to_workflow(row: &AnyRow) -> Result<WorkflowRow, String> {
    let get = |c: &str| row.try_get::<String, _>(c).map_err(|e| e.to_string());
    let steps_json = get("steps")?;
    Ok(WorkflowRow {
        name: get("name")?,
        description: get("description")?,
        start: get("start")?,
        steps: serde_json::from_str(&steps_json).map_err(|e| format!("workflows.steps: {e}"))?,
        created_at: get("created_at")?,
        updated_at: get("updated_at")?,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use horsie_models::workflow::WorkflowTransition;

    async fn store() -> WorkflowStore {
        WorkflowStore::new(crate::db::testing::db().await)
    }

    fn row(name: &str) -> WorkflowRow {
        WorkflowRow {
            name: name.into(),
            description: "d".into(),
            start: "triage".into(),
            steps: vec![
                WorkflowStepDef {
                    name: "triage".into(),
                    agent: "bug-triager".into(),
                    prompt: "Triage it.".into(),
                    output_schema: Some(serde_json::json!({
                        "type": "object",
                        "properties": { "severity": { "type": "string" } }
                    })),
                    transitions: Some(vec![
                        WorkflowTransition {
                            to: "fix".into(),
                            condition: Some("output.severity == \"p0\"".into()),
                        },
                        WorkflowTransition {
                            to: "file".into(),
                            condition: None,
                        },
                    ]),
                    max_iterations: Some(20),
                    max_retries: None,
                },
                WorkflowStepDef {
                    name: "fix".into(),
                    agent: "coder".into(),
                    prompt: "Fix it.".into(),
                    output_schema: None,
                    transitions: None,
                    max_iterations: None,
                    max_retries: None,
                },
                WorkflowStepDef {
                    name: "file".into(),
                    agent: "writer".into(),
                    prompt: "File it.".into(),
                    output_schema: None,
                    transitions: None,
                    max_iterations: None,
                    max_retries: None,
                },
            ],
            created_at: "1".into(),
            updated_at: "1".into(),
        }
    }

    #[tokio::test]
    async fn insert_get_list_roundtrip_including_the_graph_column() {
        let s = store().await;
        s.insert(&row("fix-bug")).await.unwrap();
        let got = s.get("fix-bug").await.unwrap().unwrap();
        assert_eq!(got, row("fix-bug"));
        // Transition order decides which condition wins, so it has to survive
        // the round trip.
        let t = got.steps[0].transitions.as_ref().unwrap();
        assert_eq!(t[0].to, "fix");
        assert!(t[1].condition.is_none());
        assert_eq!(s.list().await.unwrap().len(), 1);
        assert!(s.get("ghost").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn duplicate_insert_is_rejected() {
        let s = store().await;
        s.insert(&row("a")).await.unwrap();
        assert!(s.insert(&row("a")).await.is_err());
    }

    #[tokio::test]
    async fn replace_reports_whether_it_matched() {
        let s = store().await;
        s.insert(&row("a")).await.unwrap();
        let mut next = row("a");
        next.description = "changed".into();
        next.start = "fix".into();
        assert!(s.replace(&next).await.unwrap());
        let got = s.get("a").await.unwrap().unwrap();
        assert_eq!(got.description, "changed");
        assert_eq!(got.start, "fix");
        assert!(!s.replace(&row("ghost")).await.unwrap());
    }

    #[tokio::test]
    async fn delete_reports_whether_it_matched() {
        let s = store().await;
        s.insert(&row("a")).await.unwrap();
        assert!(s.delete("a").await.unwrap());
        assert!(!s.delete("a").await.unwrap());
    }
}
