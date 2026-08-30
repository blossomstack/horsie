//! Where an agent gets artifact bytes from.
//!
//! The adapter between [`ArtifactService`], which knows about projects and
//! rows, and [`horsie_agentcore::ArtifactSource`], which an agent calls just
//! before each provider call.

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use base64::Engine as _;
use horsie_agentcore::ArtifactSource;

use crate::{artifacts::ArtifactService, projects::ProjectId};

/// One project's artifacts, base64-encoded on the way out.
///
/// Encoding happens here rather than in a provider because every wire format
/// wants base64 — Anthropic's `source.data`, OpenAI's data URL — so doing it
/// once at this boundary saves both a repeat and a dependency in three
/// adapters.
pub struct ProjectArtifacts {
    service: Arc<ArtifactService>,
    project: ProjectId,
    /// Whether this session's model can be shown artifacts at all.
    ///
    /// **This is where vision gating lives, and the only place it lives.** A
    /// text-only model gets a source that resolves nothing, and every provider
    /// then renders `ArtifactRef::omitted_text` — so no provider holds a
    /// capability flag and no provider can forget to check one.
    shows_artifacts: bool,
}

impl ProjectArtifacts {
    #[must_use]
    pub fn new(service: Arc<ArtifactService>, project: ProjectId, shows_artifacts: bool) -> Self {
        Self {
            service,
            project,
            shows_artifacts,
        }
    }
}

#[async_trait]
impl ArtifactSource for ProjectArtifacts {
    async fn resolve(&self, ids: &[String]) -> HashMap<String, String> {
        if !self.shows_artifacts || ids.is_empty() {
            return HashMap::new();
        }
        match self.service.resolve(&self.project, ids).await {
            Ok(found) => found
                .into_iter()
                .map(|(id, bytes)| (id, base64::engine::general_purpose::STANDARD.encode(&bytes)))
                .collect(),
            // Deliberately not an error. An artifact whose bytes will not load
            // is omitted, and the model is told it was withheld; failing the
            // turn instead would throw away the message the person actually
            // sent over a storage hiccup.
            Err(error) => {
                tracing::warn!(%error, count = ids.len(), "could not resolve artifacts for a turn");
                HashMap::new()
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    async fn service_with(bytes: &[u8]) -> (Arc<ArtifactService>, ProjectId, String) {
        let db = crate::db::testing::db().await;
        let service = Arc::new(ArtifactService::in_database(db));
        let project = ProjectId::new("p-test");
        let stored = service
            .put(&project, bytes.to_vec(), Some("shot.png".into()))
            .await
            .expect("stored");
        (service, project, stored.id)
    }

    fn png() -> Vec<u8> {
        vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00,
        ]
    }

    #[tokio::test]
    async fn resolves_bytes_as_base64() {
        let (service, project, id) = service_with(&png()).await;
        let source = ProjectArtifacts::new(service, project, true);

        let out = source.resolve(std::slice::from_ref(&id)).await;

        let expected = base64::engine::general_purpose::STANDARD.encode(png());
        assert_eq!(out.get(&id), Some(&expected));
    }

    /// The gate. A text-only model must be shown nothing, and this is the one
    /// place that decision is made.
    #[tokio::test]
    async fn a_model_that_cannot_see_resolves_nothing() {
        let (service, project, id) = service_with(&png()).await;
        let source = ProjectArtifacts::new(service, project, false);

        assert!(
            source.resolve(&[id]).await.is_empty(),
            "a gated source must hand a provider nothing at all"
        );
    }

    /// An id this project does not have must not resolve, and must not take
    /// the turn down either.
    #[tokio::test]
    async fn an_unknown_id_resolves_to_nothing_without_failing() {
        let (service, project, _) = service_with(&png()).await;
        let source = ProjectArtifacts::new(service, project, true);

        assert!(source.resolve(&["nope".to_string()]).await.is_empty());
    }
}
