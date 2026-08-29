//! Where an artifact's bytes live — and nothing else about it.
//!
//! [`ArtifactBlobs`] is deliberately the narrowest thing that can be true of a
//! byte store: put, get, delete. Metadata is *not* here. It stays in
//! [`super::store`], which queries and joins it, and that split is the whole
//! point — swapping S3/R2/GCS in later means writing these three methods
//! against an SDK and changing one line of wiring. Nothing else in the server
//! knows where the bytes are, because nothing else in the server can ask.
//!
//! **There is no presigned-URL method, on purpose.** It is the obvious fourth
//! method and it has no caller today: horsie serves artifact bytes through its
//! own authenticated route, and a `DbBlobs` deployment has no URL to sign. A
//! trait method with one implementation that returns "unsupported" is a lie in
//! the type system. Adding a *defaulted* method later is not a breaking change
//! to any implementor, so the cost of waiting is zero and the cost of guessing
//! now is a permanent hole in the abstraction.
//!
//! ## How [`DbBlobs`] shares a row with the metadata store
//!
//! `artifacts.bytes` is a column of the same row the metadata lives in, so the
//! two writers meet. They are kept apart by column ownership rather than by
//! ordering: `DbBlobs` upserts only `bytes` (plus the columns a row cannot be
//! created without), `ArtifactStore::insert` upserts only the metadata columns,
//! and neither's `ON CONFLICT` clause touches the other's. Either may run
//! first, twice, or concurrently, and the row converges to the same state.
//!
//! `delete` drops the row outright — in this backend "the bytes are gone" and
//! "the row is gone" are the same statement. An object-store backend would
//! delete an object and leave the row to `release_session`, which has usually
//! deleted it already.

use crate::db::Db;
use crate::projects::ProjectId;
use async_trait::async_trait;
use sqlx::Row;

/// Which bytes: the project that owns them and the artifact that names them.
///
/// The project is part of the key rather than an ambient scope so that no
/// implementation *can* be written that reads across projects — an object-store
/// backend derives its object path from both halves, and the SQL backend binds
/// both into its predicate.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BlobKey {
    pub project: ProjectId,
    /// Lowercase-hex sha256 of the bytes.
    pub artifact_id: String,
}

impl BlobKey {
    pub fn new(project: ProjectId, artifact_id: impl Into<String>) -> Self {
        Self {
            project,
            artifact_id: artifact_id.into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    /// No bytes under this key. Distinct from a backend failure because a
    /// caller answers them differently: one is a 404, the other a 500.
    #[error("no artifact bytes for '{0}'")]
    NotFound(String),
    #[error("blob storage: {0}")]
    Backend(String),
}

/// The bytes of an artifact, wherever they are kept.
#[async_trait]
pub trait ArtifactBlobs: Send + Sync {
    /// Store `bytes` under `key`. Idempotent: an id *is* the hash of its bytes,
    /// so writing the same key twice writes the same bytes.
    async fn put(&self, key: &BlobKey, bytes: &[u8]) -> Result<(), BlobError>;

    async fn get(&self, key: &BlobKey) -> Result<Vec<u8>, BlobError>;

    /// Delete every key given. Takes a slice rather than one key because the
    /// only caller is session release, which frees a batch — a per-key API
    /// would make that N round trips against a remote store.
    ///
    /// Absent keys are not an error: release is retryable, and a second run
    /// must succeed.
    async fn delete(&self, keys: &[BlobKey]) -> Result<(), BlobError>;
}

/// Bytes in the `artifacts.bytes` column.
///
/// The default backend, and the only one a self-hoster needs: it inherits the
/// database's durability and backups, and it works on a host with an ephemeral
/// disk and in a multi-node cluster, where a local file is invisible to the
/// node that serves the next request.
#[derive(Clone)]
pub struct DbBlobs {
    db: Db,
}

impl DbBlobs {
    pub fn new(db: Db) -> Self {
        Self { db }
    }
}

fn backend(e: sqlx::Error) -> BlobError {
    BlobError::Backend(e.to_string())
}

#[async_trait]
impl ArtifactBlobs for DbBlobs {
    async fn put(&self, key: &BlobKey, bytes: &[u8]) -> Result<(), BlobError> {
        // `media_type`, `kind` and `created_at` are NOT NULL, so a row cannot be
        // created without them. They are placeholders only on the insert path
        // and are never in the `DO UPDATE` list: whichever of the two writers
        // arrives second leaves the other's columns exactly as it found them.
        let sql = format!(
            "INSERT INTO artifacts \
               (project_id, id, media_type, kind, byte_size, bytes, created_at) \
             VALUES (?, ?, '', '', ?, ?, {now}) \
             ON CONFLICT (project_id, id) DO UPDATE SET bytes = excluded.bytes",
            now = self.db.now_text()
        );
        let mut tx = self.db.begin_write().await.map_err(backend)?;
        sqlx::query(&self.db.q(&sql))
            .bind(key.project.as_str())
            .bind(&key.artifact_id)
            .bind(i64::try_from(bytes.len()).unwrap_or(i64::MAX))
            .bind(bytes.to_vec())
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        Ok(())
    }

    async fn get(&self, key: &BlobKey) -> Result<Vec<u8>, BlobError> {
        let row = sqlx::query(
            &self
                .db
                .q("SELECT bytes FROM artifacts WHERE project_id = ? AND id = ?"),
        )
        .bind(key.project.as_str())
        .bind(&key.artifact_id)
        .fetch_optional(self.db.pool())
        .await
        .map_err(backend)?
        .ok_or_else(|| BlobError::NotFound(key.artifact_id.clone()))?;
        row.try_get::<Vec<u8>, _>("bytes").map_err(backend)
    }

    async fn delete(&self, keys: &[BlobKey]) -> Result<(), BlobError> {
        if keys.is_empty() {
            return Ok(());
        }
        let mut tx = self.db.begin_write().await.map_err(backend)?;
        for key in keys {
            sqlx::query(
                &self
                    .db
                    .q("DELETE FROM artifacts WHERE project_id = ? AND id = ?"),
            )
            .bind(key.project.as_str())
            .bind(&key.artifact_id)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        }
        tx.commit().await.map_err(backend)?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use crate::db::testing;

    fn key(project: &str, id: &str) -> BlobKey {
        BlobKey::new(ProjectId::new(project), id)
    }

    #[tokio::test]
    async fn bytes_round_trip() {
        let blobs = DbBlobs::new(testing::db().await);
        let k = key("p1", "abc");
        blobs.put(&k, b"\x00\x01\x02hello").await.unwrap();
        assert_eq!(blobs.get(&k).await.unwrap(), b"\x00\x01\x02hello".to_vec());
    }

    #[tokio::test]
    async fn a_missing_key_is_not_found_rather_than_a_backend_error() {
        let blobs = DbBlobs::new(testing::db().await);
        match blobs.get(&key("p1", "nope")).await {
            Err(BlobError::NotFound(id)) => assert_eq!(id, "nope"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    /// Another project's id must not resolve, whatever it is.
    #[tokio::test]
    async fn a_key_is_scoped_to_its_project() {
        let blobs = DbBlobs::new(testing::db().await);
        blobs.put(&key("p1", "shared"), b"secret").await.unwrap();
        assert!(matches!(
            blobs.get(&key("p2", "shared")).await,
            Err(BlobError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn putting_the_same_key_twice_is_idempotent() {
        let blobs = DbBlobs::new(testing::db().await);
        let k = key("p1", "abc");
        blobs.put(&k, b"same").await.unwrap();
        blobs.put(&k, b"same").await.unwrap();
        assert_eq!(blobs.get(&k).await.unwrap(), b"same".to_vec());
    }

    #[tokio::test]
    async fn delete_removes_the_named_keys_and_tolerates_absent_ones() {
        let blobs = DbBlobs::new(testing::db().await);
        blobs.put(&key("p1", "a"), b"one").await.unwrap();
        blobs.put(&key("p1", "b"), b"two").await.unwrap();

        blobs
            .delete(&[key("p1", "a"), key("p1", "gone-already")])
            .await
            .unwrap();

        assert!(matches!(
            blobs.get(&key("p1", "a")).await,
            Err(BlobError::NotFound(_))
        ));
        assert_eq!(blobs.get(&key("p1", "b")).await.unwrap(), b"two".to_vec());
        // Retryable: a second release must not fail.
        blobs.delete(&[key("p1", "a")]).await.unwrap();
    }

    #[tokio::test]
    async fn deleting_nothing_touches_nothing() {
        let blobs = DbBlobs::new(testing::db().await);
        blobs.put(&key("p1", "a"), b"one").await.unwrap();
        blobs.delete(&[]).await.unwrap();
        assert_eq!(blobs.get(&key("p1", "a")).await.unwrap(), b"one".to_vec());
    }
}
