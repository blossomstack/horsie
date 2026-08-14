//! Three horsie nodes over one journal and one bus.
//!
//! The first test in this repo that needs more than one node, and the reason it
//! exists: every remaining cross-node bug fails the same silent way — the
//! connection stays open, keep-alives flow, and nothing ever arrives. None of
//! them can be *reproduced* without a second node, so none of them can be fixed
//! with a test that fails first.
//!
//! **Three rather than two, and not for thoroughness.** With three you can
//! assert both halves of quorum: a majority that keeps serving, and a minority
//! that stands down. With two, losing either leaves no quorum, so only the
//! standing-down half is expressible — and the half you lose is the one that
//! proves placement actually moved rather than everything merely stopping.
//!
//! Skips unless both `HORSIE_TEST_POSTGRES_URL` and `HORSIE_TEST_REDIS_URL` are
//! set, following `bus::tests::redis_url`. CI provides both. A cluster needs a
//! shared journal and a shared bus, and the boot refuses to start without
//! either — so there is nothing here that can be meaningfully faked.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use horsie_server::boot::{BootOptions, boot};
use horsie_server::cluster::ClusterSection;
use horsie_server::runtime_vendor::fake::FakeRuntimeVendor;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

/// How long to wait for three nodes to elect a leader and all report serving.
///
/// Generous against horsie-actor's own election timing (300–600ms) plus the
/// liveness window, because a loaded CI runner is the case this has to survive.
const FORMATION: Duration = Duration::from_secs(30);

struct Node {
    http: SocketAddr,
    client: reqwest::Client,
    /// Kept so the server keeps serving; aborted on drop.
    _task: tokio::task::JoinHandle<()>,
    /// Kept alive: dropping it closes the fake vendor's transport.
    ///
    /// One per node, because the vendor registry is node-local — a session
    /// placed on node 2 picks its runtime from node 2's map.
    _vendor: FakeRuntimeVendor,
    state: horsie_server::http::AppState,
}

impl Node {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.http)
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.client.get(self.url(path)).send().await.unwrap()
    }

    async fn post(&self, path: &str, body: &serde_json::Value) -> reqwest::Response {
        self.client
            .post(self.url(path))
            .json(body)
            .send()
            .await
            .unwrap()
    }

    /// `get`, retrying while this node says it is not serving. See
    /// [`Node::post_when_serving`] for why retrying is the correct behaviour
    /// rather than a workaround. Deliberately not used for `/api/health`,
    /// which is the thing being asserted about.
    async fn get_when_serving(&self, path: &str) -> reqwest::Response {
        let start = Instant::now();
        loop {
            let res = self.get(path).await;
            if res.status() != 503 || start.elapsed() > FORMATION {
                return res;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// `post`, retrying while this node says it is not serving.
    ///
    /// Not a workaround for flakiness: 503 from horsie means "this node cannot
    /// serve right now, retry" — so a client that retries is the behaviour the
    /// status code asks for, and a test that does not retry is asserting
    /// something no real client would. A node can stand down for a moment
    /// whenever a heartbeat is late, which on a shared CI runner is ordinary.
    async fn post_when_serving(&self, path: &str, body: &serde_json::Value) -> reqwest::Response {
        let start = Instant::now();
        loop {
            let res = self.post(path, body).await;
            if res.status() != 503 || start.elapsed() > FORMATION {
                return res;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Whether this node currently considers itself able to serve.
    fn serving(&self) -> bool {
        self.state
            .shared
            .serving
            .as_ref()
            .is_none_or(|rx| *rx.borrow())
    }
}

struct Cluster {
    nodes: Vec<Node>,
}

impl Cluster {
    /// Three nodes, or `None` when this run has no Postgres or no Redis.
    async fn start() -> Option<Self> {
        let postgres = env("HORSIE_TEST_POSTGRES_URL")?;
        let redis = env("HORSIE_TEST_REDIS_URL")?;

        // One database for all three: a cluster is nodes that agree on a
        // journal, so a database per node would be three clusters of one.
        //
        // The name *replaces* the base URL's database rather than being
        // appended to it — `…/postgres` plus `/name` is a path, not a database.
        let name = fresh_db_name();
        let host = postgres
            .trim_end_matches('/')
            .rsplit_once('/')
            .map_or(postgres.trim_end_matches('/'), |(head, _)| head);
        let db = format!("{host}/{name}");
        create_database(&postgres, &name).await;

        let ports: Vec<u16> = (0..3).map(|_| free_port()).collect();
        let addrs: HashMap<u64, SocketAddr> = ports
            .iter()
            .enumerate()
            .map(|(i, p)| (i as u64 + 1, format!("127.0.0.1:{p}").parse().unwrap()))
            .collect();

        let mut nodes = Vec::new();
        for id in 1..=3u64 {
            // Sequentially, not concurrently: the first boot runs the
            // migrations and creates the account, and three racing to do it
            // would be testing sqlx rather than horsie.
            nodes.push(start_node(id, &addrs, &db, &redis).await);
        }

        let cluster = Self { nodes };
        cluster.await_all_serving().await;
        Some(cluster)
    }

    fn node(&self, i: usize) -> &Node {
        &self.nodes[i]
    }

    /// Wait until every node has a leader and reports serving.
    ///
    /// Not a fixed sleep: election takes as long as it takes, and a fixed wait
    /// is either flaky or slow.
    async fn await_all_serving(&self) {
        let start = Instant::now();
        loop {
            if self.nodes.iter().all(Node::serving) {
                return;
            }
            assert!(
                start.elapsed() < FORMATION,
                "the cluster never formed: serving = {:?}",
                self.nodes.iter().map(Node::serving).collect::<Vec<_>>()
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

fn fresh_db_name() -> String {
    format!("horsie_cluster_{}", uuid::Uuid::new_v4().simple())
}

/// A port nothing is listening on, released immediately.
///
/// Peers address each other by number, so a port has to be known before any
/// node starts — `127.0.0.1:0` would only be resolvable after binding.
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

async fn create_database(base: &str, name: &str) {
    sqlx::any::install_default_drivers();
    let admin = sqlx::any::AnyPoolOptions::new()
        .max_connections(1)
        .connect(base)
        .await
        .unwrap_or_else(|e| panic!("connect to HORSIE_TEST_POSTGRES_URL: {e}"));
    // The name is a UUID minted here, never input, so interpolating it is safe
    // — `CREATE DATABASE` takes no bind parameters.
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&admin)
        .await
        .unwrap_or_else(|e| panic!("create {name}: {e}"));
}

async fn start_node(
    id: u64,
    addrs: &HashMap<u64, SocketAddr>,
    db_url: &str,
    bus_url: &str,
) -> Node {
    let dir = tempfile::tempdir().unwrap().keep();
    let peers = addrs
        .iter()
        .filter(|(peer, _)| **peer != id)
        .map(|(peer, addr)| (*peer, *addr))
        .collect();

    let mut opts = BootOptions::new(dir.join("state"), dir.join("data"));
    opts.db_url = Some(db_url.to_string());
    opts.bus_url = Some(bus_url.to_string());
    opts.auth_mode = horsie_server::auth::AuthMode::Off;
    opts.cluster = Some(ClusterSection {
        node_id: id,
        bind: addrs[&id],
        peers,
        // Identical on every node, which is the whole point of the field. A
        // mismatch would present as peers that never handshake.
        secret: "cluster-e2e-secret".to_string(),
        // Per node: this is that node's vote, and two nodes over one file is
        // two nodes voting as one.
        raft_dir: Some(dir.join("raft")),
        // Deliberately generous. Three of these clusters run at once on one
        // CI runner — nine Raft nodes — and a heartbeat that arrives late
        // because the scheduler was busy is indistinguishable from a node that
        // died. Two seconds is fine on a quiet machine and produces spurious
        // stand-downs on a loaded one.
        liveness_window_secs: Some(10),
    });

    let booted = boot(opts).await.expect("a node boots");

    // Every node publishes a vendor under the same name. A session is built by
    // whichever node the cluster places it on, and it resolves `main` from that
    // node's registry — so a vendor on one node only is a session that works or
    // fails depending on placement.
    let vendor = FakeRuntimeVendor::builder("main")
        .serve_in_process()
        .await
        .expect("fake vendor");
    let account = booted.state.shared.anonymous.clone();
    booted
        .state
        .users
        .get(&account)
        .await
        .unwrap()
        .connected_vendors
        .publish(vendor.link())
        .expect("`main` is unclaimed on this node");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = listener.local_addr().unwrap();
    let state = booted.state.clone();
    let app = horsie_server::http::app(booted.state);
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    Node {
        http,
        client: reqwest::Client::new(),
        _task: task,
        _vendor: vendor,
        state,
    }
}

/// Every node answers, and answers as itself.
///
/// The cheapest thing that can only pass with three nodes actually up: three
/// distinct HTTP servers over one journal, each having formed consensus.
#[tokio::test]
async fn three_nodes_form_a_cluster_and_all_report_ready() {
    let Some(c) = Cluster::start().await else {
        eprintln!("skipped: needs HORSIE_TEST_POSTGRES_URL and HORSIE_TEST_REDIS_URL");
        return;
    };
    for i in 0..3 {
        // `serving` is `Some` only on a node that actually joined a cluster.
        // Asserted because health alone cannot tell the two apart: an
        // unclustered node also answers 200, so without this the test would
        // pass against three servers that merely share a database.
        assert!(
            c.node(i).state.shared.serving.is_some(),
            "node {i} should have joined a cluster"
        );
        let res = c.node(i).get("/api/health").await;
        assert_eq!(res.status(), 200, "node {i} should be ready");
    }
}

/// A session created against one node is readable through another.
///
/// The property this whole layer exists for. It fails on a single-node build
/// not by erroring but by being unwritable — there is no second node to read
/// from — which is exactly why the delivery bugs above it went unnoticed.
#[tokio::test]
async fn a_session_created_on_one_node_is_readable_on_another() {
    let Some(c) = Cluster::start().await else {
        eprintln!("skipped: needs HORSIE_TEST_POSTGRES_URL and HORSIE_TEST_REDIS_URL");
        return;
    };

    let created = c
        .node(0)
        .post_when_serving(
            "/api/sessions",
            &serde_json::json!({
                "agent": { "model": "mock", "use_plugins": false },
                "environment": {"type": "Runtime", "value": {"vendor": "main"}},
                "message": "hi"
            }),
        )
        .await;
    assert_eq!(created.status(), 201, "node 0 should create the session");
    let body: serde_json::Value = created.json().await.unwrap();
    let id = body["session"]["id"]
        .as_str()
        .expect("a session id")
        .to_string();

    // Read it from a *different* node. The session actor is placed by the
    // cluster, so this either resolves across hosts or it does not resolve.
    let read = c
        .node(1)
        .get_when_serving(&format!("/api/sessions/{id}"))
        .await;
    assert_eq!(
        read.status(),
        200,
        "node 1 must reach a session created through node 0"
    );
    let read: serde_json::Value = read.json().await.unwrap();
    assert_eq!(
        read["session"]["id"].as_str().or(read["id"].as_str()),
        Some(id.as_str()),
        "node 1 must return the same session node 0 created"
    );
}

/// And the session list, which is the supervisor rather than the session — a
/// separately-placed actor, so it can be on a third node again.
#[tokio::test]
async fn every_node_lists_the_same_sessions() {
    let Some(c) = Cluster::start().await else {
        eprintln!("skipped: needs HORSIE_TEST_POSTGRES_URL and HORSIE_TEST_REDIS_URL");
        return;
    };

    c.node(0)
        .post_when_serving(
            "/api/sessions",
            &serde_json::json!({
                "agent": { "model": "mock", "use_plugins": false },
                "environment": {"type": "Runtime", "value": {"vendor": "main"}},
                "message": "hi"
            }),
        )
        .await;

    for i in 0..3 {
        let res = c.node(i).get_when_serving("/api/sessions").await;
        assert_eq!(res.status(), 200, "node {i} should list sessions");
        let page: serde_json::Value = res.json().await.unwrap();
        let count = page["sessions"].as_array().map_or(0, Vec::len);
        assert_eq!(count, 1, "node {i} should see the one session that exists");
    }
}
