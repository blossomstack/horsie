//! Upload and fetch the images and documents a message carries.
//!
//! Two routes and no more: bytes in, bytes out. Everything about *what* the
//! bytes are — the type, the size, the dimensions, whether they are acceptable
//! at all — is decided by [`crate::artifacts::ArtifactService`] from the bytes
//! themselves, so nothing here inspects a payload.
//!
//! Bytes go up as a raw body rather than as multipart. One file per request
//! means multipart would buy only a filename, and that fits in a query
//! parameter; a raw body also keeps the size limit a single number rather than
//! a per-part accounting problem.

use axum::{
    Json,
    body::Bytes,
    extract::Query,
    http::{StatusCode, header},
    response::IntoResponse,
};
use horsie_models::agent::ArtifactRef;

use crate::{
    artifacts::{ArtifactError, MAX_ARTIFACT_BYTES},
    http::{Scope, Scoped, error::Api},
};

/// What the client called the file, when it had a name.
///
/// A query parameter rather than a header: a paste has no filename at all, and
/// a browser sending one has to encode it somehow — `?filename=` is the one
/// place both are already handled by every HTTP client.
#[derive(serde::Deserialize)]
pub struct UploadParams {
    #[serde(default)]
    pub filename: Option<String>,
}

/// `POST /api/p/{project}/artifacts?filename=` — store one file.
///
/// The request's `Content-Type` is deliberately ignored. It is set by the
/// client, is routinely wrong or absent for a paste, and would be a way to have
/// a PDF stored as an image if it were believed. The service sniffs instead.
pub async fn upload(
    Scope(state): Scope,
    Query(params): Query<UploadParams>,
    bytes: Bytes,
) -> Result<impl IntoResponse, Api> {
    let artifact = state
        .artifacts
        .put(&state.project, bytes.to_vec(), params.filename)
        .await
        .map_err(to_api)?;
    Ok((StatusCode::OK, Json(artifact)))
}

/// `GET /api/p/{project}/artifacts/{id}` — the bytes back.
///
/// Cached for a year and marked `immutable`, which is safe precisely because
/// the id is the sha256 of the body: this URL cannot ever answer with something
/// else. `private` because a project's artifacts are not public, so only the
/// browser that fetched them may keep a copy.
pub async fn fetch(
    Scope(state): Scope,
    Scoped(id): Scoped<String>,
) -> Result<impl IntoResponse, Api> {
    let meta = state
        .artifacts
        .meta(&state.project, &id)
        .await
        .map_err(to_api)?
        .ok_or_else(|| Api::not_found("no such artifact"))?;
    let bytes = state
        .artifacts
        .get(&state.project, &id)
        .await
        .map_err(to_api)?;

    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, meta.media_type.clone()),
            (
                header::CACHE_CONTROL,
                "private, max-age=31536000, immutable".to_string(),
            ),
            // Never inline-render an upload as a document in the user's own
            // origin: an `<img>` tag is unaffected by this, and a hostile SVG
            // or HTML that slipped past the sniffer cannot become a page.
            (
                header::CONTENT_DISPOSITION,
                content_disposition(meta.filename.as_deref()),
            ),
            (
                header::HeaderName::from_static("x-content-type-options"),
                "nosniff".to_string(),
            ),
        ],
        bytes,
    ))
}

/// `inline` so an image renders in place, with the original filename attached
/// for a download. The name is quoted and stripped of anything that would break
/// out of the header.
fn content_disposition(filename: Option<&str>) -> String {
    let Some(name) = filename else {
        return "inline".to_string();
    };
    let safe: String = name
        .chars()
        .filter(|c| !matches!(c, '"' | '\\' | '\r' | '\n'))
        .collect();
    if safe.is_empty() {
        return "inline".to_string();
    }
    format!("inline; filename=\"{safe}\"")
}

fn to_api(error: ArtifactError) -> Api {
    match error {
        ArtifactError::UnsupportedType { .. } => Api::conflict(
            "unsupported-artifact",
            format!("{error}. Accepted: PNG, JPEG, GIF, WebP and PDF."),
        ),
        ArtifactError::TooLarge { .. } => Api::conflict(
            "artifact-too-large",
            format!(
                "{error}. The limit is {} MB.",
                MAX_ARTIFACT_BYTES / (1024 * 1024)
            ),
        ),
        ArtifactError::NotFound { .. } => Api::not_found("no such artifact"),
        ArtifactError::Storage(_) => Api::internal(format!("storing the artifact: {error}")),
    }
}

/// Every artifact id in `refs` that this project actually holds, as an error
/// when any is missing.
///
/// Used by `send_message`: a client names artifacts by id, and an id it did not
/// upload — or uploaded to a different project — must not be attachable. The
/// check is a membership test against this project's rows, so a correct
/// sha256 from another project fails it.
pub async fn verify_owned(
    services: &crate::projects::ProjectServices,
    refs: &[ArtifactRef],
) -> Result<(), Api> {
    if refs.is_empty() {
        return Ok(());
    }
    let ids: Vec<String> = refs.iter().map(|r| r.id.clone()).collect();
    let present = services
        .artifacts
        .exists(&services.project, &ids)
        .await
        .map_err(to_api)?;
    if let Some(missing) = ids.iter().find(|id| !present.contains(id)) {
        return Err(Api::conflict(
            "unknown-artifact",
            format!("no artifact {missing} in this project"),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_filename_is_quoted_and_sanitised() {
        assert_eq!(content_disposition(None), "inline");
        assert_eq!(
            content_disposition(Some("shot.png")),
            "inline; filename=\"shot.png\""
        );
    }

    /// A filename is client-supplied, so it must not be able to inject a second
    /// header or terminate the quoted string.
    #[test]
    fn a_filename_cannot_break_out_of_the_header() {
        let attack = "a\";\r\nSet-Cookie: x=1\r\n";
        let out = content_disposition(Some(attack));
        assert!(!out.contains('\r'), "{out}");
        assert!(!out.contains('\n'), "{out}");
        assert_eq!(
            out.matches('"').count(),
            2,
            "exactly the two we added: {out}"
        );
    }

    #[test]
    fn a_filename_of_only_illegal_characters_falls_back_to_inline() {
        assert_eq!(content_disposition(Some("\"\"")), "inline");
    }
}
