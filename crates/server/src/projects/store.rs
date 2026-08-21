//! Storage for the `projects` table.
//!
//! Scoped by *user*, not by project — this is the one store that sits above the
//! scope rather than inside it, because it is what tells a request which scope
//! it is allowed to use. Nothing here appears in `db::scope_audit`'s list for
//! the same reason: `projects` has no `project_id`.

use crate::auth::UserId;
use crate::db::Db;
use crate::projects::ProjectId;
use sqlx::Row;
use sqlx::any::AnyRow;

const COLS: &str = "id, user_id, name, is_default, created_at, updated_at";

/// One row of the `projects` table.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectRow {
    pub id: ProjectId,
    /// The sole owner. There is no membership: a project belongs to one
    /// account, and orgs stay in the control plane.
    pub user_id: UserId,
    pub name: String,
    /// Exactly one per user, and it cannot be deleted.
    pub is_default: bool,
    pub created_at: String,
    pub updated_at: String,
}

pub struct ProjectStore {
    db: Db,
}

impl ProjectStore {
    #[must_use]
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Every project this account owns, default first and then by name.
    ///
    /// Ordered in SQL rather than by the caller so the switcher, the CLI and
    /// the settings page agree without each remembering to sort.
    pub async fn list(&self, user: &UserId) -> Result<Vec<ProjectRow>, String> {
        let rows = sqlx::query(&self.db.q(&format!(
            "SELECT {COLS} FROM projects WHERE user_id = ? \
             ORDER BY is_default DESC, name"
        )))
        .bind(user.as_str())
        .fetch_all(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        rows.iter().map(row_to_project).collect()
    }

    /// One project by id, whoever owns it.
    ///
    /// Deliberately *not* scoped to a user: the caller that needs this is
    /// [`crate::http::Scope`], which is asking the question "may this account
    /// use this id?" and therefore has to see the owner to answer it. Taking a
    /// `UserId` here and returning `None` on a mismatch would answer it too,
    /// but it would also make an ownership failure indistinguishable from a
    /// typo — and one of those deserves a log line.
    pub async fn get(&self, id: &ProjectId) -> Result<Option<ProjectRow>, String> {
        let row = sqlx::query(
            &self
                .db
                .q(&format!("SELECT {COLS} FROM projects WHERE id = ?")),
        )
        .bind(id.as_str())
        .fetch_optional(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_project).transpose()
    }

    /// This account's default project, creating it if the account has none.
    ///
    /// Called on resolution rather than only at account creation, because there
    /// are three ways an account first appears and only one of them writes an
    /// `auth_users` row: `create_user`, an [`AuthMode::Delegated`] first login
    /// (a `DelegatedIdentity` this repo never creates a row for), and
    /// `Shared.anonymous` on a deployment with authentication off. One
    /// resolution-time call covers all three; three creation-time calls would
    /// cover the first and silently miss the others.
    ///
    /// Two concurrent first requests must not mint two defaults — the account
    /// would then see a different half of its own data depending on which one
    /// resolved. The two backends need different mechanisms for that, and this
    /// carries both:
    ///
    /// * **SQLite** serializes them. [`Db::begin_write`] is `BEGIN IMMEDIATE`,
    ///   so the second reader waits rather than reading "no default" too — and
    ///   a deferred transaction upgrading to a write on its second statement is
    ///   the shape that deadlocks instead of waiting.
    /// * **PostgreSQL does not.** Its `BEGIN` is read-committed and there is no
    ///   row yet to lock, so both transactions read "no default" and both
    ///   insert. The loser gets a unique violation on `(user_id, name)` — and
    ///   that violation *is* the answer: the winner's row is what it wanted, so
    ///   it re-reads instead of failing. `begin_write`'s own comment is right
    ///   that PostgreSQL takes its locks as it goes; it just has nothing to
    ///   take one on until this row exists.
    ///
    /// Caught by `concurrent_first_requests_mint_exactly_one_default` on the
    /// PostgreSQL run only, which is why the suite runs twice.
    ///
    /// [`AuthMode::Delegated`]: crate::auth::AuthMode::Delegated
    pub async fn ensure_default(&self, user: &UserId) -> Result<ProjectRow, String> {
        let mut tx = self.db.begin_write().await.map_err(|e| e.to_string())?;
        let existing = sqlx::query(&self.db.q(&format!(
            "SELECT {COLS} FROM projects WHERE user_id = ? AND is_default = 1"
        )))
        .bind(user.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        if let Some(row) = existing {
            return row_to_project(&row);
        }

        let id = ProjectId::generate();
        let inserted = sqlx::query(&self.db.q(&format!(
            "INSERT INTO projects ({COLS}) \
             VALUES (?, ?, ?, 1, {now}, {now})",
            now = self.db.now_text()
        )))
        .bind(id.as_str())
        .bind(user.as_str())
        .bind(DEFAULT_NAME)
        .execute(&mut *tx)
        .await;

        if let Err(insert_error) = inserted {
            // Rolled back by dropping it: the transaction is poisoned, and the
            // read below has to see the other one's committed row anyway.
            drop(tx);
            return match self.default_of(user).await? {
                Some(row) => Ok(row),
                // Nothing to have raced with, so the violation was real. The
                // account holds a project named `Default` that is not its
                // default — reachable only by creating one before ever
                // resolving one.
                None => Err(format!(
                    "create the default project for {user}: {insert_error}"
                )),
            };
        }

        let row = sqlx::query(
            &self
                .db
                .q(&format!("SELECT {COLS} FROM projects WHERE id = ?")),
        )
        .bind(id.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())?;
        row_to_project(&row)
    }

    /// This account's default project, if it has one. No transaction: the one
    /// caller reads it after another writer has already committed.
    async fn default_of(&self, user: &UserId) -> Result<Option<ProjectRow>, String> {
        let row = sqlx::query(&self.db.q(&format!(
            "SELECT {COLS} FROM projects WHERE user_id = ? AND is_default = 1"
        )))
        .bind(user.as_str())
        .fetch_optional(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_project).transpose()
    }

    /// Create a project. Errs when the account already has one by that name.
    pub async fn insert(&self, user: &UserId, name: &str) -> Result<ProjectRow, String> {
        let id = ProjectId::generate();
        sqlx::query(&self.db.q(&format!(
            "INSERT INTO projects ({COLS}) VALUES (?, ?, ?, 0, {now}, {now})",
            now = self.db.now_text()
        )))
        .bind(id.as_str())
        .bind(user.as_str())
        .bind(name)
        .execute(self.db.pool())
        .await
        .map_err(|e| format!("create project '{name}': {e}"))?;
        self.get(&id)
            .await?
            .ok_or_else(|| format!("project '{name}' vanished after being created"))
    }

    /// Rename. Returns false when no project of this account has that id.
    pub async fn rename(&self, user: &UserId, id: &ProjectId, name: &str) -> Result<bool, String> {
        let res = sqlx::query(&self.db.q(&format!(
            "UPDATE projects SET name = ?, updated_at = {now} \
             WHERE id = ? AND user_id = ?",
            now = self.db.now_text()
        )))
        .bind(name)
        .bind(id.as_str())
        .bind(user.as_str())
        .execute(self.db.pool())
        .await
        .map_err(|e| format!("rename project '{id}': {e}"))?;
        Ok(res.rows_affected() > 0)
    }

    /// Delete the row. Returns false when no project of this account has that
    /// id.
    ///
    /// The row only — [`ProjectService::delete`] is what removes everything the
    /// project owned, and what refuses to touch a default.
    ///
    /// [`ProjectService::delete`]: crate::projects::ProjectService::delete
    pub async fn delete(&self, user: &UserId, id: &ProjectId) -> Result<bool, String> {
        let res = sqlx::query(
            &self
                .db
                .q("DELETE FROM projects WHERE id = ? AND user_id = ? AND is_default = 0"),
        )
        .bind(id.as_str())
        .bind(user.as_str())
        .execute(self.db.pool())
        .await
        .map_err(|e| format!("delete project '{id}': {e}"))?;
        Ok(res.rows_affected() > 0)
    }
}

/// What a default project is called when it is created. Renameable afterwards —
/// it is the `is_default` flag that makes it undeletable, never the name.
pub const DEFAULT_NAME: &str = "Default";

fn row_to_project(row: &AnyRow) -> Result<ProjectRow, String> {
    let text = |col: &str| -> Result<String, String> {
        row.try_get::<String, _>(col)
            .map_err(|e| format!("read projects.{col}: {e}"))
    };
    Ok(ProjectRow {
        id: ProjectId::new(text("id")?),
        user_id: UserId::new(text("user_id")?),
        name: text("name")?,
        // INTEGER in both dialects, deliberately: see 0040_projects.sql.
        is_default: row
            .try_get::<i64, _>("is_default")
            .map_err(|e| format!("read projects.is_default: {e}"))?
            != 0,
        created_at: text("created_at")?,
        updated_at: text("updated_at")?,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    async fn store() -> ProjectStore {
        ProjectStore::new(crate::db::testing::db().await)
    }

    #[tokio::test]
    async fn ensure_default_creates_one_and_then_returns_it() {
        let s = store().await;
        let user = UserId::generate();
        let first = s.ensure_default(&user).await.unwrap();
        assert!(first.is_default);
        assert_eq!(first.name, DEFAULT_NAME);
        assert_eq!(first.user_id, user);

        let second = s.ensure_default(&user).await.unwrap();
        assert_eq!(first, second, "a second call must not create a second one");
        assert_eq!(s.list(&user).await.unwrap().len(), 1);
    }

    /// What the migration's id copy is *for*.
    ///
    /// `0040_projects.sql` seeds a project per owner it finds in the scoped
    /// tables, with `id = user_id`. `0009` seeds a memory space, `0024`
    /// backfilled it to `'1'`, so every migrated database — including a fresh
    /// one, which runs the whole chain — has a project `'1'` owned by `'1'`.
    ///
    /// `ensure_default` must **find** that row rather than mint a new one. If
    /// it minted, two things would break at once and neither loudly: the
    /// account would look at an empty deployment holding all its data, and
    /// every existing actor address — `session-supervisor|1`, `session|1|<uuid>`
    /// — would be orphaned under an id nothing resolves.
    #[tokio::test]
    async fn a_migrated_deployments_default_project_is_the_one_the_migration_seeded() {
        let db = crate::db::testing::db().await;
        let seeded: Vec<(String, String)> =
            sqlx::query_as(&db.q("SELECT id, user_id FROM projects"))
                .fetch_all(db.pool())
                .await
                .unwrap();
        assert_eq!(
            seeded,
            vec![("1".to_string(), "1".to_string())],
            "the migration seeds a project per owner it finds in the scoped tables"
        );

        let bootstrap = UserId::bootstrap();
        let row = ProjectStore::new(db.clone())
            .ensure_default(&bootstrap)
            .await
            .unwrap();
        assert_eq!(
            row.id.as_str(),
            bootstrap.as_str(),
            "the first account must resolve the project the migration seeded, \
             not a fresh one"
        );

        // And that project is the one the backfilled rows are under, which is
        // the half an id comparison alone would not prove.
        let spaces: i64 =
            sqlx::query_scalar(&db.q("SELECT COUNT(*) FROM memory_spaces WHERE project_id = ?"))
                .bind(row.id.as_str())
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(spaces, 1, "0009's seeded memory space must be reachable");
    }

    /// The id is minted, never derived. A deployment migrated by 0040 has a
    /// default project whose id happens to equal its owner's, and nothing may
    /// come to depend on that.
    #[tokio::test]
    async fn a_fresh_default_project_does_not_reuse_the_user_id() {
        let s = store().await;
        let user = UserId::generate();
        let row = s.ensure_default(&user).await.unwrap();
        assert_ne!(row.id.as_str(), user.as_str());
    }

    /// Two requests arriving together for an account that has never been seen.
    /// Without `begin_write` this races into two defaults, and the account then
    /// sees a different half of its own data depending on which one resolves.
    #[tokio::test]
    async fn concurrent_first_requests_mint_exactly_one_default() {
        let db = crate::db::testing::db().await;
        let user = UserId::generate();
        let (a, b) = tokio::join!(
            {
                let s = ProjectStore::new(db.clone());
                let u = user.clone();
                async move { s.ensure_default(&u).await }
            },
            {
                let s = ProjectStore::new(db.clone());
                let u = user.clone();
                async move { s.ensure_default(&u).await }
            },
        );
        assert_eq!(a.unwrap(), b.unwrap());
        assert_eq!(
            ProjectStore::new(db).list(&user).await.unwrap().len(),
            1,
            "two defaults would split the account's data in half"
        );
    }

    #[tokio::test]
    async fn a_project_is_only_visible_to_its_owner() {
        let s = store().await;
        let (mine, theirs) = (UserId::generate(), UserId::generate());
        let p = s.insert(&mine, "work").await.unwrap();

        assert_eq!(s.list(&theirs).await.unwrap(), vec![]);
        assert!(!s.rename(&theirs, &p.id, "hijacked").await.unwrap());
        assert!(!s.delete(&theirs, &p.id).await.unwrap());
        assert_eq!(s.get(&p.id).await.unwrap().unwrap().name, "work");
    }

    #[tokio::test]
    async fn the_default_project_cannot_be_deleted() {
        let s = store().await;
        let user = UserId::generate();
        let default = s.ensure_default(&user).await.unwrap();
        assert!(!s.delete(&user, &default.id).await.unwrap());

        let other = s.insert(&user, "second").await.unwrap();
        assert!(s.delete(&user, &other.id).await.unwrap());
        assert_eq!(s.list(&user).await.unwrap(), vec![default]);
    }

    #[tokio::test]
    async fn two_projects_of_one_account_cannot_share_a_name() {
        let s = store().await;
        let user = UserId::generate();
        s.insert(&user, "work").await.unwrap();
        assert!(s.insert(&user, "work").await.is_err());
        // But two accounts may each have one.
        assert!(s.insert(&UserId::generate(), "work").await.is_ok());
    }

    #[tokio::test]
    async fn the_default_sorts_first_and_the_rest_by_name() {
        let s = store().await;
        let user = UserId::generate();
        s.insert(&user, "zebra").await.unwrap();
        s.insert(&user, "apple").await.unwrap();
        let default = s.ensure_default(&user).await.unwrap();
        let names: Vec<String> = s
            .list(&user)
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, vec![default.name, "apple".into(), "zebra".into()]);
    }

    #[tokio::test]
    async fn renaming_reports_whether_it_found_anything() {
        let s = store().await;
        let user = UserId::generate();
        let p = s.insert(&user, "work").await.unwrap();
        assert!(s.rename(&user, &p.id, "play").await.unwrap());
        assert_eq!(s.get(&p.id).await.unwrap().unwrap().name, "play");
        assert!(
            !s.rename(&user, &ProjectId::generate(), "ghost")
                .await
                .unwrap()
        );
    }
}
