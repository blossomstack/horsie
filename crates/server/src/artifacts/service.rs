//! The artifact service: the only thing outside this module anyone calls.
//!
//! Three collaborators sit behind it — the metadata rows, the bytes, and an
//! in-memory cache of the bytes — and no caller is given any of them. An HTTP
//! handler asks for "the bytes of this id in this project" and cannot express
//! "the bytes of this id" without a project, cannot store bytes without them
//! being identified first, and cannot say what type they are.
//!
//! **Nothing here takes a declared media type, and that is the design.** There
//! is no parameter for it, so no caller can pass one and no future caller can
//! start trusting one. A browser's `Content-Type` and an MCP block's `mimeType`
//! are claims about bytes the server is holding, and [`super::media::sniff`]
//! answers with the bytes themselves.

use super::blobs::{ArtifactBlobs, BlobError, BlobKey, DbBlobs};
use super::cache::ArtifactCache;
use super::media;
use super::store::{ArtifactRow, ArtifactShape, ArtifactStore};
use crate::db::Db;
use crate::projects::ProjectId;
use horsie_models::agent::ArtifactRef;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;

/// The largest artifact horsie will store.
///
/// 10 MB. Not a storage limit — it is a *provider* limit: Anthropic caps an
/// image at 5 MB and a PDF at 32 MB, OpenAI at 20 MB, and an artifact that
/// cannot be sent to a model is one the user is told about at upload time
/// rather than three turns later. The cap is also what keeps `byte_size`
/// arithmetic below trivially in range.
pub const MAX_ARTIFACT_BYTES: usize = 10 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    /// The bytes are not a type horsie stores. Names what was rejected: a user
    /// who dragged in the wrong file needs to know which one.
    #[error(
        "{name} is not a file horsie can store (it begins {prefix}); \
         supported types are PNG, JPEG, GIF, WebP and PDF"
    )]
    UnsupportedType { name: String, prefix: String },
    #[error("{name} is {size} bytes, over the {limit}-byte limit")]
    TooLarge {
        name: String,
        size: usize,
        limit: usize,
    },
    /// No such artifact *in this project*. A caller is never told whether the
    /// id exists somewhere else.
    #[error("no artifact '{id}'")]
    NotFound { id: String },
    #[error("artifact storage: {0}")]
    Storage(String),
}

/// Store and fetch the images and documents a conversation carries.
///
/// Cheap to clone-by-`Arc` and shared process-wide: one cache and one blob
/// backend serve every project, and the project travels in each call rather
/// than in the handle.
pub struct ArtifactService {
    store: ArtifactStore,
    blobs: Arc<dyn ArtifactBlobs>,
    cache: Arc<ArtifactCache>,
}

impl ArtifactService {
    pub fn new(db: Db, blobs: Arc<dyn ArtifactBlobs>, cache: Arc<ArtifactCache>) -> Self {
        Self {
            store: ArtifactStore::new(db),
            blobs,
            cache,
        }
    }

    /// The default wiring: bytes in the database, a default-sized cache.
    pub fn in_database(db: Db) -> Self {
        let blobs = Arc::new(DbBlobs::new(db.clone()));
        Self::new(db, blobs, Arc::new(ArtifactCache::default()))
    }

    /// Store `bytes` and return the reference a message will carry.
    ///
    /// Content-addressed: the same bytes stored twice produce the same id and
    /// one row, so a re-pasted screenshot costs nothing. `filename` is what the
    /// client called it and is used only for display — never to decide what the
    /// file is.
    pub async fn put(
        &self,
        project: &ProjectId,
        bytes: Vec<u8>,
        filename: Option<String>,
    ) -> Result<ArtifactRef, ArtifactError> {
        let name = display_name(filename.as_deref());
        // Size before type: refusing a huge upload should not depend on
        // understanding it, and the answer "too large" is the more useful one.
        if bytes.len() > MAX_ARTIFACT_BYTES {
            return Err(ArtifactError::TooLarge {
                name,
                size: bytes.len(),
                limit: MAX_ARTIFACT_BYTES,
            });
        }
        let Some(sniffed) = media::sniff(&bytes) else {
            return Err(ArtifactError::UnsupportedType {
                name,
                prefix: leading_bytes(&bytes),
            });
        };
        let dimensions = if sniffed.is_image() {
            media::dimensions(sniffed, &bytes)
        } else {
            None
        };

        let row = ArtifactRow {
            id: sha256_hex(&bytes),
            media_type: sniffed.media_type().to_string(),
            shape: ArtifactShape::of(sniffed),
            byte_size: bytes.len() as u64,
            width: dimensions.map(|(w, _)| w),
            height: dimensions.map(|(_, h)| h),
            filename,
            // Stamped by the database on insert; nothing reads it back here.
            created_at: String::new(),
        };
        let key = BlobKey::new(project.clone(), &row.id);

        // Bytes first. If the metadata write then fails, the row is rolled back
        // below and the artifact is simply absent — where the other order would
        // announce an artifact whose bytes never arrived.
        self.blobs.put(&key, &bytes).await.map_err(storage)?;
        if let Err(e) = self.store.insert(project, &row).await {
            // Best effort: the compensating delete failing leaves orphaned
            // bytes, which release and GC handle, and reporting *its* error
            // would hide the one that actually happened.
            let _ = self.blobs.delete(&[key]).await;
            return Err(ArtifactError::Storage(e));
        }
        self.cache.insert(key_of(project, &row.id), Arc::new(bytes));
        Ok(row.to_ref())
    }

    /// The bytes of an artifact.
    ///
    /// Cache first, and safely so: the cache is keyed by project as well as id,
    /// so a hit is already proof the artifact was reached through this project.
    pub async fn get(&self, project: &ProjectId, id: &str) -> Result<Vec<u8>, ArtifactError> {
        let key = key_of(project, id);
        if let Some(hit) = self.cache.get(&key) {
            return Ok(hit.as_ref().clone());
        }
        let bytes = match self.blobs.get(&key).await {
            Ok(bytes) => bytes,
            Err(BlobError::NotFound(id)) => return Err(ArtifactError::NotFound { id }),
            Err(e) => return Err(storage(e)),
        };
        let bytes = Arc::new(bytes);
        self.cache.insert(key, Arc::clone(&bytes));
        Ok(bytes.as_ref().clone())
    }

    /// The bytes of several artifacts, for hydrating one provider request.
    ///
    /// Strict: a missing id is an error rather than an absent map entry. A
    /// tolerant version would send the model a conversation with an image
    /// silently removed, and it would answer confidently about nothing.
    pub async fn resolve(
        &self,
        project: &ProjectId,
        ids: &[String],
    ) -> Result<HashMap<String, Vec<u8>>, ArtifactError> {
        let mut out = HashMap::with_capacity(ids.len());
        for id in ids {
            if out.contains_key(id) {
                continue;
            }
            out.insert(id.clone(), self.get(project, id).await?);
        }
        Ok(out)
    }

    /// Metadata without the bytes — what a listing or a header needs.
    pub async fn meta(
        &self,
        project: &ProjectId,
        id: &str,
    ) -> Result<Option<ArtifactRow>, ArtifactError> {
        self.store.get_meta(project, id).await.map_err(storage)
    }

    /// Which of these ids this project actually has. The caller rejects the
    /// rest — this is how a message naming another project's artifact is
    /// refused.
    pub async fn exists(
        &self,
        project: &ProjectId,
        ids: &[String],
    ) -> Result<Vec<String>, ArtifactError> {
        self.store.exists(project, ids).await.map_err(storage)
    }

    /// Record that a session needs this artifact. Idempotent.
    pub async fn mark_used(
        &self,
        project: &ProjectId,
        artifact_id: &str,
        session_id: &str,
    ) -> Result<(), ArtifactError> {
        self.store
            .mark_used(project, artifact_id, session_id)
            .await
            .map_err(storage)
    }

    /// Release a deleted session's artifacts: drop its references, then the
    /// bytes of anything no other session still needs. Returns the ids that
    /// went.
    pub async fn release_session(
        &self,
        project: &ProjectId,
        session_id: &str,
    ) -> Result<Vec<String>, ArtifactError> {
        let deleted = self
            .store
            .release_session(project, session_id)
            .await
            .map_err(storage)?;
        let keys: Vec<BlobKey> = deleted.iter().map(|id| key_of(project, id)).collect();
        self.blobs.delete(&keys).await.map_err(storage)?;
        self.cache.forget(&keys);
        Ok(deleted)
    }
}

fn key_of(project: &ProjectId, id: &str) -> BlobKey {
    BlobKey::new(project.clone(), id)
}

fn storage(e: impl std::fmt::Display) -> ArtifactError {
    ArtifactError::Storage(e.to_string())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// What to call the thing being refused. A paste has no filename, so it gets a
/// phrase rather than an empty pair of quotes.
fn display_name(filename: Option<&str>) -> String {
    match filename {
        Some(name) if !name.is_empty() => format!("'{name}'"),
        _ => "this upload".to_string(),
    }
}

/// The first few bytes, escaped, so a rejection says something checkable.
///
/// Bounded and escaped rather than raw: these bytes came off the network and
/// end up in a log line and an error message.
fn leading_bytes(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "nothing at all".to_string();
    }
    let shown: String = bytes
        .iter()
        .take(8)
        .flat_map(|b| std::ascii::escape_default(*b))
        .map(char::from)
        .collect();
    format!("\"{shown}\"")
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
    use async_trait::async_trait;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A blob backend that counts what it is asked for, so a test can prove the
    /// cache actually spared it a read.
    #[derive(Default)]
    struct CountingBlobs {
        stored: Mutex<HashMap<BlobKey, Vec<u8>>>,
        gets: AtomicUsize,
        puts: AtomicUsize,
    }

    impl CountingBlobs {
        fn gets(&self) -> usize {
            self.gets.load(Ordering::SeqCst)
        }
        fn puts(&self) -> usize {
            self.puts.load(Ordering::SeqCst)
        }
        fn len(&self) -> usize {
            self.stored.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl ArtifactBlobs for CountingBlobs {
        async fn put(&self, key: &BlobKey, bytes: &[u8]) -> Result<(), BlobError> {
            self.puts.fetch_add(1, Ordering::SeqCst);
            self.stored
                .lock()
                .unwrap()
                .insert(key.clone(), bytes.to_vec());
            Ok(())
        }
        async fn get(&self, key: &BlobKey) -> Result<Vec<u8>, BlobError> {
            self.gets.fetch_add(1, Ordering::SeqCst);
            self.stored
                .lock()
                .unwrap()
                .get(key)
                .cloned()
                .ok_or_else(|| BlobError::NotFound(key.artifact_id.clone()))
        }
        async fn delete(&self, keys: &[BlobKey]) -> Result<(), BlobError> {
            let mut stored = self.stored.lock().unwrap();
            for key in keys {
                stored.remove(key);
            }
            Ok(())
        }
    }

    /// A service over a real database and a counting blob backend.
    async fn service() -> (ArtifactService, Arc<CountingBlobs>, Db) {
        let db = testing::db().await;
        let blobs = Arc::new(CountingBlobs::default());
        let service = ArtifactService::new(
            db.clone(),
            Arc::clone(&blobs) as Arc<dyn ArtifactBlobs>,
            Arc::new(ArtifactCache::default()),
        );
        (service, blobs, db)
    }

    fn project() -> ProjectId {
        ProjectId::new("p1")
    }

    /// A real 1x1 PNG.
    fn png() -> Vec<u8> {
        vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00,
        ]
    }

    #[tokio::test]
    async fn put_then_get_returns_the_same_bytes() {
        let (service, _, _db) = service().await;
        let stored = service.put(&project(), png(), None).await.unwrap();
        assert_eq!(service.get(&project(), &stored.id).await.unwrap(), png());
    }

    #[tokio::test]
    async fn a_stored_image_carries_its_type_and_dimensions() {
        let (service, _, _db) = service().await;
        let r = service
            .put(&project(), png(), Some("shot.png".into()))
            .await
            .unwrap();
        assert_eq!(r.media_type, "image/png");
        assert_eq!(r.byte_size, png().len() as u64);
        assert_eq!(r.filename.as_deref(), Some("shot.png"));
        match r.kind {
            horsie_models::agent::ArtifactKind::Image(img) => {
                assert_eq!((img.width, img.height), (Some(1), Some(1)));
            }
            horsie_models::agent::ArtifactKind::Document(_) => panic!("a PNG is an image"),
        }
    }

    #[tokio::test]
    async fn the_id_is_the_sha256_of_the_bytes() {
        let (service, _, _db) = service().await;
        let r = service.put(&project(), png(), None).await.unwrap();
        assert_eq!(r.id, sha256_hex(&png()));
    }

    /// Content addressing: the same screenshot pasted twice is one row.
    #[tokio::test]
    async fn identical_bytes_dedupe_to_one_artifact() {
        let (service, blobs, db) = service().await;
        let first = service
            .put(&project(), png(), Some("a.png".into()))
            .await
            .unwrap();
        let second = service
            .put(&project(), png(), Some("a.png".into()))
            .await
            .unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(blobs.len(), 1);

        let count: i64 =
            sqlx::query_scalar(&db.q("SELECT COUNT(*) FROM artifacts WHERE project_id = ?"))
                .bind(project().as_str())
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(count, 1, "a second put must not add a row");
    }

    /// The claim never wins: these bytes are a PDF whatever the file is called.
    #[tokio::test]
    async fn a_pdf_named_as_an_image_is_stored_as_a_document() {
        let (service, _, _db) = service().await;
        let r = service
            .put(
                &project(),
                b"%PDF-1.7\nnot a png".to_vec(),
                Some("photo.png".into()),
            )
            .await
            .unwrap();
        assert_eq!(r.media_type, "application/pdf");
        assert!(matches!(
            r.kind,
            horsie_models::agent::ArtifactKind::Document(_)
        ));
        let row = service.meta(&project(), &r.id).await.unwrap().unwrap();
        assert_eq!(row.shape, ArtifactShape::Document);
        assert_eq!(row.media_type, "application/pdf");
    }

    #[tokio::test]
    async fn an_unsupported_type_is_refused_and_nothing_is_stored() {
        let (service, blobs, _db) = service().await;
        let bytes = b"MZ\x90\x00".to_vec();
        let err = service
            .put(&project(), bytes.clone(), Some("setup.exe".into()))
            .await
            .unwrap_err();
        match &err {
            ArtifactError::UnsupportedType { name, .. } => assert!(name.contains("setup.exe")),
            other => panic!("expected UnsupportedType, got {other:?}"),
        }
        assert!(err.to_string().contains("setup.exe"), "{err}");

        assert_eq!(blobs.puts(), 0);
        assert_eq!(blobs.len(), 0);
        assert!(
            service
                .meta(&project(), &sha256_hex(&bytes))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn an_oversize_upload_is_refused() {
        let (service, blobs, _db) = service().await;
        let mut bytes = png();
        bytes.resize(MAX_ARTIFACT_BYTES + 1, 0);
        match service.put(&project(), bytes, None).await.unwrap_err() {
            ArtifactError::TooLarge { size, limit, .. } => {
                assert_eq!(size, MAX_ARTIFACT_BYTES + 1);
                assert_eq!(limit, MAX_ARTIFACT_BYTES);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
        assert_eq!(blobs.puts(), 0);
    }

    /// The cache is the point: the second read must not reach the backend.
    #[tokio::test]
    async fn a_second_get_is_served_from_the_cache() {
        let (writer, blobs, _db) = service().await;
        let stored = writer.put(&project(), png(), None).await.unwrap();

        // A fresh service over the *same* blobs, so the first read is a genuine
        // miss rather than the cache `put` already populated.
        let reader = ArtifactService::new(
            testing::db().await,
            Arc::clone(&blobs) as Arc<dyn ArtifactBlobs>,
            Arc::new(ArtifactCache::default()),
        );
        assert_eq!(reader.get(&project(), &stored.id).await.unwrap(), png());
        assert_eq!(blobs.gets(), 1);
        assert_eq!(reader.get(&project(), &stored.id).await.unwrap(), png());
        assert_eq!(blobs.gets(), 1, "the second read came from the cache");
    }

    #[tokio::test]
    async fn getting_an_unknown_id_is_not_found() {
        let (service, _, _db) = service().await;
        match service.get(&project(), "deadbeef").await.unwrap_err() {
            ArtifactError::NotFound { id } => assert_eq!(id, "deadbeef"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    /// Another project's id is unreachable even though the bytes are identical.
    #[tokio::test]
    async fn an_artifact_is_not_readable_from_another_project() {
        let (service, _, _db) = service().await;
        let stored = service.put(&project(), png(), None).await.unwrap();
        assert!(matches!(
            service.get(&ProjectId::new("p2"), &stored.id).await,
            Err(ArtifactError::NotFound { .. })
        ));
        assert!(
            service
                .exists(&ProjectId::new("p2"), std::slice::from_ref(&stored.id))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            service
                .exists(&project(), std::slice::from_ref(&stored.id))
                .await
                .unwrap(),
            vec![stored.id]
        );
    }

    #[tokio::test]
    async fn resolve_fetches_every_id_once() {
        let (service, blobs, _db) = service().await;
        let a = service.put(&project(), png(), None).await.unwrap();
        let b = service
            .put(&project(), b"%PDF-1.7\n".to_vec(), None)
            .await
            .unwrap();

        let ids = vec![a.id.clone(), b.id.clone(), a.id.clone()];
        let got = service.resolve(&project(), &ids).await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got.get(&a.id), Some(&png()));
        assert_eq!(got.get(&b.id), Some(&b"%PDF-1.7\n".to_vec()));
        assert_eq!(blobs.gets(), 0, "everything was already cached by put");
    }

    #[tokio::test]
    async fn resolve_fails_loudly_on_a_missing_id() {
        let (service, _, _db) = service().await;
        let a = service.put(&project(), png(), None).await.unwrap();
        assert!(matches!(
            service.resolve(&project(), &[a.id, "nope".into()]).await,
            Err(ArtifactError::NotFound { .. })
        ));
    }

    /// The use table earning its keep, end to end: the shared artifact's bytes
    /// survive, the exclusive one's do not.
    #[tokio::test]
    async fn releasing_a_session_frees_only_what_nothing_else_needs() {
        let (service, blobs, _db) = service().await;
        let p = project();
        let shared = service.put(&p, png(), None).await.unwrap();
        let mine = service
            .put(&p, b"%PDF-1.7\nmine".to_vec(), None)
            .await
            .unwrap();
        service.mark_used(&p, &shared.id, "s1").await.unwrap();
        service.mark_used(&p, &shared.id, "s2").await.unwrap();
        service.mark_used(&p, &mine.id, "s1").await.unwrap();

        let deleted = service.release_session(&p, "s1").await.unwrap();
        assert_eq!(deleted, vec![mine.id.clone()]);
        assert_eq!(blobs.len(), 1, "only the shared artifact's bytes remain");
        assert!(matches!(
            service.get(&p, &mine.id).await,
            Err(ArtifactError::NotFound { .. })
        ));
        assert_eq!(service.get(&p, &shared.id).await.unwrap(), png());
    }

    #[test]
    fn a_rejection_names_what_it_saw() {
        assert_eq!(display_name(Some("a.png")), "'a.png'");
        assert_eq!(display_name(None), "this upload");
        assert_eq!(display_name(Some("")), "this upload");
        assert_eq!(leading_bytes(b"MZ\x90\x00"), "\"MZ\\x90\\x00\"");
        assert_eq!(leading_bytes(b""), "nothing at all");
        // Bounded: a megabyte of rubbish does not become a megabyte of message.
        assert!(leading_bytes(&[b'a'; 4096]).len() < 32);
    }
}
