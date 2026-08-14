//! Turning a `cluster` config section into a node that has joined its peers.
//!
//! horsie-actor ships the cluster layer — membership behind Raft, liveness
//! observed by the leader and replicated, placement by rendezvous hashing over
//! the two. This module is the whole of horsie's side of it: the guards that
//! refuse a configuration which would look healthy while losing what it
//! carries, the three constructors in the right order, and the loop that drains
//! what peers send here.
//!
//! **Absent config means absent cluster.** A single-node deployment binds no
//! transport, opens no Raft store, and takes exactly the boot path it took
//! before any of this existed. That is the common case and it pays nothing.

use horsie_actor::{
    ActorSystem, ClusterConfig, ClusterNode, NodeId, RaftStore, TcpConfig, TcpTransport,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// One node's place in a cluster.
#[derive(Debug, Clone, Deserialize)]
pub struct ClusterSection {
    /// This node's identity, stable across restarts. A node that comes back
    /// under a different id is a different node as far as consensus is
    /// concerned.
    pub node_id: u64,
    /// Where this node listens for its peers.
    pub bind: SocketAddr,
    /// Where each peer listens.
    ///
    /// A **bootstrap** list, not live membership: it seeds consensus only while
    /// the Raft store is empty, and after that membership lives in the log.
    /// Editing it will not reshape a running cluster, and a node cannot join
    /// one without a config rollout.
    #[serde(default)]
    pub peers: HashMap<u64, SocketAddr>,
    /// The shared secret every node presents to every other.
    ///
    /// Required rather than defaulted, deliberately: an absent secret would
    /// mean an unauthenticated cluster port, and anyone able to reach it could
    /// inject envelopes straight into the actor system — a full bypass of
    /// horsie's auth rather than weaker hardening. It must be identical on
    /// every node. It authenticates but does **not** encrypt, so a private
    /// network or a TLS tunnel is a deployment requirement.
    pub secret: String,
    /// Where this node keeps its Raft vote. Defaults to `<state_dir>/cluster`.
    ///
    /// Configurable because it is per-node state with its own durability
    /// requirement: it must survive a restart of this node and must never be
    /// shared with another, so a deployment places it deliberately rather than
    /// inheriting wherever `state_dir` happens to point.
    #[serde(default)]
    pub raft_dir: Option<PathBuf>,
    /// How long a peer may go unacknowledged before the leader stops counting
    /// it live. Defaults to horsie-actor's own 3 seconds.
    #[serde(default)]
    pub liveness_window_secs: Option<u64>,
}

/// A clustered configuration that cannot be honoured.
#[derive(Debug, thiserror::Error)]
pub enum ClusterError {
    #[error(
        "cluster mode needs a Postgres journal: set database.url to a postgres:// URL, \
         or remove the cluster section"
    )]
    SqliteJournal,
    #[error(
        "cluster mode needs a shared bus: set bus.url to a redis:// URL, \
         or remove the cluster section"
    )]
    NoBus,
}

/// Everything the guards read, gathered from across the boot config.
pub struct ClusterInputs<'a> {
    pub section: &'a ClusterSection,
    pub database_url: Option<&'a str>,
    pub bus_url: Option<&'a str>,
    pub state_dir: &'a Path,
}

/// Check a clustered configuration and resolve where this node's vote lives.
///
/// Both refusals are the rule the bus already follows: a deployment that asked
/// for a shared thing and silently got a per-process one looks perfectly
/// healthy while losing everything that was supposed to cross between nodes.
///
/// # Errors
/// [`ClusterError::SqliteJournal`] on a journal that is not Postgres — three
/// nodes need one journal, and SQLite gives each its own file, so the cluster
/// would form, agree on placement, and then keep three divergent histories.
/// [`ClusterError::NoBus`] without a bus URL.
pub fn check(inputs: &ClusterInputs<'_>) -> Result<PathBuf, ClusterError> {
    let postgres = inputs
        .database_url
        .is_some_and(|u| u.starts_with("postgres://") || u.starts_with("postgresql://"));
    if !postgres {
        return Err(ClusterError::SqliteJournal);
    }
    if inputs.bus_url.is_none_or(str::is_empty) {
        return Err(ClusterError::NoBus);
    }
    Ok(inputs
        .section
        .raft_dir
        .clone()
        .unwrap_or_else(|| inputs.state_dir.join("cluster")))
}

/// Start this node's membership and return it.
///
/// Unreachable peers are **not** an error. A node whose peers are not up yet
/// starts, reports `serving() == false`, and begins serving when a quorum
/// appears — failing here would deadlock every rolling restart, since the first
/// node up would exit before the second could start.
///
/// # Errors
/// If the configuration is refused by [`check`], the Raft directory cannot be
/// created, the store cannot be opened, the bind fails, or Raft cannot start.
pub async fn start(
    inputs: &ClusterInputs<'_>,
) -> Result<Arc<ClusterNode>, Box<dyn std::error::Error + Send + Sync>> {
    let raft_dir = check(inputs)?;
    std::fs::create_dir_all(&raft_dir)?;
    // A corrupt store is reported rather than silently replaced: starting fresh
    // would discard a vote, which is how a node votes twice in one term.
    let store = RaftStore::open(raft_dir.join("raft.json"))?;

    let local = NodeId(inputs.section.node_id);
    let peers: HashMap<NodeId, SocketAddr> = inputs
        .section
        .peers
        .iter()
        .map(|(id, addr)| (NodeId(*id), *addr))
        .collect();
    let transport = TcpTransport::bind(TcpConfig {
        local,
        bind: inputs.section.bind,
        peers,
        secret: inputs.section.secret.clone().into_bytes(),
    })
    .await?;

    // This node plus its peers: the set a brand-new cluster is formed from,
    // consulted once and then never again.
    let mut bootstrap = vec![local];
    bootstrap.extend(inputs.section.peers.keys().map(|id| NodeId(*id)));
    let mut config = ClusterConfig::new(local, bootstrap);
    if let Some(secs) = inputs.section.liveness_window_secs {
        config.liveness_window = std::time::Duration::from_secs(secs);
    }

    ClusterNode::start(config, transport, store).await
}

/// Drain what peers send this node into the actor system.
///
/// Separate from [`start`] because the system does not exist yet when the node
/// does — `ActorSystem::clustered` takes the node, so this can only be spawned
/// once both halves are built. Without it a clustered node accepts connections
/// and agrees on placement while silently answering nothing addressed to it
/// from anywhere else.
pub fn pump(node: &Arc<ClusterNode>, system: ActorSystem) {
    let Some(mut inbox) = node.incoming() else {
        // One drainer is the invariant; a second would race it for every
        // envelope, and each would see half.
        tracing::error!("the cluster inbox was already taken; not starting a second pump");
        return;
    };
    tokio::spawn(async move {
        while let Some(message) = inbox.recv().await {
            if let Err(err) = system.dispatch(message).await {
                tracing::warn!(error = %err, "an envelope from a peer could not be dispatched");
            }
        }
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn section() -> ClusterSection {
        ClusterSection {
            node_id: 1,
            bind: "127.0.0.1:0".parse().unwrap(),
            peers: HashMap::new(),
            secret: "s".to_string(),
            raft_dir: None,
            liveness_window_secs: None,
        }
    }

    fn inputs<'a>(
        section: &'a ClusterSection,
        database_url: Option<&'a str>,
        bus_url: Option<&'a str>,
        state_dir: &'a Path,
    ) -> ClusterInputs<'a> {
        ClusterInputs {
            section,
            database_url,
            bus_url,
            state_dir,
        }
    }

    /// Three nodes need one journal. SQLite gives each its own file, so the
    /// cluster would form and then keep three divergent histories — every
    /// symptom of which appears a long way from the cause.
    #[test]
    fn a_cluster_on_sqlite_is_refused() {
        let s = section();
        let err = check(&inputs(
            &s,
            Some("sqlite:///data/horsie.db"),
            Some("redis://localhost"),
            Path::new("/state"),
        ))
        .unwrap_err();
        assert!(matches!(err, ClusterError::SqliteJournal));
    }

    /// The default journal is SQLite, so an absent url is the same fault as an
    /// explicit sqlite one — and is the likelier way to arrive at it.
    #[test]
    fn a_cluster_with_no_database_url_is_refused() {
        let s = section();
        let err = check(&inputs(
            &s,
            None,
            Some("redis://localhost"),
            Path::new("/state"),
        ))
        .unwrap_err();
        assert!(matches!(err, ClusterError::SqliteJournal));
    }

    /// `BusConfig` already documents that several nodes without a URL lose every
    /// message meant to cross between them. Nothing could be multi-node before,
    /// so nothing enforced it.
    #[test]
    fn a_cluster_without_a_bus_is_refused() {
        let s = section();
        let err = check(&inputs(
            &s,
            Some("postgres://localhost/horsie"),
            None,
            Path::new("/state"),
        ))
        .unwrap_err();
        assert!(matches!(err, ClusterError::NoBus));
    }

    #[test]
    fn the_raft_dir_defaults_under_the_state_dir() {
        let s = section();
        let dir = check(&inputs(
            &s,
            Some("postgres://localhost/horsie"),
            Some("redis://localhost"),
            Path::new("/state"),
        ))
        .unwrap();
        assert_eq!(dir, Path::new("/state/cluster"));
    }

    #[test]
    fn a_configured_raft_dir_wins() {
        let mut s = section();
        s.raft_dir = Some(PathBuf::from("/mnt/raft"));
        let dir = check(&inputs(
            &s,
            Some("postgres://localhost/horsie"),
            Some("redis://localhost"),
            Path::new("/state"),
        ))
        .unwrap();
        assert_eq!(dir, Path::new("/mnt/raft"));
    }

    /// A node whose peers are not up yet must still start, and must come up not
    /// serving. Failing here would deadlock every rolling restart.
    #[tokio::test]
    async fn peers_that_are_not_up_yet_do_not_fail_the_boot() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = section();
        // Nothing listens on either, and nothing will.
        s.peers.insert(2, "127.0.0.1:1".parse().unwrap());
        s.peers.insert(3, "127.0.0.1:2".parse().unwrap());
        let node = start(&inputs(
            &s,
            Some("postgres://localhost/horsie"),
            Some("redis://localhost"),
            dir.path(),
        ))
        .await
        .expect("unreachable peers must not fail the boot");
        assert!(!node.serving(), "a node without a quorum must not serve");
    }

    /// The pump takes the inbox, and there is only one. A second taker would
    /// mean nothing is draining what peers send here.
    #[tokio::test]
    async fn the_pump_takes_the_inbox() {
        let dir = tempfile::tempdir().unwrap();
        let s = section();
        let node = start(&inputs(
            &s,
            Some("postgres://localhost/horsie"),
            Some("redis://localhost"),
            dir.path(),
        ))
        .await
        .unwrap();
        pump(&node, ActorSystem::in_memory());
        assert!(node.incoming().is_none());
    }
}
