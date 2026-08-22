//! Version history and compare-and-set, shared by agent presets and memories.
//!
//! Both are things a scheduled tuning agent rewrites without a human watching,
//! so both want the same two properties: a record of what they used to say, and
//! a refusal when the writer's idea of "current" has gone stale. See the 0044
//! migration for why.
//!
//! Every method here takes a transaction rather than opening one. That is the
//! whole discipline of this module: appending a revision and overwriting the
//! head are one write, and a caller that could do the first without the second
//! would leave a history claiming a change that never landed.

use crate::db::Db;
use crate::projects::ProjectId;
use sqlx::{Any, Row, Transaction};

/// Which kind of thing a revision belongs to.
///
/// An enum rather than a `&str`, so a typo cannot silently open a third
/// namespace that shares the table and matches nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntityKind {
    Agent,
    Memory,
}

impl EntityKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Memory => "memory",
        }
    }
}

/// One past version of an entity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revision {
    pub revision: i64,
    /// A JSON snapshot of the whole entity as it was.
    pub payload: String,
    /// This revision recorded a deletion; `payload` is what was deleted.
    pub deleted: bool,
    pub created_at: String,
}

/// Why a compare-and-set write was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum CasError {
    /// The caller named a revision that is not the current one. It re-reads and
    /// decides again — there is no merge that would be right here, because the
    /// two writers disagree about what the entity should say, not about how to
    /// combine two halves of it.
    Stale { expected: i64, actual: Option<i64> },
}

impl std::fmt::Display for CasError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stale { expected, actual } => match actual {
                Some(actual) => write!(
                    f,
                    "this was changed since you read it: you expected revision \
                     {expected}, it is now at {actual}. Read it again and \
                     decide against what it says now."
                ),
                None => write!(
                    f,
                    "you expected revision {expected}, but this has no revision \
                     history — it has not been written since versioning was \
                     added. Write without an expected revision, or read it first."
                ),
            },
        }
    }
}

pub struct RevisionStore {
    db: Db,
    project: ProjectId,
}

impl RevisionStore {
    pub fn new(db: Db, project: ProjectId) -> Self {
        Self { db, project }
    }

    /// Refuse the write unless `head` is what the caller expected.
    ///
    /// `expected` of `None` is an unconditional write. That is not a loophole
    /// left open by accident: the web form and the CLI have always written
    /// unconditionally, and making them all carry a revision is a separate
    /// change from giving the ones that want it somewhere to put it. What a
    /// tuning agent gets is the *ability* to be careful, and the control plane
    /// is where that is spent.
    pub fn check(head: Option<i64>, expected: Option<i64>) -> Result<(), CasError> {
        match expected {
            None => Ok(()),
            Some(expected) if head == Some(expected) => Ok(()),
            Some(expected) => Err(CasError::Stale {
                expected,
                actual: head,
            }),
        }
    }

    /// Append a revision and answer with its number.
    ///
    /// The caller writes that number onto the entity's own `revision` column in
    /// the same transaction. Two statements rather than one because the head
    /// lives on the entity — which is what lets a read of the entity answer
    /// "what revision is this" without touching this table at all.
    pub async fn append(
        &self,
        tx: &mut Transaction<'static, Any>,
        kind: EntityKind,
        entity_id: &str,
        payload: &str,
        deleted: bool,
        created_at: &str,
    ) -> Result<i64, String> {
        let next = self.next(tx, kind, entity_id).await?;
        sqlx::query(&self.db.q("INSERT INTO entity_revisions \
             (project_id, entity_kind, entity_id, revision, payload, deleted, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)"))
        .bind(self.project.as_str())
        .bind(kind.as_str())
        .bind(entity_id)
        .bind(next)
        .bind(payload)
        .bind(i64::from(deleted))
        .bind(created_at)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("append revision for {} '{entity_id}': {e}", kind.as_str()))?;
        Ok(next)
    }

    /// The number the next revision of this entity gets.
    ///
    /// Read from the history rather than from the entity's own head: a deleted
    /// entity has no head row left, and re-creating it under the same name must
    /// not reuse numbers its history already holds.
    async fn next(
        &self,
        tx: &mut Transaction<'static, Any>,
        kind: EntityKind,
        entity_id: &str,
    ) -> Result<i64, String> {
        let row = sqlx::query(
            &self
                .db
                .q("SELECT MAX(revision) AS top FROM entity_revisions \
             WHERE project_id = ? AND entity_kind = ? AND entity_id = ?"),
        )
        .bind(self.project.as_str())
        .bind(kind.as_str())
        .bind(entity_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| e.to_string())?;
        let top: Option<i64> = row.try_get("top").map_err(|e| e.to_string())?;
        Ok(top.unwrap_or(0) + 1)
    }

    /// Every revision of one entity, newest first.
    pub async fn list(&self, kind: EntityKind, entity_id: &str) -> Result<Vec<Revision>, String> {
        let rows = sqlx::query(&self.db.q(
            "SELECT revision, payload, deleted, created_at FROM entity_revisions \
             WHERE project_id = ? AND entity_kind = ? AND entity_id = ? \
             ORDER BY revision DESC",
        ))
        .bind(self.project.as_str())
        .bind(kind.as_str())
        .bind(entity_id)
        .fetch_all(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        rows.iter().map(row_to_revision).collect()
    }

    /// One revision of one entity.
    pub async fn get(
        &self,
        kind: EntityKind,
        entity_id: &str,
        revision: i64,
    ) -> Result<Option<Revision>, String> {
        let row = sqlx::query(&self.db.q(
            "SELECT revision, payload, deleted, created_at FROM entity_revisions \
             WHERE project_id = ? AND entity_kind = ? AND entity_id = ? AND revision = ?",
        ))
        .bind(self.project.as_str())
        .bind(kind.as_str())
        .bind(entity_id)
        .bind(revision)
        .fetch_optional(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_revision).transpose()
    }
}

fn row_to_revision(row: &sqlx::any::AnyRow) -> Result<Revision, String> {
    Ok(Revision {
        revision: row.try_get("revision").map_err(|e| e.to_string())?,
        payload: row.try_get("payload").map_err(|e| e.to_string())?,
        // `i64`, not `bool`: the Any driver has no mapping for SQLite's BOOLEAN.
        deleted: row
            .try_get::<i64, _>("deleted")
            .map_err(|e| e.to_string())?
            != 0,
        created_at: row.try_get("created_at").map_err(|e| e.to_string())?,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    async fn store() -> RevisionStore {
        RevisionStore::new(
            crate::db::testing::db().await,
            crate::projects::ProjectId::new("1"),
        )
    }

    async fn append(s: &RevisionStore, id: &str, payload: &str, deleted: bool) -> i64 {
        let mut tx = s.db.begin_write().await.unwrap();
        let n = s
            .append(&mut tx, EntityKind::Agent, id, payload, deleted, "1")
            .await
            .unwrap();
        tx.commit().await.unwrap();
        n
    }

    #[test]
    fn an_absent_expectation_is_an_unconditional_write() {
        assert!(RevisionStore::check(Some(7), None).is_ok());
        assert!(RevisionStore::check(None, None).is_ok());
    }

    #[test]
    fn a_matching_expectation_passes_and_a_stale_one_does_not() {
        assert!(RevisionStore::check(Some(7), Some(7)).is_ok());
        assert_eq!(
            RevisionStore::check(Some(8), Some(7)),
            Err(CasError::Stale {
                expected: 7,
                actual: Some(8)
            })
        );
    }

    /// A caller naming a revision on a row that has never been versioned is
    /// claiming to know something that was never true. Accepting it would make
    /// the *first* concurrent write after the migration unprotected, which is
    /// exactly the window someone would rely on this in.
    #[test]
    fn expecting_a_revision_on_an_unversioned_row_is_refused() {
        assert_eq!(
            RevisionStore::check(None, Some(1)),
            Err(CasError::Stale {
                expected: 1,
                actual: None
            })
        );
    }

    #[tokio::test]
    async fn revisions_number_from_one_and_never_repeat() {
        let s = store().await;
        assert_eq!(append(&s, "a", "v1", false).await, 1);
        assert_eq!(append(&s, "a", "v2", false).await, 2);
        // A different entity has its own sequence.
        assert_eq!(append(&s, "b", "v1", false).await, 1);
    }

    /// Re-creating a deleted entity under the same name must not reuse numbers
    /// its history already holds — a restore addressed by number would then
    /// resolve to two different things.
    #[tokio::test]
    async fn a_recreated_entity_continues_its_old_numbering() {
        let s = store().await;
        append(&s, "a", "v1", false).await;
        append(&s, "a", "gone", true).await;
        assert_eq!(
            append(&s, "a", "back", false).await,
            3,
            "numbering continues past the deletion rather than restarting"
        );
    }

    #[tokio::test]
    async fn history_comes_back_newest_first_and_keeps_the_deletion() {
        let s = store().await;
        append(&s, "a", "v1", false).await;
        append(&s, "a", "v2", false).await;
        append(&s, "a", "v2", true).await;

        let all = s.list(EntityKind::Agent, "a").await.unwrap();
        assert_eq!(
            all.iter().map(|r| r.revision).collect::<Vec<_>>(),
            vec![3, 2, 1]
        );
        assert!(all[0].deleted, "the save that deleted is itself a revision");
        assert!(!all[1].deleted);
        assert_eq!(all[1].payload, "v2");
    }

    #[tokio::test]
    async fn one_revision_is_addressable_by_number() {
        let s = store().await;
        append(&s, "a", "v1", false).await;
        append(&s, "a", "v2", false).await;
        assert_eq!(
            s.get(EntityKind::Agent, "a", 1)
                .await
                .unwrap()
                .unwrap()
                .payload,
            "v1"
        );
        assert!(s.get(EntityKind::Agent, "a", 99).await.unwrap().is_none());
    }

    /// The two kinds share a table, so an id that collides across them must
    /// not collide in the history. A preset called "7" and memory 7 are
    /// different things.
    #[tokio::test]
    async fn the_two_kinds_do_not_share_a_namespace() {
        let s = store().await;
        let mut tx = s.db.begin_write().await.unwrap();
        s.append(&mut tx, EntityKind::Agent, "7", "a-preset", false, "1")
            .await
            .unwrap();
        s.append(&mut tx, EntityKind::Memory, "7", "a-memory", false, "1")
            .await
            .unwrap();
        tx.commit().await.unwrap();

        assert_eq!(
            s.list(EntityKind::Agent, "7").await.unwrap()[0].payload,
            "a-preset"
        );
        assert_eq!(
            s.list(EntityKind::Memory, "7").await.unwrap()[0].payload,
            "a-memory"
        );
    }
}
