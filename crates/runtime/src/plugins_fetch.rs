//! Fetch the session's selected plugin bundles at startup and unpack them into
//! a plugins dir the existing scanner reads. The server injects a manifest of
//! `{name, hash}` refs plus a bearer token via env, and the vendor process adds
//! the base URL its runtimes can reach the server at plus the directory to
//! unpack into; the runtime GETs each zip over its own outbound connection,
//! verifies the content hash, and materializes the tree.
//!
//! Materialization happens once per runtime, not once per process. The
//! directory records the manifest it was built from, and a start that finds a
//! matching record does nothing — which is what makes a hibernated runtime's
//! respawn free, and keeps it from presenting an artifact token that has since
//! expired.
//!
//! Fully best-effort: any failure is logged and skipped, so a session never
//! fails to start because a bundle was unavailable — it just runs without it.

use horsie_models::{ENV_PLUGIN_MANIFEST, ENV_PLUGINS_BASE, ENV_PLUGINS_DIR, ENV_PLUGINS_TOKEN};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Records the manifest a plugins dir was last materialized from. A plain file,
/// so `plugin_dirs` — which keeps only entries that are directories — never
/// mistakes it for a plugin.
const MARKER: &str = ".manifest.json";

#[derive(Deserialize)]
struct ArtifactRef {
    name: String,
    hash: String,
}

impl ArtifactRef {
    /// Where this bundle is fetched from. Built here rather than sent by the
    /// server, which has no way to know the address that reaches it from
    /// wherever this runtime happens to be running.
    fn url(&self, base: &str) -> String {
        format!(
            "{}/api/plugin-artifacts/{}.zip",
            base.trim_end_matches('/'),
            self.hash
        )
    }
}

/// Read the plugin manifest from the environment and materialize the bundles.
/// Returns the plugins dir whenever the manifest named anything, whether or not
/// every bundle landed: there is no host library left to fall back to, so the
/// distinction no longer decides anything.
pub async fn provision_plugins() -> Option<PathBuf> {
    let manifest = std::env::var(ENV_PLUGIN_MANIFEST).ok()?;
    let dir = PathBuf::from(std::env::var(ENV_PLUGINS_DIR).ok()?);
    let base = std::env::var(ENV_PLUGINS_BASE).ok()?;
    let token = std::env::var(ENV_PLUGINS_TOKEN).ok();
    provision_into(&manifest, &base, &dir, token.as_deref()).await
}

/// Env-free core (so tests need not touch process env): parse the manifest,
/// then fetch/verify/unpack each bundle into `dir`.
async fn provision_into(
    manifest: &str,
    base: &str,
    dir: &Path,
    token: Option<&str>,
) -> Option<PathBuf> {
    let refs: Vec<ArtifactRef> = match serde_json::from_str(manifest) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("plugins: ignoring malformed manifest: {e}");
            return None;
        }
    };
    if refs.is_empty() {
        return None;
    }
    if already_materialized(dir, manifest) {
        return Some(dir.to_path_buf());
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("plugins: cannot create plugins dir {}: {e}", dir.display());
        return None;
    }
    // Whatever a previous manifest left behind is not what this session
    // selected, and the scanner reads the whole directory.
    clear_dir(dir);
    let client = match reqwest::Client::builder().build() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("plugins: http client init failed: {e}");
            return None;
        }
    };
    let mut complete = true;
    for r in &refs {
        if let Err(e) = materialize(&client, r, base, dir, token).await {
            eprintln!("plugins: skipping bundle '{}': {e}", r.name);
            complete = false;
        }
    }
    // Only a complete run earns the marker. A partial one must be retried on
    // the next start rather than frozen in place for the session's whole life.
    if !complete {
        return Some(dir.to_path_buf());
    }
    if let Err(e) = std::fs::write(dir.join(MARKER), manifest) {
        eprintln!("plugins: cannot record the manifest: {e}");
    }
    Some(dir.to_path_buf())
}

/// Empty `dir` without removing `dir` itself.
///
/// The distinction is load-bearing under a sandbox. This directory is the unit
/// the vendor granted, and it is granted by path: removing it and making a new
/// one needs a write on the *parent*, which this runtime has no grant for. So a
/// `remove_dir_all` here fails the whole provision — silently, since fetching is
/// best-effort — and the session comes up with no skills.
fn clear_dir(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let _ = if entry.file_type().is_ok_and(|t| t.is_dir()) {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
    }
}

/// Whether `dir` was already built from exactly this manifest.
fn already_materialized(dir: &Path, manifest: &str) -> bool {
    std::fs::read_to_string(dir.join(MARKER)).is_ok_and(|recorded| recorded == manifest)
}

async fn materialize(
    client: &reqwest::Client,
    r: &ArtifactRef,
    base: &str,
    dir: &Path,
    token: Option<&str>,
) -> Result<(), String> {
    let mut req = client.get(r.url(base));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    let got = sha256_hex(&bytes);
    if got != r.hash {
        return Err(format!("hash mismatch (want {}, got {got})", r.hash));
    }
    unpack_zip(&bytes, &dir.join(&r.name))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Unpack zip `bytes` into `into`, ignoring any entry with an unsafe path.
fn unpack_zip(bytes: &[u8], into: &Path) -> Result<(), String> {
    std::fs::create_dir_all(into).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| e.to_string())?;
    for i in 0..zip.len() {
        let mut file = zip.by_index(i).map_err(|e| e.to_string())?;
        let Some(rel) = file.enclosed_name() else {
            continue; // reject path traversal
        };
        let out = into.join(rel);
        if file.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
        } else {
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let mut w = std::fs::File::create(&out).map_err(|e| e.to_string())?;
            std::io::copy(&mut file, &mut w).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};

    /// Build a small deterministic zip with one file.
    fn make_zip() -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("skills/a/SKILL.md", opts).unwrap();
        zip.write_all(b"---\nname: a\n---\nbody").unwrap();
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn unpack_writes_tree_and_sha_is_stable() {
        let bytes = make_zip();
        assert_eq!(sha256_hex(b"abc"), sha256_hex(b"abc"));
        assert_ne!(sha256_hex(b"abc"), sha256_hex(b"abd"));
        let tmp = tempfile::tempdir().unwrap();
        unpack_zip(&bytes, tmp.path()).unwrap();
        assert!(tmp.path().join("skills/a/SKILL.md").is_file());
    }

    /// Serve exactly one HTTP/1.1 GET with the given body, then close, and
    /// return the *base* URL. The stub ignores the request path, so it stands
    /// in for the artifact route the runtime now builds from base + hash. A
    /// plain std-thread stub so the test needs no extra tokio io features.
    fn serve_once(body: Vec<u8>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf); // consume the request line/headers
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/zip\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(header.as_bytes());
                let _ = sock.write_all(&body);
                let _ = sock.flush();
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn provision_fetches_verifies_unpacks_and_marks() {
        let bytes = make_zip();
        let hash = sha256_hex(&bytes);
        let base = serve_once(bytes);
        let manifest = serde_json::json!([{ "name": "demo", "hash": hash }]).to_string();

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plugins");
        let out = provision_into(&manifest, &base, &dir, Some("tok")).await;
        assert_eq!(out.as_deref(), Some(dir.as_path()));
        assert!(dir.join("demo/skills/a/SKILL.md").is_file());
        assert_eq!(
            std::fs::read_to_string(dir.join(MARKER)).unwrap(),
            manifest,
            "a complete materialization records what it was built from"
        );
    }

    /// The whole point of the marker: a respawned runtime must not re-fetch.
    /// The base points at a port nothing listens on, so any fetch attempt would
    /// fail — and the sentinel file proves the directory was never cleared.
    #[tokio::test]
    async fn a_matching_marker_skips_the_fetch_entirely() {
        let manifest = serde_json::json!([{ "name": "demo", "hash": "abc" }]).to_string();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plugins");
        std::fs::create_dir_all(dir.join("demo")).unwrap();
        std::fs::write(dir.join("demo/sentinel"), b"kept").unwrap();
        std::fs::write(dir.join(MARKER), &manifest).unwrap();

        let out = provision_into(&manifest, "http://127.0.0.1:1", &dir, None).await;
        assert_eq!(out.as_deref(), Some(dir.as_path()));
        assert!(dir.join("demo/sentinel").is_file());
    }

    /// A different selection is not a merge: what the last manifest left has to
    /// go, or the session scans skills it did not select.
    #[tokio::test]
    async fn a_changed_manifest_clears_the_directory() {
        let bytes = make_zip();
        let hash = sha256_hex(&bytes);
        let base = serve_once(bytes);
        let manifest = serde_json::json!([{ "name": "demo", "hash": hash }]).to_string();

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plugins");
        std::fs::create_dir_all(dir.join("stale")).unwrap();
        std::fs::write(dir.join("stale/SKILL.md"), b"old").unwrap();
        std::fs::write(dir.join(MARKER), b"[{\"name\":\"stale\",\"hash\":\"old\"}]").unwrap();

        let before = std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&dir).unwrap());
        provision_into(&manifest, &base, &dir, None).await;
        assert!(
            !dir.join("stale").exists(),
            "the previous selection is gone"
        );
        assert!(dir.join("demo/skills/a/SKILL.md").is_file());
        // The directory itself must survive: it is what the sandbox grants, by
        // path, and re-creating it needs a write on the parent the runtime has
        // no grant for. Removing it makes every sandboxed provision fail — and
        // fail silently, because fetching is best-effort.
        assert_eq!(
            std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&dir).unwrap()),
            before,
            "the granted directory was replaced rather than emptied"
        );
    }

    /// No marker after a partial run, so the next start retries. Today a bundle
    /// that fails once is lost for the life of the session.
    #[tokio::test]
    async fn a_partial_materialization_writes_no_marker() {
        let bytes = make_zip();
        let hash = sha256_hex(&bytes);
        // The stub serves exactly one request, so the second ref cannot be
        // fetched at all.
        let base = serve_once(bytes);
        let manifest = serde_json::json!([
            { "name": "first", "hash": hash },
            { "name": "second", "hash": "deadbeef" },
        ])
        .to_string();

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plugins");
        let out = provision_into(&manifest, &base, &dir, None).await;

        assert_eq!(out.as_deref(), Some(dir.as_path()));
        assert!(dir.join("first/skills/a/SKILL.md").is_file());
        assert!(!dir.join("second").exists());
        assert!(!dir.join(MARKER).exists(), "a partial run must be retried");
    }

    #[tokio::test]
    async fn provision_rejects_hash_mismatch() {
        let base = serve_once(make_zip());
        // Manifest claims a wrong hash → bundle skipped, nothing materialized.
        let manifest = serde_json::json!([{ "name": "demo", "hash": "deadbeef" }]).to_string();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plugins");
        provision_into(&manifest, &base, &dir, None).await;
        assert!(!dir.join("demo").exists());
        assert!(!dir.join(MARKER).exists());
    }

    #[test]
    fn the_fetch_url_is_built_from_the_base_and_hash() {
        let r = ArtifactRef {
            name: "demo".into(),
            hash: "abc123".into(),
        };
        assert_eq!(
            r.url("http://server:3789/"),
            "http://server:3789/api/plugin-artifacts/abc123.zip",
            "a trailing slash on the agent-supplied base must not double up"
        );
    }

    #[tokio::test]
    async fn empty_manifest_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let out = provision_into("[]", "http://unused", &tmp.path().join("p"), None).await;
        assert!(out.is_none());
    }
}
