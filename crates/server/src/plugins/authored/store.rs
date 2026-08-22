//! Storage for authored plugins: the editable source the renderer reads.
//!
//! Four tables, one job each — the plugin's identity, the head of each skill,
//! the head of each skill's files, and an append-only revision log. A save
//! touches all four in one transaction, because a generation that has been
//! bumped without its content landing names a package the server cannot render.

use crate::db::Db;
use crate::projects::ProjectId;
use sqlx::Row;
use sqlx::any::AnyRow;

/// One file beside a skill's `SKILL.md`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AuthoredFile {
    pub path: String,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AuthoredPluginRow {
    pub name: String,
    pub description: Option<String>,
    pub generation: u64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AuthoredSkillRow {
    pub plugin: String,
    pub name: String,
    pub description: String,
    pub body: String,
    pub revision: u64,
    pub updated_at: String,
}

/// One past state of a skill, including the state "deleted".
#[derive(Clone, Debug, PartialEq)]
pub struct AuthoredRevisionRow {
    pub revision: u64,
    pub description: String,
    pub body: String,
    pub files: Vec<AuthoredFile>,
    pub deleted: bool,
    pub created_at: String,
}

pub struct AuthoredStore {
    db: Db,
    user: ProjectId,
}

impl AuthoredStore {
    pub fn new(db: Db, user: ProjectId) -> Self {
        Self { db, user }
    }

    pub async fn list_plugins(&self) -> Result<Vec<AuthoredPluginRow>, String> {
        let sql = self.db.q(
            "SELECT name, description, generation, created_at, updated_at \
             FROM authored_plugins WHERE project_id = ? ORDER BY name",
        );
        let rows = sqlx::query(&sql)
            .bind(self.user.as_str())
            .fetch_all(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        rows.iter().map(row_to_plugin).collect()
    }

    pub async fn get_plugin(&self, name: &str) -> Result<Option<AuthoredPluginRow>, String> {
        let sql = self.db.q(
            "SELECT name, description, generation, created_at, updated_at \
             FROM authored_plugins WHERE project_id = ? AND name = ?",
        );
        let row = sqlx::query(&sql)
            .bind(self.user.as_str())
            .bind(name)
            .fetch_optional(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_plugin).transpose()
    }

    /// Create the plugin, or change its description. Either way the generation
    /// advances: the description is rendered into `plugin.json`, so an edit
    /// changes the package's bytes.
    pub async fn upsert_plugin(
        &self,
        name: &str,
        description: Option<&str>,
        now: &str,
    ) -> Result<AuthoredPluginRow, String> {
        let mut tx = self.db.begin_write().await.map_err(|e| e.to_string())?;
        let existing = sqlx::query(&self.db.q(
            "SELECT generation, created_at FROM authored_plugins WHERE project_id = ? AND name = ?",
        ))
        .bind(self.user.as_str())
        .bind(name)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        let (generation, created_at) = match &existing {
            Some(r) => (
                u64::try_from(r.try_get::<i64, _>("generation").unwrap_or(0)).unwrap_or(0) + 1,
                r.try_get::<String, _>("created_at")
                    .unwrap_or_else(|_| now.to_string()),
            ),
            None => (1, now.to_string()),
        };
        sqlx::query(&self.db.q(
            "INSERT INTO authored_plugins (project_id, name, description, generation, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT(project_id, name) DO UPDATE SET description = excluded.description, \
             generation = excluded.generation, updated_at = excluded.updated_at",
        ))
        .bind(self.user.as_str())
        .bind(name)
        .bind(description)
        .bind(i64::try_from(generation).unwrap_or(i64::MAX))
        .bind(&created_at)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("save plugin '{name}': {e}"))?;
        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(AuthoredPluginRow {
            name: name.to_string(),
            description: description.map(str::to_string),
            generation,
            created_at,
            updated_at: now.to_string(),
        })
    }

    pub async fn delete_plugin(&self, name: &str) -> Result<(), String> {
        let mut tx = self.db.begin_write().await.map_err(|e| e.to_string())?;
        // All three name the plugin the same way, so one statement shape
        // serves them.
        for table in [
            "authored_skill_revisions",
            "authored_skill_files",
            "authored_skills",
        ] {
            let sql = self.db.q(&format!(
                "DELETE FROM {table} WHERE project_id = ? AND plugin = ?"
            ));
            sqlx::query(&sql)
                .bind(self.user.as_str())
                .bind(name)
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
        }
        sqlx::query(
            &self
                .db
                .q("DELETE FROM authored_plugins WHERE project_id = ? AND name = ?"),
        )
        .bind(self.user.as_str())
        .bind(name)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())
    }

    /// Every skill of `plugin`, or of every plugin when `None`.
    pub async fn list_skills(&self, plugin: Option<&str>) -> Result<Vec<AuthoredSkillRow>, String> {
        let statement = match plugin {
            Some(_) => {
                "SELECT plugin, name, description, body, revision, updated_at FROM authored_skills \
                 WHERE project_id = ? AND plugin = ? ORDER BY plugin, name"
            }
            None => {
                "SELECT plugin, name, description, body, revision, updated_at FROM authored_skills \
                 WHERE project_id = ? ORDER BY plugin, name"
            }
        };
        let sql = self.db.q(statement);
        let mut query = sqlx::query(&sql).bind(self.user.as_str());
        if let Some(p) = plugin {
            query = query.bind(p);
        }
        let rows = query
            .fetch_all(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        rows.iter().map(row_to_skill).collect()
    }

    pub async fn get_skill(
        &self,
        plugin: &str,
        name: &str,
    ) -> Result<Option<AuthoredSkillRow>, String> {
        let sql = self.db.q(
            "SELECT plugin, name, description, body, revision, updated_at FROM authored_skills \
             WHERE project_id = ? AND plugin = ? AND name = ?",
        );
        let row = sqlx::query(&sql)
            .bind(self.user.as_str())
            .bind(plugin)
            .bind(name)
            .fetch_optional(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_skill).transpose()
    }

    pub async fn files_for(&self, plugin: &str, skill: &str) -> Result<Vec<AuthoredFile>, String> {
        let sql = self.db.q("SELECT path, content FROM authored_skill_files \
             WHERE project_id = ? AND plugin = ? AND skill = ? ORDER BY path");
        let rows = sqlx::query(&sql)
            .bind(self.user.as_str())
            .bind(plugin)
            .bind(skill)
            .fetch_all(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        rows.iter()
            .map(|r| {
                Ok(AuthoredFile {
                    path: r.try_get::<String, _>("path").map_err(|e| e.to_string())?,
                    content: r
                        .try_get::<String, _>("content")
                        .map_err(|e| e.to_string())?,
                })
            })
            .collect()
    }

    /// Write a skill and advance the plugin's generation, in one transaction.
    ///
    /// The revision log is appended to first and the head overwritten after, so
    /// a crash between the two leaves history that is ahead of the head rather
    /// than a head with no history behind it.
    pub async fn save_skill(
        &self,
        row: &AuthoredSkillRow,
        files: &[AuthoredFile],
        now: &str,
    ) -> Result<u64, String> {
        let mut tx = self.db.begin_write().await.map_err(|e| e.to_string())?;
        let revision = self.next_revision(&mut tx, &row.plugin, &row.name).await?;
        self.append_revision(
            &mut tx,
            &row.plugin,
            &row.name,
            revision,
            &row.description,
            &row.body,
            files,
            false,
            now,
        )
        .await?;
        sqlx::query(&self.db.q(
            "INSERT INTO authored_skills (project_id, plugin, name, description, body, revision, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(project_id, plugin, name) DO UPDATE SET description = excluded.description, \
             body = excluded.body, revision = excluded.revision, updated_at = excluded.updated_at",
        ))
        .bind(self.user.as_str())
        .bind(&row.plugin)
        .bind(&row.name)
        .bind(&row.description)
        .bind(&row.body)
        .bind(i64::try_from(revision).unwrap_or(i64::MAX))
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("save skill '{}/{}': {e}", row.plugin, row.name))?;

        self.replace_files(&mut tx, &row.plugin, &row.name, files)
            .await?;
        let generation = self.bump_generation(&mut tx, &row.plugin, now).await?;
        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(generation)
    }

    /// Remove a skill's head, keeping its history and recording the removal as
    /// a revision of its own.
    pub async fn delete_skill(&self, plugin: &str, name: &str, now: &str) -> Result<u64, String> {
        let mut tx = self.db.begin_write().await.map_err(|e| e.to_string())?;
        let revision = self.next_revision(&mut tx, plugin, name).await?;
        self.append_revision(&mut tx, plugin, name, revision, "", "", &[], true, now)
            .await?;
        for (table, column) in [
            ("authored_skill_files", "skill"),
            ("authored_skills", "name"),
        ] {
            sqlx::query(&self.db.q(&format!(
                "DELETE FROM {table} WHERE project_id = ? AND plugin = ? AND {column} = ?"
            )))
            .bind(self.user.as_str())
            .bind(plugin)
            .bind(name)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        }
        let generation = self.bump_generation(&mut tx, plugin, now).await?;
        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(generation)
    }

    pub async fn revisions(
        &self,
        plugin: &str,
        skill: &str,
    ) -> Result<Vec<AuthoredRevisionRow>, String> {
        let sql = self.db.q(
            "SELECT revision, description, body, files, deleted, created_at \
             FROM authored_skill_revisions WHERE project_id = ? AND plugin = ? AND skill = ? \
             ORDER BY revision DESC",
        );
        let rows = sqlx::query(&sql)
            .bind(self.user.as_str())
            .bind(plugin)
            .bind(skill)
            .fetch_all(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        rows.iter().map(row_to_revision).collect()
    }

    pub async fn revision(
        &self,
        plugin: &str,
        skill: &str,
        revision: u64,
    ) -> Result<Option<AuthoredRevisionRow>, String> {
        let sql = self.db.q(
            "SELECT revision, description, body, files, deleted, created_at \
             FROM authored_skill_revisions \
             WHERE project_id = ? AND plugin = ? AND skill = ? AND revision = ?",
        );
        let row = sqlx::query(&sql)
            .bind(self.user.as_str())
            .bind(plugin)
            .bind(skill)
            .bind(i64::try_from(revision).unwrap_or(i64::MAX))
            .fetch_optional(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_revision).transpose()
    }

    async fn next_revision(
        &self,
        tx: &mut sqlx::Transaction<'static, sqlx::Any>,
        plugin: &str,
        skill: &str,
    ) -> Result<u64, String> {
        let row = sqlx::query(
            &self
                .db
                .q("SELECT MAX(revision) AS top FROM authored_skill_revisions \
             WHERE project_id = ? AND plugin = ? AND skill = ?"),
        )
        .bind(self.user.as_str())
        .bind(plugin)
        .bind(skill)
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| e.to_string())?;
        let top = row.try_get::<Option<i64>, _>("top").unwrap_or(None);
        Ok(u64::try_from(top.unwrap_or(0)).unwrap_or(0) + 1)
    }

    #[allow(clippy::too_many_arguments)]
    async fn append_revision(
        &self,
        tx: &mut sqlx::Transaction<'static, sqlx::Any>,
        plugin: &str,
        skill: &str,
        revision: u64,
        description: &str,
        body: &str,
        files: &[AuthoredFile],
        deleted: bool,
        now: &str,
    ) -> Result<(), String> {
        sqlx::query(&self.db.q("INSERT INTO authored_skill_revisions \
             (project_id, plugin, skill, revision, description, body, files, deleted, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"))
        .bind(self.user.as_str())
        .bind(plugin)
        .bind(skill)
        .bind(i64::try_from(revision).unwrap_or(i64::MAX))
        .bind(description)
        .bind(body)
        .bind(serde_json::to_string(files).unwrap_or_else(|_| "[]".to_string()))
        .bind(i64::from(deleted))
        .bind(now)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("append revision for '{plugin}/{skill}': {e}"))?;
        Ok(())
    }

    async fn replace_files(
        &self,
        tx: &mut sqlx::Transaction<'static, sqlx::Any>,
        plugin: &str,
        skill: &str,
        files: &[AuthoredFile],
    ) -> Result<(), String> {
        sqlx::query(&self.db.q(
            "DELETE FROM authored_skill_files WHERE project_id = ? AND plugin = ? AND skill = ?",
        ))
        .bind(self.user.as_str())
        .bind(plugin)
        .bind(skill)
        .execute(&mut **tx)
        .await
        .map_err(|e| e.to_string())?;
        for file in files {
            sqlx::query(&self.db.q(
                "INSERT INTO authored_skill_files (project_id, plugin, skill, path, content) \
                 VALUES (?, ?, ?, ?, ?)",
            ))
            .bind(self.user.as_str())
            .bind(plugin)
            .bind(skill)
            .bind(&file.path)
            .bind(&file.content)
            .execute(&mut **tx)
            .await
            .map_err(|e| format!("save file '{}': {e}", file.path))?;
        }
        Ok(())
    }

    /// Advance the plugin's generation. Errs when the plugin does not exist:
    /// a skill saved into nothing would be unreachable, and silently creating
    /// the plugin here would hide a typo in its name.
    async fn bump_generation(
        &self,
        tx: &mut sqlx::Transaction<'static, sqlx::Any>,
        plugin: &str,
        now: &str,
    ) -> Result<u64, String> {
        let updated = sqlx::query(&self.db.q(
            "UPDATE authored_plugins SET generation = generation + 1, updated_at = ? \
             WHERE project_id = ? AND name = ? RETURNING generation",
        ))
        .bind(now)
        .bind(self.user.as_str())
        .bind(plugin)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no authored plugin '{plugin}'"))?;
        Ok(u64::try_from(updated.try_get::<i64, _>("generation").unwrap_or(1)).unwrap_or(1))
    }
}

fn row_to_plugin(row: &AnyRow) -> Result<AuthoredPluginRow, String> {
    Ok(AuthoredPluginRow {
        name: row
            .try_get("name")
            .map_err(|e: sqlx::Error| e.to_string())?,
        description: row
            .try_get::<Option<String>, _>("description")
            .map_err(|e| e.to_string())?,
        generation: u64::try_from(
            row.try_get::<i64, _>("generation")
                .map_err(|e| e.to_string())?,
        )
        .unwrap_or(0),
        created_at: row.try_get("created_at").map_err(|e| e.to_string())?,
        updated_at: row.try_get("updated_at").map_err(|e| e.to_string())?,
    })
}

fn row_to_skill(row: &AnyRow) -> Result<AuthoredSkillRow, String> {
    Ok(AuthoredSkillRow {
        plugin: row
            .try_get("plugin")
            .map_err(|e: sqlx::Error| e.to_string())?,
        name: row.try_get("name").map_err(|e| e.to_string())?,
        description: row.try_get("description").map_err(|e| e.to_string())?,
        body: row.try_get("body").map_err(|e| e.to_string())?,
        revision: u64::try_from(
            row.try_get::<i64, _>("revision")
                .map_err(|e| e.to_string())?,
        )
        .unwrap_or(0),
        updated_at: row.try_get("updated_at").map_err(|e| e.to_string())?,
    })
}

fn row_to_revision(row: &AnyRow) -> Result<AuthoredRevisionRow, String> {
    Ok(AuthoredRevisionRow {
        revision: u64::try_from(
            row.try_get::<i64, _>("revision")
                .map_err(|e: sqlx::Error| e.to_string())?,
        )
        .unwrap_or(0),
        description: row.try_get("description").map_err(|e| e.to_string())?,
        body: row.try_get("body").map_err(|e| e.to_string())?,
        files: row
            .try_get::<String, _>("files")
            .ok()
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default(),
        deleted: row
            .try_get::<i64, _>("deleted")
            .map_err(|e| e.to_string())?
            != 0,
        created_at: row.try_get("created_at").map_err(|e| e.to_string())?,
    })
}
