//! Validation and lifecycle over [`ProjectStore`].
//!
//! Creating and renaming are ordinary CRUD. Deleting is not: a project owns
//! rows in sixteen tables, a session list, and whatever substrate its sessions
//! provisioned — so it is the one operation here with an order that matters.

use crate::auth::UserId;
use crate::db::Db;
use crate::projects::ProjectId;
use crate::projects::store::{ProjectRow, ProjectStore};
use crate::sessions::addressing::SupervisorRef;
use crate::sessions::supervisor::SessionSupervisorCommand;

/// Typed service errors so the HTTP layer can pick a status without string
/// matching: NotFound → 404, Conflict → 409, Invalid → 422, Internal → 500.
#[derive(Debug)]
pub enum ProjectError {
    NotFound(String),
    Conflict(String),
    Invalid(String),
    Internal(String),
}

impl std::fmt::Display for ProjectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(m) | Self::Conflict(m) | Self::Invalid(m) | Self::Internal(m) => {
                write!(f, "{m}")
            }
        }
    }
}

impl std::error::Error for ProjectError {}

/// Every table a project owns rows in, and therefore every table a delete has
/// to clear.
///
/// Read by `db::scope_audit` as the definition of "scoped", so this list is not
/// merely documentation: adding a scoped table without adding it here fails
/// that test rather than leaking rows into a deleted project's id.
pub const SCOPED_TABLES: &[&str] = &[
    "providers",
    "models",
    "settings",
    "mcp_servers",
    "plugins",
    "authored_plugins",
    "authored_skills",
    "authored_skill_files",
    "authored_skill_revisions",
    "memory_spaces",
    "memories",
    "agents",
    "routines",
    "environments",
    "workflows",
    "provider_oauth",
    "marketplaces",
    "model_cards",
    "github_credentials",
    "runtime_vendors",
    "agent_runs",
    "entity_revisions",
    "artifacts",
    "artifact_uses",
];

pub struct ProjectService {
    db: Db,
    store: ProjectStore,
}

impl ProjectService {
    #[must_use]
    pub fn new(db: Db) -> Self {
        Self {
            store: ProjectStore::new(db.clone()),
            db,
        }
    }

    #[must_use]
    pub fn store(&self) -> &ProjectStore {
        &self.store
    }

    pub async fn list(&self, user: &UserId) -> Result<Vec<ProjectRow>, ProjectError> {
        self.store.list(user).await.map_err(ProjectError::Internal)
    }

    /// This account's default project, creating it if the account has none.
    pub async fn default_project(&self, user: &UserId) -> Result<ProjectRow, ProjectError> {
        self.store
            .ensure_default(user)
            .await
            .map_err(ProjectError::Internal)
    }

    pub async fn create(&self, user: &UserId, name: &str) -> Result<ProjectRow, ProjectError> {
        let name = validate_name(name)?;
        // Asked rather than inferred from the insert's error: a UNIQUE
        // violation reads differently on the two backends, and matching on
        // either one's message is how a 409 turns into a 500 on the other.
        if self
            .store
            .list(user)
            .await
            .map_err(ProjectError::Internal)?
            .iter()
            .any(|p| p.name == name)
        {
            return Err(ProjectError::Conflict(format!(
                "a project named '{name}' already exists"
            )));
        }
        self.store
            .insert(user, &name)
            .await
            .map_err(ProjectError::Internal)
    }

    pub async fn rename(
        &self,
        user: &UserId,
        id: &ProjectId,
        name: &str,
    ) -> Result<ProjectRow, ProjectError> {
        let name = validate_name(name)?;
        if self
            .store
            .list(user)
            .await
            .map_err(ProjectError::Internal)?
            .iter()
            .any(|p| p.name == name && &p.id != id)
        {
            return Err(ProjectError::Conflict(format!(
                "a project named '{name}' already exists"
            )));
        }
        if !self
            .store
            .rename(user, id, &name)
            .await
            .map_err(ProjectError::Internal)?
        {
            return Err(ProjectError::NotFound(format!("no such project: {id}")));
        }
        self.store
            .get(id)
            .await
            .map_err(ProjectError::Internal)?
            .ok_or_else(|| ProjectError::NotFound(format!("no such project: {id}")))
    }

    /// Delete a project and everything in it.
    ///
    /// The order is the whole content of this function:
    ///
    /// 1. **Refuse a default.** An account must always have somewhere to land,
    ///    and every caller that resolves "this account's project" would
    ///    otherwise have an empty case to invent an answer for.
    /// 2. **Delete the sessions through the supervisor**, one at a time,
    ///    because the session actor is what knows how to cancel a run in flight
    ///    and tell the vendor to destroy the machine. Dropping the rows first
    ///    would leave a sandbox billing with nothing left that names it.
    /// 3. **Clear the supervisor's own journal**, which is not a session's and
    ///    so is not covered by (2).
    /// 4. **Delete the rows**, then the project itself last — so a failure
    ///    part-way through leaves a project that still lists what remains,
    ///    rather than orphaned rows under an id nothing can reach.
    ///
    /// A session that cannot be deleted stops the whole operation. Reporting
    /// "deleted" over a sandbox still running is the more expensive lie.
    ///
    /// The project's bundle stays in [`ProjectRegistry`] afterwards, and that is
    /// deliberate rather than overlooked: it is a handful of `Arc`s that no
    /// request can reach again, because [`crate::http::Scope`] reads the row
    /// before it reaches the registry and the row is gone. Evicting it would
    /// mean a second way to unload a bundle, for a few hundred bytes and an id
    /// that is never reissued.
    ///
    /// [`ProjectRegistry`]: crate::projects::ProjectRegistry
    pub async fn delete(
        &self,
        user: &UserId,
        id: &ProjectId,
        supervisor: &SupervisorRef,
    ) -> Result<(), ProjectError> {
        let project = self
            .store
            .get(id)
            .await
            .map_err(ProjectError::Internal)?
            .filter(|p| &p.user_id == user)
            .ok_or_else(|| ProjectError::NotFound(format!("no such project: {id}")))?;
        if project.is_default {
            return Err(ProjectError::Invalid(
                "the default project cannot be deleted".to_string(),
            ));
        }

        let sessions = supervisor
            .ask(|reply| SessionSupervisorCommand::List { reply })
            .await
            .map_err(|e| {
                ProjectError::Internal(format!("could not read the project's sessions: {e}"))
            })?;
        for (session, _) in sessions {
            supervisor
                .ask(|reply| SessionSupervisorCommand::Delete {
                    id: session.clone(),
                    reply,
                })
                .await
                .map_err(|e| ProjectError::Internal(format!("deleting session {session}: {e}")))?
                .map_err(ProjectError::Internal)?;
        }

        {
            use horsie_actor::Journal;
            let journal = crate::db::journal::SqlJournal::new(self.db.clone());
            let pid = horsie_actor::PersistenceId::new(
                crate::sessions::supervisor::SUPERVISOR_KIND,
                id.as_str(),
            );
            journal
                .clear(&pid)
                .await
                .map_err(|e| ProjectError::Internal(format!("clearing the session list: {e}")))?;
        }

        let mut tx = self
            .db
            .begin_write()
            .await
            .map_err(|e| ProjectError::Internal(e.to_string()))?;
        for table in SCOPED_TABLES {
            // The table name is one of the constants above, never input.
            let sql = self
                .db
                .q(&format!("DELETE FROM {table} WHERE project_id = ?"));
            sqlx::query(&sql)
                .bind(id.as_str())
                .execute(&mut *tx)
                .await
                .map_err(|e| ProjectError::Internal(format!("clearing {table}: {e}")))?;
        }
        tx.commit()
            .await
            .map_err(|e| ProjectError::Internal(e.to_string()))?;

        if !self
            .store
            .delete(user, id)
            .await
            .map_err(ProjectError::Internal)?
        {
            return Err(ProjectError::NotFound(format!("no such project: {id}")));
        }
        Ok(())
    }
}

/// Names are for humans, so the rule is only that there is one and it fits.
///
/// No slug constraint: a project is addressed by its id everywhere — in the URL
/// as much as in the database — so there is nothing a space or a slash could
/// break.
fn validate_name(name: &str) -> Result<String, ProjectError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ProjectError::Invalid("a project needs a name".to_string()));
    }
    if name.chars().count() > 64 {
        return Err(ProjectError::Invalid(
            "a project name is at most 64 characters".to_string(),
        ));
    }
    Ok(name.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    async fn service() -> ProjectService {
        ProjectService::new(crate::db::testing::db().await)
    }

    #[tokio::test]
    async fn a_name_is_trimmed_and_must_not_be_empty() {
        let s = service().await;
        let user = UserId::generate();
        assert_eq!(s.create(&user, "  work  ").await.unwrap().name, "work");
        assert!(matches!(
            s.create(&user, "   ").await,
            Err(ProjectError::Invalid(_))
        ));
        assert!(matches!(
            s.create(&user, &"x".repeat(65)).await,
            Err(ProjectError::Invalid(_))
        ));
    }

    /// A 409 rather than whatever the backend's UNIQUE violation happens to
    /// look like — the two dialects word it differently.
    #[tokio::test]
    async fn a_duplicate_name_is_a_conflict_on_either_backend() {
        let s = service().await;
        let user = UserId::generate();
        s.create(&user, "work").await.unwrap();
        assert!(matches!(
            s.create(&user, "work").await,
            Err(ProjectError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn renaming_to_its_own_name_is_not_a_conflict() {
        let s = service().await;
        let user = UserId::generate();
        let p = s.create(&user, "work").await.unwrap();
        assert_eq!(s.rename(&user, &p.id, "work").await.unwrap().name, "work");
    }

    #[tokio::test]
    async fn renaming_something_that_is_not_yours_is_a_404() {
        let s = service().await;
        let p = s.create(&UserId::generate(), "work").await.unwrap();
        assert!(matches!(
            s.rename(&UserId::generate(), &p.id, "hijacked").await,
            Err(ProjectError::NotFound(_))
        ));
    }

    /// `SCOPED_TABLES` is what a delete clears *and* what the scope audit
    /// checks, so a table that grew a `project_id` and was not added here would
    /// silently survive its project. The audit asserts the other direction —
    /// that every name here really is scoped — which together pin the set.
    #[test]
    fn the_scoped_table_list_has_no_duplicates() {
        let mut sorted = SCOPED_TABLES.to_vec();
        sorted.sort_unstable();
        let len = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), len);
    }
}
