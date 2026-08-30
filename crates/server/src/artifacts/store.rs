//! The `artifacts` metadata row and the `artifact_uses` reference table.
//!
//! Everything except the bytes: what an artifact is, how big it is, who called
//! it what, and which sessions still need it. This is the half that is queried
//! and joined, and it stays in SQL wherever the bytes end up — see
//! [`super::blobs`] for why the split exists at all.
//!
//! [`ArtifactRow`] is a *storage* type and deliberately not
//! [`horsie_models::agent::ArtifactRef`]: the row carries `created_at`, which
//! no client is told, and it names its shape with a column value rather than a
//! tagged union. `ArtifactRow::to_ref` is the one place the two meet.

use crate::db::Db;
use crate::projects::ProjectId;
use horsie_models::agent::{ArtifactKind, ArtifactRef, DocumentArtifact, ImageArtifact};
use sqlx::Row;
use sqlx::any::AnyRow;

/// Every metadata column, in the order [`row_to_artifact`] reads them.
const COLS: &str = "id, media_type, kind, byte_size, width, height, filename, created_at";

/// What shape an artifact is.
///
/// The `kind` column's whole vocabulary, as a type: a row cannot hold a third
/// value and reach the rest of the server, because reading one is an error
/// here. That is what makes [`ArtifactRow::to_ref`] total — no fallback arm
/// inventing a kind for a corrupt row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactShape {
    Image,
    Document,
}

impl ArtifactShape {
    /// The `kind` column's value. Matches `Sniffed::kind`, which is what wrote
    /// it.
    pub fn as_str(self) -> &'static str {
        match self {
            ArtifactShape::Image => "image",
            ArtifactShape::Document => "document",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "image" => Some(ArtifactShape::Image),
            "document" => Some(ArtifactShape::Document),
            _ => None,
        }
    }

    /// What the sniffer said these bytes were.
    pub fn of(sniffed: super::media::Sniffed) -> Self {
        if sniffed.is_image() {
            ArtifactShape::Image
        } else {
            ArtifactShape::Document
        }
    }
}

/// One row of the `artifacts` table, bytes excluded.
#[derive(Clone, Debug, PartialEq)]
pub struct ArtifactRow {
    /// Lowercase-hex sha256 of the bytes.
    pub id: String,
    /// The *sniffed* type, never a caller's claim.
    pub media_type: String,
    pub shape: ArtifactShape,
    pub byte_size: u64,
    /// Images only, and `None` for an image whose header would not parse.
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub filename: Option<String>,
    pub created_at: String,
}

impl ArtifactRow {
    /// The wire reference for this row.
    ///
    /// Total by construction: `shape` is already narrowed, so there is no arm
    /// that has to guess.
    #[must_use]
    pub fn to_ref(&self) -> ArtifactRef {
        let kind = match self.shape {
            ArtifactShape::Image => ArtifactKind::Image(ImageArtifact {
                width: self.width,
                height: self.height,
            }),
            ArtifactShape::Document => ArtifactKind::Document(DocumentArtifact {}),
        };
        ArtifactRef {
            id: self.id.clone(),
            media_type: self.media_type.clone(),
            kind,
            byte_size: self.byte_size,
            filename: self.filename.clone(),
        }
    }
}

/// Metadata and reference counting for one deployment's artifacts.
///
/// Every method takes the project explicitly. The service above is shared
/// across projects — it holds one cache and one blob backend for the whole
/// process — so binding a project into the store would mean building one store
/// per project per request.
#[derive(Clone)]
pub struct ArtifactStore {
    db: Db,
}

impl ArtifactStore {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// Write the metadata for an artifact.
    ///
    /// An upsert, because storing is content-addressed: re-pasting the same
    /// screenshot must be a no-op, not a primary-key error. Only the metadata
    /// columns are updated — `bytes` belongs to [`super::blobs::DbBlobs`] and
    /// is never named here, so the two writers cannot clobber each other
    /// whichever order they run in.
    ///
    /// The `bytes` placeholder on the insert path exists because the column is
    /// NOT NULL; the blob write fills it.
    pub async fn insert(&self, project: &ProjectId, row: &ArtifactRow) -> Result<(), String> {
        let sql = format!(
            "INSERT INTO artifacts \
               (project_id, id, media_type, kind, byte_size, width, height, filename, \
                bytes, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, {now}) \
             ON CONFLICT (project_id, id) DO UPDATE SET \
               media_type = excluded.media_type, \
               kind = excluded.kind, \
               byte_size = excluded.byte_size, \
               width = excluded.width, \
               height = excluded.height, \
               filename = excluded.filename",
            now = self.db.now_text()
        );
        let mut tx = self.db.begin_write().await.map_err(|e| e.to_string())?;
        sqlx::query(&self.db.q(&sql))
            .bind(project.as_str())
            .bind(&row.id)
            .bind(&row.media_type)
            .bind(row.shape.as_str())
            .bind(i64::try_from(row.byte_size).unwrap_or(i64::MAX))
            .bind(row.width.map(i64::from))
            .bind(row.height.map(i64::from))
            .bind(row.filename.as_deref())
            .bind(Vec::<u8>::new())
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("store artifact '{}': {e}", row.id))?;
        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn get_meta(
        &self,
        project: &ProjectId,
        id: &str,
    ) -> Result<Option<ArtifactRow>, String> {
        let row = sqlx::query(&self.db.q(&format!(
            "SELECT {COLS} FROM artifacts WHERE project_id = ? AND id = ?"
        )))
        .bind(project.as_str())
        .bind(id)
        .fetch_optional(self.db.pool())
        .await
        .map_err(|e| e.to_string())?;
        row.as_ref().map(row_to_artifact).transpose()
    }

    /// Which of `ids` exist in this project.
    ///
    /// The predicate is the point: a message naming an artifact from another
    /// project gets that id back as *missing*, so the caller rejects it without
    /// ever learning whether it exists elsewhere.
    pub async fn exists(&self, project: &ProjectId, ids: &[String]) -> Result<Vec<String>, String> {
        if ids.is_empty() {
            // `IN ()` is a syntax error in both dialects, and the answer is
            // known without asking.
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; ids.len()].join(", ");
        let mut query = sqlx::query(&self.db.q(&format!(
            "SELECT id FROM artifacts WHERE project_id = ? AND id IN ({placeholders})"
        )))
        .bind(project.as_str());
        for id in ids {
            query = query.bind(id);
        }
        let rows = query
            .fetch_all(self.db.pool())
            .await
            .map_err(|e| e.to_string())?;
        rows.iter()
            .map(|r| r.try_get::<String, _>("id").map_err(|e| e.to_string()))
            .collect()
    }

    /// Record that `session_id` needs this artifact.
    ///
    /// Idempotent: a session that re-sends the same image, or a replayed
    /// command, must not fail and must not double-count. The table is a set of
    /// references, not a counter, which is what makes that possible.
    pub async fn mark_used(
        &self,
        project: &ProjectId,
        artifact_id: &str,
        session_id: &str,
    ) -> Result<(), String> {
        let mut tx = self.db.begin_write().await.map_err(|e| e.to_string())?;
        sqlx::query(&self.db.q(
            "INSERT INTO artifact_uses (project_id, artifact_id, session_id) VALUES (?, ?, ?) \
             ON CONFLICT (project_id, artifact_id, session_id) DO NOTHING",
        ))
        .bind(project.as_str())
        .bind(artifact_id)
        .bind(session_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("mark artifact '{artifact_id}' used: {e}"))?;
        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Drop this session's references, and with them any artifact nothing else
    /// still refers to. Returns the ids deleted, so the caller can free their
    /// bytes.
    ///
    /// The candidate set is *this session's* artifacts and nothing wider. A
    /// blanket "delete every artifact with no uses" would also delete one that
    /// has just been uploaded and not yet attached to a message — there is a
    /// window between `put` and `mark_used` in every send.
    ///
    /// One write transaction: reading the candidates, deleting the uses and
    /// deleting the artifacts have to see the same reference set, or a
    /// concurrent `mark_used` lands between the last two and its artifact is
    /// deleted out from under it.
    pub async fn release_session(
        &self,
        project: &ProjectId,
        session_id: &str,
    ) -> Result<Vec<String>, String> {
        let mut tx = self.db.begin_write().await.map_err(|e| e.to_string())?;

        let candidates: Vec<String> =
            sqlx::query(&self.db.q(
                "SELECT artifact_id FROM artifact_uses WHERE project_id = ? AND session_id = ?",
            ))
            .bind(project.as_str())
            .bind(session_id)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| e.to_string())?
            .iter()
            .map(|r| {
                r.try_get::<String, _>("artifact_id")
                    .map_err(|e| e.to_string())
            })
            .collect::<Result<_, _>>()?;

        sqlx::query(
            &self
                .db
                .q("DELETE FROM artifact_uses WHERE project_id = ? AND session_id = ?"),
        )
        .bind(project.as_str())
        .bind(session_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

        if candidates.is_empty() {
            tx.commit().await.map_err(|e| e.to_string())?;
            return Ok(Vec::new());
        }

        let placeholders = vec!["?"; candidates.len()].join(", ");
        let mut query = sqlx::query(&self.db.q(&format!(
            "DELETE FROM artifacts WHERE project_id = ? AND id IN ({placeholders}) \
             AND NOT EXISTS (SELECT 1 FROM artifact_uses u \
               WHERE u.project_id = artifacts.project_id AND u.artifact_id = artifacts.id) \
             RETURNING id"
        )))
        .bind(project.as_str());
        for id in &candidates {
            query = query.bind(id);
        }
        let deleted: Vec<String> = query
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| e.to_string())?
            .iter()
            .map(|r| r.try_get::<String, _>("id").map_err(|e| e.to_string()))
            .collect::<Result<_, _>>()?;

        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(deleted)
    }
}

fn row_to_artifact(row: &AnyRow) -> Result<ArtifactRow, String> {
    let kind: String = row.try_get("kind").map_err(|e| e.to_string())?;
    let shape = ArtifactShape::parse(&kind)
        .ok_or_else(|| format!("artifact row has an unknown kind '{kind}'"))?;
    // INTEGER on SQLite and BIGINT on PostgreSQL, so i64 either way; the
    // service caps a size long before it could be negative or overflow.
    let byte_size: i64 = row.try_get("byte_size").map_err(|e| e.to_string())?;
    let dimension = |name: &str| -> Result<Option<u32>, String> {
        Ok(row
            .try_get::<Option<i64>, _>(name)
            .map_err(|e| e.to_string())?
            .and_then(|v| u32::try_from(v).ok()))
    };
    Ok(ArtifactRow {
        id: row.try_get("id").map_err(|e| e.to_string())?,
        media_type: row.try_get("media_type").map_err(|e| e.to_string())?,
        shape,
        byte_size: u64::try_from(byte_size).unwrap_or_default(),
        width: dimension("width")?,
        height: dimension("height")?,
        filename: row.try_get("filename").map_err(|e| e.to_string())?,
        created_at: row.try_get("created_at").map_err(|e| e.to_string())?,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::db::testing;

    fn project(name: &str) -> ProjectId {
        ProjectId::new(name)
    }

    fn image(id: &str) -> ArtifactRow {
        ArtifactRow {
            id: id.to_string(),
            media_type: "image/png".into(),
            shape: ArtifactShape::Image,
            byte_size: 27,
            width: Some(16),
            height: Some(32),
            filename: Some("shot.png".into()),
            created_at: String::new(),
        }
    }

    fn document(id: &str) -> ArtifactRow {
        ArtifactRow {
            id: id.to_string(),
            media_type: "application/pdf".into(),
            shape: ArtifactShape::Document,
            byte_size: 9,
            width: None,
            height: None,
            filename: None,
            created_at: String::new(),
        }
    }

    #[tokio::test]
    async fn a_row_round_trips() {
        let store = ArtifactStore::new(testing::db().await);
        let p = project("p1");
        store.insert(&p, &image("aaa")).await.unwrap();

        let got = store.get_meta(&p, "aaa").await.unwrap().unwrap();
        assert_eq!(got.media_type, "image/png");
        assert_eq!(got.shape, ArtifactShape::Image);
        assert_eq!(got.byte_size, 27);
        assert_eq!((got.width, got.height), (Some(16), Some(32)));
        assert_eq!(got.filename.as_deref(), Some("shot.png"));
        assert!(!got.created_at.is_empty(), "the store stamps this");
    }

    #[tokio::test]
    async fn a_document_keeps_its_null_dimensions() {
        let store = ArtifactStore::new(testing::db().await);
        let p = project("p1");
        store.insert(&p, &document("bbb")).await.unwrap();
        let got = store.get_meta(&p, "bbb").await.unwrap().unwrap();
        assert_eq!(got.shape, ArtifactShape::Document);
        assert_eq!((got.width, got.height, got.filename), (None, None, None));
    }

    #[tokio::test]
    async fn inserting_the_same_id_twice_is_an_upsert_not_an_error() {
        let store = ArtifactStore::new(testing::db().await);
        let p = project("p1");
        store.insert(&p, &image("aaa")).await.unwrap();
        store.insert(&p, &image("aaa")).await.unwrap();

        let count: i64 = sqlx::query_scalar(
            &store
                .db
                .q("SELECT COUNT(*) FROM artifacts WHERE project_id = ?"),
        )
        .bind(p.as_str())
        .fetch_one(store.db.pool())
        .await
        .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn get_meta_is_scoped_to_the_project() {
        let store = ArtifactStore::new(testing::db().await);
        store.insert(&project("p1"), &image("aaa")).await.unwrap();
        assert!(
            store
                .get_meta(&project("p2"), "aaa")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn exists_answers_only_for_this_project() {
        let store = ArtifactStore::new(testing::db().await);
        store.insert(&project("p1"), &image("mine")).await.unwrap();
        store
            .insert(&project("p2"), &image("theirs"))
            .await
            .unwrap();

        let found = store
            .exists(
                &project("p1"),
                &["mine".into(), "theirs".into(), "nowhere".into()],
            )
            .await
            .unwrap();
        assert_eq!(found, vec!["mine".to_string()]);

        assert!(store.exists(&project("p1"), &[]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn mark_used_is_idempotent() {
        let store = ArtifactStore::new(testing::db().await);
        let p = project("p1");
        store.insert(&p, &image("aaa")).await.unwrap();
        store.mark_used(&p, "aaa", "s1").await.unwrap();
        store.mark_used(&p, "aaa", "s1").await.unwrap();

        let count: i64 = sqlx::query_scalar(
            &store
                .db
                .q("SELECT COUNT(*) FROM artifact_uses WHERE project_id = ?"),
        )
        .bind(p.as_str())
        .fetch_one(store.db.pool())
        .await
        .unwrap();
        assert_eq!(count, 1);
    }

    /// The whole reason `artifact_uses` exists: a shared artifact outlives the
    /// first session that releases it.
    #[tokio::test]
    async fn release_deletes_the_unreferenced_and_keeps_the_shared() {
        let store = ArtifactStore::new(testing::db().await);
        let p = project("p1");
        store.insert(&p, &image("shared")).await.unwrap();
        store.insert(&p, &image("only-s1")).await.unwrap();
        store.mark_used(&p, "shared", "s1").await.unwrap();
        store.mark_used(&p, "shared", "s2").await.unwrap();
        store.mark_used(&p, "only-s1", "s1").await.unwrap();

        let deleted = store.release_session(&p, "s1").await.unwrap();
        assert_eq!(deleted, vec!["only-s1".to_string()]);
        assert!(store.get_meta(&p, "only-s1").await.unwrap().is_none());
        assert!(store.get_meta(&p, "shared").await.unwrap().is_some());

        // And when the last session goes, so does the artifact.
        let deleted = store.release_session(&p, "s2").await.unwrap();
        assert_eq!(deleted, vec!["shared".to_string()]);
        assert!(store.get_meta(&p, "shared").await.unwrap().is_none());
    }

    /// A just-uploaded artifact that no session has claimed yet must survive
    /// somebody else's release.
    #[tokio::test]
    async fn release_never_touches_an_artifact_outside_the_session() {
        let store = ArtifactStore::new(testing::db().await);
        let p = project("p1");
        store.insert(&p, &image("just-uploaded")).await.unwrap();
        store.insert(&p, &image("s1s")).await.unwrap();
        store.mark_used(&p, "s1s", "s1").await.unwrap();

        let deleted = store.release_session(&p, "s1").await.unwrap();
        assert_eq!(deleted, vec!["s1s".to_string()]);
        assert!(store.get_meta(&p, "just-uploaded").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn releasing_a_session_with_no_artifacts_is_nothing() {
        let store = ArtifactStore::new(testing::db().await);
        assert!(
            store
                .release_session(&project("p1"), "never-used-one")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_row_renders_the_wire_reference_for_its_shape() {
        let r = image("aaa").to_ref();
        assert_eq!(r.id, "aaa");
        assert_eq!(r.byte_size, 27);
        assert_eq!(
            r.kind,
            ArtifactKind::Image(ImageArtifact {
                width: Some(16),
                height: Some(32)
            })
        );
        assert_eq!(
            document("bbb").to_ref().kind,
            ArtifactKind::Document(DocumentArtifact {})
        );
    }

    #[test]
    fn a_shape_round_trips_through_its_column_value() {
        for shape in [ArtifactShape::Image, ArtifactShape::Document] {
            assert_eq!(ArtifactShape::parse(shape.as_str()), Some(shape));
        }
        assert_eq!(ArtifactShape::parse("audio"), None);
    }
}
