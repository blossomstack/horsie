//! Artifacts: the images and documents a conversation carries.
//!
//! A message never holds bytes — it holds an `ArtifactRef`, and the bytes live
//! here. [`ArtifactService`] is the whole public surface; the four modules
//! under it are one concern each.
//!
//! - [`media`] — what a pile of bytes *is*, decided from the bytes alone.
//! - [`blobs`] — where the bytes are kept, behind a three-method trait so an
//!   object store can replace the database without anything else noticing.
//! - [`store`] — the metadata row and the per-session reference table that
//!   decides when an artifact may be deleted.
//! - [`cache`] — a byte-bounded LRU. An id is the hash of its bytes, so a
//!   cached entry can never be stale.

pub mod blobs;
pub mod cache;
pub mod media;
pub mod service;
pub mod source;
pub mod store;

pub use blobs::{ArtifactBlobs, BlobError, BlobKey, DbBlobs};
pub use cache::{ArtifactCache, DEFAULT_BUDGET_BYTES};
pub use service::{ArtifactError, ArtifactService, MAX_ARTIFACT_BYTES};
pub use store::{ArtifactRow, ArtifactShape, ArtifactStore};
