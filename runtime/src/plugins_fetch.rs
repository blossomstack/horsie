//! Fetch the session's selected plugin bundles at startup and unpack them into
//! a plugins dir the existing scanner reads. The server injects a manifest of
//! `{name, hash, url}` refs plus a bearer token via env; the runtime GETs each
//! zip (over its own outbound connection — loopback for local, `advertise_host`
//! for velos), verifies the content hash, and materializes the tree.
//!
//! Fully best-effort: any failure is logged and skipped, so a session never
//! fails to start because a bundle was unavailable — it just runs without it.

use horsie_models::{ENV_PLUGIN_MANIFEST, ENV_PLUGINS_CACHE, ENV_PLUGINS_DIR, ENV_PLUGINS_TOKEN};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
struct ArtifactRef {
    name: String,
    hash: String,
    url: String,
}

/// Read the plugin manifest from the environment and materialize the bundles.
/// Returns the plugins dir when ≥1 bundle landed (to use as `plugins_dir`), or
/// `None` when there is nothing to provision or nothing succeeded.
pub async fn provision_plugins() -> Option<PathBuf> {
    let manifest = std::env::var(ENV_PLUGIN_MANIFEST).ok()?;
    let dir = PathBuf::from(std::env::var(ENV_PLUGINS_DIR).ok()?);
    let token = std::env::var(ENV_PLUGINS_TOKEN).ok();
    let cache = std::env::var(ENV_PLUGINS_CACHE).ok().map(PathBuf::from);
    provision_into(&manifest, &dir, token.as_deref(), cache.as_deref()).await
}

/// Env-free core (so tests need not touch process env): parse the manifest,
/// fetch/verify/unpack each bundle into `dir`, using `cache` as a content-hash
/// unpack cache when provided.
async fn provision_into(
    manifest: &str,
    dir: &Path,
    token: Option<&str>,
    cache: Option<&Path>,
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
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("plugins: cannot create plugins dir {}: {e}", dir.display());
        return None;
    }
    let client = match reqwest::Client::builder().build() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("plugins: http client init failed: {e}");
            return None;
        }
    };
    let mut any = false;
    for r in &refs {
        match materialize(&client, r, dir, token, cache).await {
            Ok(()) => any = true,
            Err(e) => eprintln!("plugins: skipping bundle '{}': {e}", r.name),
        }
    }
    any.then(|| dir.to_path_buf())
}

async fn materialize(
    client: &reqwest::Client,
    r: &ArtifactRef,
    dir: &Path,
    token: Option<&str>,
    cache: Option<&Path>,
) -> Result<(), String> {
    let dest = dir.join(&r.name);
    // Cache hit: link from the shared content-hash cache (local vendor).
    if let Some(cache) = cache {
        let cached = cache.join(&r.hash);
        if cached.is_dir() {
            copy_dir(&cached, &dest)?;
            return Ok(());
        }
    }
    let mut req = client.get(&r.url);
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
    match cache {
        Some(cache) => {
            let cached = cache.join(&r.hash);
            unpack_zip(&bytes, &cached)?;
            copy_dir(&cached, &dest)?;
        }
        None => unpack_zip(&bytes, &dest)?,
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
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

fn copy_dir(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(src).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to).map_err(|e| e.to_string())?;
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

    /// Serve exactly one HTTP/1.1 GET with the given body, then close. A plain
    /// std-thread stub so the test needs no extra tokio io features.
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
        format!("http://{addr}/artifact.zip")
    }

    #[tokio::test]
    async fn provision_fetches_verifies_and_unpacks() {
        let bytes = make_zip();
        let hash = sha256_hex(&bytes);
        let url = serve_once(bytes);
        let manifest =
            serde_json::json!([{ "name": "demo", "hash": hash, "url": url }]).to_string();

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plugins");
        let out = provision_into(&manifest, &dir, Some("tok"), None).await;
        assert_eq!(out.as_deref(), Some(dir.as_path()));
        assert!(dir.join("demo/skills/a/SKILL.md").is_file());
    }

    #[tokio::test]
    async fn provision_rejects_hash_mismatch() {
        let url = serve_once(make_zip());
        // Manifest claims a wrong hash → bundle skipped, nothing materialized.
        let manifest =
            serde_json::json!([{ "name": "demo", "hash": "deadbeef", "url": url }]).to_string();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("plugins");
        let out = provision_into(&manifest, &dir, None, None).await;
        assert!(out.is_none());
        assert!(!dir.join("demo").exists());
    }

    #[tokio::test]
    async fn empty_manifest_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let out = provision_into("[]", &tmp.path().join("p"), None, None).await;
        assert!(out.is_none());
    }
}
