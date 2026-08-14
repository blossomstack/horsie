//! One content-addressed bundle store, and a tree of links per agent.
//!
//! ```text
//! <plugins_dir>/                        ← the one path the vendor granted
//!   store/<hash>/…                      ← the real files, written once
//!   agents/<agent_id>/
//!     <bundle_name> -> ../../store/<hash>
//!     .manifest.json                    ← the set this tree was last built from
//! ```
//!
//! **Why a store rather than a directory per agent.** An agent is the unit that
//! has a plugin set — a workflow step runs under its own preset — but agents in
//! one session overwhelmingly select the *same* bundles. Unpacking per agent
//! would download and store N copies of one zip. Content-addressing makes the
//! second agent cost a symlink.
//!
//! **Why both halves live under `plugins_dir`.** It is the unit the vendor
//! granted, and it is granted *by path*: the runtime has no write grant on the
//! parent, so it cannot create a sibling directory. A confined runtime can read
//! through the links because the resolved target is inside the same grant —
//! verified against the kernel in `tests/sandbox_symlinks.rs` rather than
//! assumed.
//!
//! **Why the store is never mutated.** A hash names its contents, so an entry
//! that exists is finished. New bytes land in a temp directory and are renamed
//! into place only once their hash verifies, which is what makes a cancelled or
//! crashed fetch leave nothing behind. A half-unpacked directory already named
//! after its hash would be treated as a cache hit for the life of the runtime.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Records the bundle set an agent's tree was last built from, so an unchanged
/// set costs no I/O at all. A plain file, so the scanner — which keeps directory
/// entries — never mistakes it for a bundle.
const MARKER: &str = ".manifest.json";

/// One bundle to install: a name to link it under, and the hash that is its
/// identity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BundleRef {
    pub name: String,
    pub hash: String,
}

impl From<&horsie_models::runtime::BundleRef> for BundleRef {
    fn from(w: &horsie_models::runtime::BundleRef) -> Self {
        Self {
            name: w.name.clone(),
            hash: w.hash.clone(),
        }
    }
}

/// Where a bundle's bytes come from. A trait so the store can be tested without
/// a server: the fetching is the only part that needs one, and it is the part
/// least worth exercising over a socket in a unit test.
#[async_trait::async_trait]
pub trait BundleSource: Send + Sync {
    /// The zip for `hash`, or why it could not be had.
    async fn fetch(&self, bundle: &BundleRef) -> Result<Vec<u8>, String>;
}

/// Fetches bundles from the server the runtime dialled, against the only
/// credential it holds.
///
/// The dial token, not a bundle-scoped one. A short-lived credential minted
/// beside it expired within the hour with nothing able to renew it, so a runtime
/// that outlived it could never fetch again — and a runtime is expected to
/// outlive an hour.
pub struct HttpBundles {
    client: reqwest::Client,
    base: String,
    token: Option<String>,
}

impl HttpBundles {
    #[must_use]
    pub fn new(base: String, token: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base,
            token,
        }
    }
}

#[async_trait::async_trait]
impl BundleSource for HttpBundles {
    async fn fetch(&self, bundle: &BundleRef) -> Result<Vec<u8>, String> {
        let url = format!(
            "{}/api/plugin-artifacts/{}.zip",
            self.base.trim_end_matches('/'),
            bundle.hash
        );
        let mut req = self.client.get(&url);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await.map_err(|e| chain(&e))?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| chain(&e))
    }
}

/// An error plus every `source()` behind it, joined.
///
/// `reqwest::Error`'s own `Display` is `error sending request for url (…)` and
/// says nothing about *why*. Reporting only the top line cannot distinguish a
/// DNS failure from a refused connection from an untrusted certificate, which
/// are three completely different bugs.
fn chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut cause = e.source();
    while let Some(c) = cause {
        let text = c.to_string();
        if !out.ends_with(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        cause = c.source();
    }
    out
}

/// The store rooted at one granted plugins directory.
pub struct PluginStore {
    root: PathBuf,
}

impl PluginStore {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Where this agent's tree lives. Computed rather than looked up, so a
    /// caller can name it before it exists.
    #[must_use]
    pub fn agent_dir(&self, agent_id: &str) -> PathBuf {
        self.root.join("agents").join(sanitize(agent_id))
    }

    fn store_dir(&self) -> PathBuf {
        self.root.join("store")
    }

    /// Install `bundles` for `agent_id` and return the agent's tree root.
    ///
    /// Fail-whole: a bundle that cannot be installed fails the call and leaves
    /// the agent's previous tree untouched. A partly built tree is worse than
    /// none, because an agent would run with a silently reduced skill set and
    /// nothing would say so.
    pub async fn provision_agent(
        &self,
        agent_id: &str,
        bundles: &[BundleRef],
        source: &dyn BundleSource,
    ) -> Result<PathBuf, String> {
        let agent_dir = self.agent_dir(agent_id);
        let manifest = serde_json::to_string(bundles).map_err(|e| e.to_string())?;
        if already_built(&agent_dir, &manifest) {
            return Ok(agent_dir);
        }

        // Every bundle into the store first. Nothing about the agent's tree
        // changes until all of them are there, so a failure half way leaves the
        // agent exactly as it was.
        for bundle in bundles {
            self.ensure_stored(bundle, source).await?;
        }

        std::fs::create_dir_all(&agent_dir)
            .map_err(|e| format!("cannot create the agent's plugin dir: {e}"))?;
        // Whatever a previous set left is not what this agent selected, and the
        // scanner reads the whole directory.
        clear_dir(&agent_dir);
        for bundle in bundles {
            self.link(&agent_dir, bundle)?;
        }
        // Only a complete tree earns the marker, so a partial one is rebuilt on
        // the next call rather than frozen in place for the runtime's life.
        std::fs::write(agent_dir.join(MARKER), &manifest)
            .map_err(|e| format!("cannot record the agent's bundle set: {e}"))?;
        Ok(agent_dir)
    }

    /// Put `bundle` in the store if it is not already there.
    ///
    /// Fetch → verify → rename. The rename is what makes the store safe to read
    /// without locking: an entry under its hash is complete by construction,
    /// because nothing is ever named that until its bytes have been checked.
    async fn ensure_stored(
        &self,
        bundle: &BundleRef,
        source: &dyn BundleSource,
    ) -> Result<(), String> {
        let final_dir = self.store_dir().join(sanitize(&bundle.hash));
        if final_dir.is_dir() {
            return Ok(());
        }
        std::fs::create_dir_all(self.store_dir())
            .map_err(|e| format!("cannot create the bundle store: {e}"))?;

        let bytes = source
            .fetch(bundle)
            .await
            .map_err(|e| format!("bundle '{}': {e}", bundle.name))?;
        let got = sha256_hex(&bytes);
        if got != bundle.hash {
            return Err(format!(
                "bundle '{}': hash mismatch (want {}, got {got})",
                bundle.name, bundle.hash
            ));
        }

        // Beside the final location, so the rename is within one filesystem.
        let staging = self
            .store_dir()
            .join(format!(".staging-{}", sanitize(&bundle.hash)));
        let _ = std::fs::remove_dir_all(&staging);
        unpack_zip(&bytes, &staging).map_err(|e| format!("bundle '{}': {e}", bundle.name))?;

        match std::fs::rename(&staging, &final_dir) {
            Ok(()) => Ok(()),
            // Another provision for the same hash won the race and put an
            // identical tree there. Identical by construction — the hash is the
            // contents — so this is a success, not a conflict.
            Err(_) if final_dir.is_dir() => {
                let _ = std::fs::remove_dir_all(&staging);
                Ok(())
            }
            Err(e) => Err(format!("bundle '{}': cannot store: {e}", bundle.name)),
        }
    }

    /// Link one stored bundle into an agent's tree under its own name.
    fn link(&self, agent_dir: &Path, bundle: &BundleRef) -> Result<(), String> {
        let link = agent_dir.join(sanitize(&bundle.name));
        // Relative, so the tree survives the whole plugins directory being
        // mounted at a different path — which it is, between a container and the
        // laptop that built the image.
        let target = Path::new("../../store").join(sanitize(&bundle.hash));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, &link)
                .map_err(|e| format!("bundle '{}': cannot link: {e}", bundle.name))
        }
        #[cfg(not(unix))]
        {
            let _ = (link, target);
            Err("plugin bundles need symlink support".to_string())
        }
    }
}

/// Whether `agent_dir` was already built from exactly this set.
fn already_built(agent_dir: &Path, manifest: &str) -> bool {
    std::fs::read_to_string(agent_dir.join(MARKER)).is_ok_and(|recorded| recorded == manifest)
}

/// Empty `dir` without removing `dir` itself — see the module doc on why the
/// granted directory cannot be recreated.
fn clear_dir(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // `file_type` here does NOT follow links, which is what makes this
        // remove the link rather than the store entry behind it.
        let _ = match entry.file_type() {
            Ok(t) if t.is_dir() => std::fs::remove_dir_all(&path),
            _ => std::fs::remove_file(&path),
        };
    }
}

/// One safe path component. A bundle name and a hash both arrive over the wire,
/// so neither may be able to name anything outside the store.
///
/// Two guarantees, and the second is not implied by the first: no separator, and
/// no `..`. Mapping separators alone leaves `.._.._etc` — harmless, since without
/// a separator it is one component and cannot climb — but a path holding `..` is
/// the kind of thing a later reader assumes is traversal and "fixes" wrongly.
fn sanitize(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    while out.contains("..") {
        out = out.replace("..", "_");
    }
    out.trim_start_matches('.').to_string()
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
    use std::io::Write as _;
    use std::sync::{Arc, Mutex};

    /// A source that hands back prepared zips and counts what it was asked for —
    /// the count is how the dedupe claim is actually checked.
    #[derive(Default)]
    struct FakeSource {
        zips: Mutex<std::collections::HashMap<String, Vec<u8>>>,
        fetched: Arc<Mutex<Vec<String>>>,
        fail: Option<String>,
    }

    impl FakeSource {
        fn with(files: &[(&str, &str)]) -> (Self, String) {
            let bytes = zip_of(files);
            let hash = sha256_hex(&bytes);
            let s = Self::default();
            s.zips.lock().unwrap().insert(hash.clone(), bytes);
            (s, hash)
        }
    }

    #[async_trait::async_trait]
    impl BundleSource for FakeSource {
        async fn fetch(&self, bundle: &BundleRef) -> Result<Vec<u8>, String> {
            self.fetched.lock().unwrap().push(bundle.hash.clone());
            if let Some(e) = &self.fail {
                return Err(e.clone());
            }
            self.zips
                .lock()
                .unwrap()
                .get(&bundle.hash)
                .cloned()
                .ok_or_else(|| "no such bundle".to_string())
        }
    }

    fn zip_of(files: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            for (path, body) in files {
                w.start_file::<_, ()>(*path, zip::write::SimpleFileOptions::default())
                    .unwrap();
                w.write_all(body.as_bytes()).unwrap();
            }
            w.finish().unwrap();
        }
        buf
    }

    #[tokio::test]
    async fn an_agents_tree_links_the_bundles_it_selected() {
        let root = tempfile::tempdir().unwrap();
        let (source, hash) = FakeSource::with(&[("skills/a/SKILL.md", "body-a")]);
        let store = PluginStore::new(root.path().to_path_buf());

        let dir = store
            .provision_agent(
                "agent-1",
                &[BundleRef {
                    name: "pack".into(),
                    hash: hash.clone(),
                }],
                &source,
            )
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.join("pack/skills/a/SKILL.md")).unwrap(),
            "body-a",
            "the agent reads the bundle through its own link"
        );
        assert!(root.path().join("store").join(&hash).is_dir());
    }

    /// The reason for content-addressing. Two agents on one bundle must cost one
    /// download, or a session with five subagents pays five times for the same
    /// zip.
    #[tokio::test]
    async fn two_agents_on_one_bundle_fetch_it_once() {
        let root = tempfile::tempdir().unwrap();
        let (source, hash) = FakeSource::with(&[("skills/a/SKILL.md", "body-a")]);
        let fetched = source.fetched.clone();
        let store = PluginStore::new(root.path().to_path_buf());
        let refs = [BundleRef {
            name: "pack".into(),
            hash,
        }];

        for agent in ["agent-1", "agent-2"] {
            store.provision_agent(agent, &refs, &source).await.unwrap();
        }

        assert_eq!(fetched.lock().unwrap().len(), 1, "one fetch for two agents");
        for agent in ["agent-1", "agent-2"] {
            assert!(
                store
                    .agent_dir(agent)
                    .join("pack/skills/a/SKILL.md")
                    .is_file(),
                "{agent} must still see the bundle"
            );
        }
    }

    /// The server sends this on every agent load, so the unchanged case has to
    /// be free — not merely correct.
    #[tokio::test]
    async fn reprovisioning_an_unchanged_set_touches_nothing() {
        let root = tempfile::tempdir().unwrap();
        let (source, hash) = FakeSource::with(&[("skills/a/SKILL.md", "body-a")]);
        let fetched = source.fetched.clone();
        let store = PluginStore::new(root.path().to_path_buf());
        let refs = [BundleRef {
            name: "pack".into(),
            hash,
        }];

        store.provision_agent("a1", &refs, &source).await.unwrap();
        store.provision_agent("a1", &refs, &source).await.unwrap();

        assert_eq!(
            fetched.lock().unwrap().len(),
            1,
            "the second call refetched"
        );
    }

    /// A changed set rebuilds the tree rather than adding to it — otherwise a
    /// removed bundle would linger and the agent would keep a skill its preset
    /// no longer grants.
    #[tokio::test]
    async fn a_changed_set_replaces_the_tree() {
        let root = tempfile::tempdir().unwrap();
        let first = zip_of(&[("skills/a/SKILL.md", "body-a")]);
        let second = zip_of(&[("skills/b/SKILL.md", "body-b")]);
        let (h1, h2) = (sha256_hex(&first), sha256_hex(&second));
        let source = FakeSource::default();
        source.zips.lock().unwrap().insert(h1.clone(), first);
        source.zips.lock().unwrap().insert(h2.clone(), second);
        let store = PluginStore::new(root.path().to_path_buf());

        store
            .provision_agent(
                "a1",
                &[BundleRef {
                    name: "one".into(),
                    hash: h1,
                }],
                &source,
            )
            .await
            .unwrap();
        let dir = store
            .provision_agent(
                "a1",
                &[BundleRef {
                    name: "two".into(),
                    hash: h2,
                }],
                &source,
            )
            .await
            .unwrap();

        assert!(dir.join("two").exists());
        assert!(!dir.join("one").exists(), "the old bundle outlived its set");
    }

    /// Fail-whole. A tree that is only partly built would give the agent a
    /// silently reduced skill set, and nothing downstream could tell.
    #[tokio::test]
    async fn a_bundle_that_cannot_be_had_fails_the_whole_call() {
        let root = tempfile::tempdir().unwrap();
        let (mut source, hash) = FakeSource::with(&[("skills/a/SKILL.md", "body-a")]);
        source.fail = Some("the artifact store is down".into());
        let store = PluginStore::new(root.path().to_path_buf());

        let err = store
            .provision_agent(
                "a1",
                &[BundleRef {
                    name: "pack".into(),
                    hash,
                }],
                &source,
            )
            .await
            .unwrap_err();

        assert!(err.contains("the artifact store is down"), "{err}");
        assert!(
            !store.agent_dir("a1").join("pack").exists(),
            "a failed provision must leave no half-built tree"
        );
    }

    /// Bytes that do not hash to what was asked for are not the bundle that was
    /// asked for, whatever they claim to be.
    #[tokio::test]
    async fn bytes_that_do_not_match_their_hash_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let source = FakeSource::default();
        source
            .zips
            .lock()
            .unwrap()
            .insert("not-the-real-hash".into(), zip_of(&[("x", "y")]));
        let store = PluginStore::new(root.path().to_path_buf());

        let err = store
            .provision_agent(
                "a1",
                &[BundleRef {
                    name: "pack".into(),
                    hash: "not-the-real-hash".into(),
                }],
                &source,
            )
            .await
            .unwrap_err();

        assert!(err.contains("hash mismatch"), "{err}");
        assert!(
            !root.path().join("store/not-the-real-hash").exists(),
            "unverified bytes must never be named after the hash they claimed"
        );
    }

    /// A name or hash from the wire must not be able to climb out of the store.
    /// Asserted as the two properties rather than an exact spelling — what
    /// matters is that nothing can traverse, not which character it became.
    #[test]
    fn a_name_from_the_wire_can_never_climb_out_of_the_store() {
        for hostile in [
            "../../etc/passwd",
            "..",
            "a/../../b",
            "....//....//x",
            ".hidden",
            "/absolute",
        ] {
            let safe = sanitize(hostile);
            assert!(
                !safe.contains('/') && !safe.contains('\\'),
                "{hostile:?} kept a separator: {safe:?}"
            );
            assert!(!safe.contains(".."), "{hostile:?} kept a `..`: {safe:?}");
            assert!(
                !safe.starts_with('.'),
                "{hostile:?} stayed hidden: {safe:?}"
            );
        }
        // An ordinary name survives intact — sanitising must not mangle the
        // common case into something a person cannot recognise in a path.
        assert_eq!(sanitize("ok-name_1.2"), "ok-name_1.2");
    }
}
