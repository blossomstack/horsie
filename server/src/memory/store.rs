//! SQLite storage for memory spaces and memories, sharing the config store's
//! pool. No secrets, so this is a plain metadata store (mirrors
//! `plugins::store` without the artifact bookkeeping).
//!
//! `memories.space` is not a SQL foreign key -- see `0008_memory.sql` for why.
//! The relationship is enforced here: `create_memory` checks the space exists,
//! and `delete_space` / `rename_space` fix up children inside a transaction.

use sqlx::Row;
use sqlx::sqlite::{SqlitePool, SqliteRow};

const SPACE_COLS: &str = "name, description, created_at, updated_at";
const MEMORY_COLS: &str = "id, space, name, description, content, created_at, updated_at";

/// One row of the `memory_spaces` table.
#[derive(Clone, Debug, PartialEq)]
pub struct MemorySpaceRow {
    pub name: String,
    pub description: String,
    pub created_at: String,
    pub updated_at: String,
}

/// One row of the `memories` table. `id` is ignored on insert (the column is
/// AUTOINCREMENT); `create_memory` returns the assigned id.
#[derive(Clone, Debug, PartialEq)]
pub struct MemoryRow {
    pub id: i64,
    pub space: String,
    pub name: String,
    pub description: String,
    pub content: String,
    pub created_at: String,
    pub updated_at: String,
}

pub struct MemoryStore {
    pool: SqlitePool,
}

impl MemoryStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    // --- spaces ---

    pub async fn list_spaces(&self) -> Result<Vec<MemorySpaceRow>, String> {
        let rows = sqlx::query(&format!(
            "SELECT {SPACE_COLS} FROM memory_spaces ORDER BY name"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        rows.iter().map(row_to_space).collect()
    }

    pub async fn get_space(&self, name: &str) -> Result<Option<MemorySpaceRow>, String> {
        let row = sqlx::query(&format!(
            "SELECT {SPACE_COLS} FROM memory_spaces WHERE name = ?"
        ))
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_space).transpose()
    }

    /// Insert a space. Errs when the name is taken (no upsert: a silent
    /// overwrite would discard the existing description).
    pub async fn create_space(&self, row: &MemorySpaceRow) -> Result<(), String> {
        sqlx::query(
            "INSERT INTO memory_spaces (name, description, created_at, updated_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(&row.name)
        .bind(&row.description)
        .bind(&row.created_at)
        .bind(&row.updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("create space '{}': {e}", row.name))?;
        Ok(())
    }

    /// Returns false when no space by that name exists.
    pub async fn update_space_description(
        &self,
        name: &str,
        description: &str,
        updated_at: &str,
    ) -> Result<bool, String> {
        let res =
            sqlx::query("UPDATE memory_spaces SET description = ?, updated_at = ? WHERE name = ?")
                .bind(description)
                .bind(updated_at)
                .bind(name)
                .execute(&self.pool)
                .await
                .map_err(|e| e.to_string())?;
        Ok(res.rows_affected() > 0)
    }

    /// Rename a space, carrying its memories across. The space name is the join
    /// key, so a bare `UPDATE memory_spaces SET name = ?` would orphan every
    /// memory in it -- all three statements run in one transaction.
    pub async fn rename_space(&self, old: &str, new: &str, updated_at: &str) -> Result<(), String> {
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;
        let existing = sqlx::query(&format!(
            "SELECT {SPACE_COLS} FROM memory_spaces WHERE name = ?"
        ))
        .bind(old)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        let existing = existing
            .as_ref()
            .map(row_to_space)
            .transpose()?
            .ok_or_else(|| format!("unknown memory space '{old}'"))?;

        sqlx::query(
            "INSERT INTO memory_spaces (name, description, created_at, updated_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(new)
        .bind(&existing.description)
        .bind(&existing.created_at)
        .bind(updated_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("rename to '{new}': {e}"))?;

        sqlx::query("UPDATE memories SET space = ? WHERE space = ?")
            .bind(new)
            .bind(old)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;

        sqlx::query("DELETE FROM memory_spaces WHERE name = ?")
            .bind(old)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;

        tx.commit().await.map_err(|e| e.to_string())
    }

    /// Delete a space and every memory in it. Returns false when the space did
    /// not exist.
    pub async fn delete_space(&self, name: &str) -> Result<bool, String> {
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;
        sqlx::query("DELETE FROM memories WHERE space = ?")
            .bind(name)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        let res = sqlx::query("DELETE FROM memory_spaces WHERE name = ?")
            .bind(name)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(res.rows_affected() > 0)
    }

    // --- memories ---

    /// All memories, or just one space's, ordered by `space, name`.
    pub async fn list_memories(&self, space: Option<&str>) -> Result<Vec<MemoryRow>, String> {
        let rows = match space {
            Some(s) => {
                sqlx::query(&format!(
                    "SELECT {MEMORY_COLS} FROM memories WHERE space = ? ORDER BY space, name"
                ))
                .bind(s)
                .fetch_all(&self.pool)
                .await
            }
            None => {
                sqlx::query(&format!(
                    "SELECT {MEMORY_COLS} FROM memories ORDER BY space, name"
                ))
                .fetch_all(&self.pool)
                .await
            }
        }
        .map_err(|e| e.to_string())?;
        rows.iter().map(row_to_memory).collect()
    }

    /// Memories across several spaces -- the index query, run once per turn.
    /// An empty `spaces` yields an empty result without touching the DB.
    pub async fn memories_in(&self, spaces: &[String]) -> Result<Vec<MemoryRow>, String> {
        if spaces.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; spaces.len()].join(", ");
        let sql = format!(
            "SELECT {MEMORY_COLS} FROM memories WHERE space IN ({placeholders}) \
             ORDER BY space, name"
        );
        let mut q = sqlx::query(&sql);
        for s in spaces {
            q = q.bind(s);
        }
        let rows = q.fetch_all(&self.pool).await.map_err(|e| e.to_string())?;
        rows.iter().map(row_to_memory).collect()
    }

    pub async fn get_memory(&self, id: i64) -> Result<Option<MemoryRow>, String> {
        let row = sqlx::query(&format!("SELECT {MEMORY_COLS} FROM memories WHERE id = ?"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_memory).transpose()
    }

    pub async fn get_memory_by_ref(
        &self,
        space: &str,
        name: &str,
    ) -> Result<Option<MemoryRow>, String> {
        let row = sqlx::query(&format!(
            "SELECT {MEMORY_COLS} FROM memories WHERE space = ? AND name = ?"
        ))
        .bind(space)
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_memory).transpose()
    }

    /// Insert a memory, returning its assigned id. Verifies the space exists in
    /// the same transaction as the insert, since there is no FK to do it.
    pub async fn create_memory(&self, row: &MemoryRow) -> Result<i64, String> {
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;
        let space = sqlx::query("SELECT name FROM memory_spaces WHERE name = ?")
            .bind(&row.space)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        if space.is_none() {
            return Err(format!("unknown memory space '{}'", row.space));
        }
        let res = sqlx::query(
            "INSERT INTO memories (space, name, description, content, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.space)
        .bind(&row.name)
        .bind(&row.description)
        .bind(&row.content)
        .bind(&row.created_at)
        .bind(&row.updated_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("create memory '{}/{}': {e}", row.space, row.name))?;
        let id = res.last_insert_rowid();
        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(id)
    }

    /// Update the supplied fields only; `None` leaves a field untouched.
    /// Returns false when no memory has that id.
    pub async fn update_memory(
        &self,
        id: i64,
        description: Option<&str>,
        content: Option<&str>,
        updated_at: &str,
    ) -> Result<bool, String> {
        let res = sqlx::query(
            "UPDATE memories SET description = COALESCE(?, description), \
             content = COALESCE(?, content), updated_at = ? WHERE id = ?",
        )
        .bind(description)
        .bind(content)
        .bind(updated_at)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn delete_memory(&self, id: i64) -> Result<bool, String> {
        let res = sqlx::query("DELETE FROM memories WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(res.rows_affected() > 0)
    }
}

fn row_to_space(row: &SqliteRow) -> Result<MemorySpaceRow, String> {
    let get_s = |c: &str| row.try_get::<String, _>(c).map_err(|e| e.to_string());
    Ok(MemorySpaceRow {
        name: get_s("name")?,
        description: get_s("description")?,
        created_at: get_s("created_at")?,
        updated_at: get_s("updated_at")?,
    })
}

fn row_to_memory(row: &SqliteRow) -> Result<MemoryRow, String> {
    let get_s = |c: &str| row.try_get::<String, _>(c).map_err(|e| e.to_string());
    Ok(MemoryRow {
        id: row.try_get::<i64, _>("id").map_err(|e| e.to_string())?,
        space: get_s("space")?,
        name: get_s("name")?,
        description: get_s("description")?,
        content: get_s("content")?,
        created_at: get_s("created_at")?,
        updated_at: get_s("updated_at")?,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::str::FromStr;

    async fn store() -> (MemoryStore, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}/t.db", tmp.path().display());
        let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
            .unwrap()
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePool::connect_with(opts).await.unwrap();
        sqlx::migrate!().run(&pool).await.unwrap();
        (MemoryStore::new(pool), tmp)
    }

    fn mem(space: &str, name: &str) -> MemoryRow {
        MemoryRow {
            id: 0,
            space: space.into(),
            name: name.into(),
            description: "d".into(),
            content: "body".into(),
            created_at: "1".into(),
            updated_at: "1".into(),
        }
    }

    async fn add_space(s: &MemoryStore, name: &str) {
        s.create_space(&MemorySpaceRow {
            name: name.into(),
            description: String::new(),
            created_at: "1".into(),
            updated_at: "1".into(),
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn migration_seeds_exactly_the_default_space() {
        let (s, _t) = store().await;
        let spaces = s.list_spaces().await.unwrap();
        assert_eq!(spaces.len(), 1);
        assert_eq!(spaces[0].name, "default");
    }

    #[tokio::test]
    async fn memory_crud_roundtrip() {
        let (s, _t) = store().await;
        let id = s.create_memory(&mem("default", "alpha")).await.unwrap();
        let got = s.get_memory(id).await.unwrap().unwrap();
        assert_eq!(got.name, "alpha");
        assert_eq!(got.content, "body");

        let by_ref = s.get_memory_by_ref("default", "alpha").await.unwrap();
        assert_eq!(by_ref.unwrap().id, id);

        assert!(
            s.update_memory(id, None, Some("new body"), "2")
                .await
                .unwrap()
        );
        let got = s.get_memory(id).await.unwrap().unwrap();
        assert_eq!(got.content, "new body");
        assert_eq!(got.description, "d", "None must leave the field untouched");
        assert_eq!(got.updated_at, "2");

        assert!(s.delete_memory(id).await.unwrap());
        assert!(s.get_memory(id).await.unwrap().is_none());
        assert!(
            !s.delete_memory(id).await.unwrap(),
            "second delete is a miss"
        );
    }

    #[tokio::test]
    async fn duplicate_name_in_same_space_is_rejected_but_allowed_across_spaces() {
        let (s, _t) = store().await;
        add_space(&s, "other").await;
        s.create_memory(&mem("default", "alpha")).await.unwrap();
        assert!(s.create_memory(&mem("default", "alpha")).await.is_err());
        s.create_memory(&mem("other", "alpha")).await.unwrap();
    }

    #[tokio::test]
    async fn create_memory_in_unknown_space_is_rejected() {
        let (s, _t) = store().await;
        let err = s.create_memory(&mem("nope", "alpha")).await.unwrap_err();
        assert!(err.contains("nope"), "error should name the space: {err}");
    }

    #[tokio::test]
    async fn deleting_a_space_deletes_its_memories_only() {
        let (s, _t) = store().await;
        add_space(&s, "other").await;
        s.create_memory(&mem("default", "alpha")).await.unwrap();
        s.create_memory(&mem("other", "beta")).await.unwrap();

        assert!(s.delete_space("default").await.unwrap());
        assert!(s.get_space("default").await.unwrap().is_none());
        assert!(s.list_memories(Some("default")).await.unwrap().is_empty());
        assert_eq!(s.list_memories(Some("other")).await.unwrap().len(), 1);
        assert!(!s.delete_space("default").await.unwrap());
    }

    #[tokio::test]
    async fn renaming_a_space_carries_its_memories_and_orphans_none() {
        let (s, _t) = store().await;
        s.create_memory(&mem("default", "alpha")).await.unwrap();
        s.rename_space("default", "renamed", "2").await.unwrap();

        assert!(s.get_space("default").await.unwrap().is_none());
        let moved = s.list_memories(Some("renamed")).await.unwrap();
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].name, "alpha");
        assert!(s.list_memories(Some("default")).await.unwrap().is_empty());
        // Nothing left pointing at a space that no longer exists.
        let all = s.list_memories(None).await.unwrap();
        assert!(all.iter().all(|m| m.space == "renamed"));
    }

    #[tokio::test]
    async fn rename_onto_an_existing_space_is_rejected_and_changes_nothing() {
        let (s, _t) = store().await;
        add_space(&s, "other").await;
        s.create_memory(&mem("default", "alpha")).await.unwrap();
        assert!(s.rename_space("default", "other", "2").await.is_err());
        assert!(s.get_space("default").await.unwrap().is_some());
        assert_eq!(s.list_memories(Some("default")).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn memories_in_filters_to_the_named_spaces_and_orders_stably() {
        let (s, _t) = store().await;
        add_space(&s, "other").await;
        add_space(&s, "third").await;
        s.create_memory(&mem("other", "b")).await.unwrap();
        s.create_memory(&mem("default", "a")).await.unwrap();
        s.create_memory(&mem("third", "c")).await.unwrap();

        let rows = s
            .memories_in(&["default".to_string(), "other".to_string()])
            .await
            .unwrap();
        let got: Vec<_> = rows
            .iter()
            .map(|r| (r.space.as_str(), r.name.as_str()))
            .collect();
        assert_eq!(got, vec![("default", "a"), ("other", "b")]);

        assert!(s.memories_in(&[]).await.unwrap().is_empty());
    }
}
