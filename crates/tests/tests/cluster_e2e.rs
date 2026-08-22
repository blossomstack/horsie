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
    /// The project every scoped request below names. One per cluster, not one
    /// per node: the nodes share a database, so they share its projects.
    project: String,
}

impl Node {
    /// A raw path, for the routes that belong to no project — `/api/health` is
    /// the only one this suite asks for.
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.http)
    }

    /// A path inside the project, which is every other route.
    fn api(&self, path: &str) -> String {
        format!("http://{}/api/p/{}{path}", self.http, self.project)
    }

    async fn get_raw(&self, path: &str) -> reqwest::Response {
        self.client.get(self.url(path)).send().await.unwrap()
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.client.get(self.api(path)).send().await.unwrap()
    }

    async fn post(&self, path: &str, body: &serde_json::Value) -> reqwest::Response {
        self.client
            .post(self.api(path))
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

    /// Open an SSE connection and start collecting what it sends.
    ///
    /// The response headers are awaited here rather than on the task, so that
    /// by the time this returns the handler is running. A test that raced its
    /// own subscription would be deciding on timing rather than on delivery,
    /// which for a bug whose signature is "nothing ever arrives" is the one
    /// mistake that turns the assertion into noise.
    async fn subscribe(&self, path: &str) -> Frames {
        let client = self.client.clone();
        let url = self.api(path);
        let opened = Self::open_stream(&client, &url).await;
        assert!(
            opened.is_some(),
            "{path} should have started streaming within {FORMATION:?}"
        );

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            use futures_util::StreamExt;
            let deadline = Instant::now() + FORMATION;
            let mut res = opened;
            // Reconnecting, not one connection read to its end. `/events` ends
            // the stream on purpose whenever its supervisor ask fails — a node
            // that has momentarily stood down — and says so in as many words:
            // the client reconnects and may land on a node that can serve it.
            // A reader that treats the close as final is asserting something no
            // real client would, which is the same reason the requests above
            // retry a 503. On reconnect the handler replays the current list as
            // its first frame, so nothing is missed by re-opening.
            while let Some(stream) = res.take() {
                let mut body = stream.bytes_stream();
                let mut buffer = String::new();
                while let Some(Ok(chunk)) = body.next().await {
                    buffer.push_str(&String::from_utf8_lossy(&chunk));
                    // A blank line ends a frame; `data:` is the only line any
                    // assertion here cares about.
                    while let Some(end) = buffer.find("\n\n") {
                        let frame: String = buffer.drain(..end + 2).collect();
                        for data in frame.lines().filter_map(|l| l.strip_prefix("data:")) {
                            let Ok(value) = serde_json::from_str::<serde_json::Value>(data.trim())
                            else {
                                continue;
                            };
                            if tx.send(value).is_err() {
                                return; // the test stopped reading
                            }
                        }
                    }
                }
                if Instant::now() >= deadline {
                    return;
                }
                res = Self::open_stream(&client, &url).await;
            }
        });
        Frames(rx)
    }

    /// Open one SSE connection, retrying while the node says it is not serving.
    ///
    /// `None` once [`FORMATION`] has passed without a 200, which is the caller's
    /// signal that this is a failure rather than a node still standing up.
    async fn open_stream(client: &reqwest::Client, url: &str) -> Option<reqwest::Response> {
        let start = Instant::now();
        loop {
            if let Ok(res) = client.get(url).send().await {
                if res.status() == 200 {
                    return Some(res);
                }
                if res.status() != 503 {
                    return None;
                }
            }
            if start.elapsed() > FORMATION {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Every entry of one agent's transcript, read rather than streamed.
    async fn transcript(&self, id: &str) -> serde_json::Value {
        let res = self
            .get_when_serving(&format!("/sessions/{id}/messages?max=200"))
            .await;
        let status = res.status();
        let body = res.text().await.unwrap();
        assert_eq!(status, 200, "a transcript should be readable: {body}");
        serde_json::from_str(&body).unwrap()
    }

    /// Wait until this agent has finished `want` turns.
    ///
    /// The turn boundary, and not `Idle` — a session reports that both when
    /// provisioning finishes and when a turn ends, so waiting on it can return
    /// before the turn has run at all. `session_server_e2e.rs` settled this and
    /// the reasoning is the same here.
    async fn await_turns(&self, id: &str, want: usize) {
        let start = Instant::now();
        loop {
            let page = self.transcript(id).await;
            let ended = page["entries"]
                .as_array()
                .map(|entries| {
                    entries
                        .iter()
                        .filter(|e| e["body"]["type"] == serde_json::json!("Lifecycle"))
                        .filter(|e| e["body"]["value"]["kind"] == serde_json::json!("TurnEnded"))
                        .count()
                })
                .unwrap_or_default();
            if ended >= want {
                return;
            }
            assert!(
                start.elapsed() < FORMATION,
                "timed out waiting for {want} finished turns; {ended} have ended"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
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

/// A live SSE connection's frames, decoded.
///
/// Collected on their own task rather than read inline, because every
/// assertion below is about what arrives *after* something happens on another
/// node — so the connection has to stay open across that.
struct Frames(tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>);

impl Frames {
    /// The next frame satisfying `want`, or a panic naming what did arrive.
    ///
    /// Reporting the frames it passed over matters more here than usual: the
    /// bugs these tests cover fail by delivering *nothing*, and "nothing
    /// arrived" and "the wrong thing arrived" want different fixes.
    async fn find(
        &mut self,
        what: &str,
        want: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let deadline = Instant::now() + FORMATION;
        let mut seen: Vec<serde_json::Value> = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, self.0.recv()).await {
                Ok(Some(frame)) if want(&frame) => return frame,
                Ok(Some(frame)) => seen.push(frame),
                Ok(None) => panic!("the stream ended before {what} arrived; it sent {seen:?}"),
                Err(_) => panic!("timed out waiting for {what}; the stream sent {seen:?}"),
            }
        }
    }
}

/// The body that creates a session against the mock provider.
fn new_session(message: &str) -> serde_json::Value {
    serde_json::json!({
        "agent": { "model": "mock", "use_plugins": false },
        "environment": {"type": "Runtime", "value": {"vendor": "main"}},
        "message": message
    })
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
    // This suite makes a database per test and cannot drop it at the end — the
    // nodes outlive the test body. Collected on the way in instead, and only
    // ones nothing is connected to, so a binary running beside this one keeps
    // its own.
    horsie_server::db::testing::sweep_abandoned_test_databases(base).await;
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
    let account = booted
        .state
        .shared
        .project_service
        .default_project(&booted.state.shared.anonymous)
        .await
        .expect("the anonymous account has a default project")
        .id;
    booted
        .state
        .projects
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
        project: account.as_str().to_string(),
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
        let res = c.node(i).get_raw("/api/health").await;
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
            "/sessions",
            &serde_json::json!({
                "agent": { "model": "mock", "use_plugins": false },
                "environment": {"type": "Runtime", "value": {"vendor": "main"}},
                "message": "hi"
            }),
        )
        .await;
    // The body before the status, so a create that failed says why rather than
    // leaving a bare number to guess from.
    let status = created.status();
    let body: serde_json::Value = created.json().await.unwrap();
    assert_eq!(status, 201, "node 0 should create the session: {body}");
    let id = body["session"]["id"]
        .as_str()
        .expect("a session id")
        .to_string();

    // Read it from a *different* node. The session actor is placed by the
    // cluster, so this either resolves across hosts or it does not resolve.
    let read = c.node(1).get_when_serving(&format!("/sessions/{id}")).await;
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
            "/sessions",
            &serde_json::json!({
                "agent": { "model": "mock", "use_plugins": false },
                "environment": {"type": "Runtime", "value": {"vendor": "main"}},
                "message": "hi"
            }),
        )
        .await;

    for i in 0..3 {
        let res = c.node(i).get_when_serving("/sessions").await;
        assert_eq!(res.status(), 200, "node {i} should list sessions");
        let page: serde_json::Value = res.json().await.unwrap();
        let count = page["sessions"].as_array().map_or(0, Vec::len);
        assert_eq!(count, 1, "node {i} should see the one session that exists");
    }
}

/// A session created on one node wakes a list reader on another.
///
/// The `/api/events` bug in its exact shape. The feed was a `broadcast::Sender`
/// on the account's bundle — a pointer into one process — so a reader whose
/// connection landed on node 1 never heard about a session whose supervisor was
/// on node 0. It failed by staying open: keep-alives kept arriving and no event
/// ever did, which is why nothing caught it until there was a second node to
/// watch from.
#[tokio::test]
async fn a_session_created_on_one_node_wakes_a_list_reader_on_another() {
    let Some(c) = Cluster::start().await else {
        eprintln!("skipped: needs HORSIE_TEST_POSTGRES_URL and HORSIE_TEST_REDIS_URL");
        return;
    };

    let mut frames = c.node(1).subscribe("/events").await;
    // The opening frame, and the reason this test cannot pass by accident: it
    // establishes that node 1 is listening, and that it is listening to an
    // empty list, before node 0 is touched at all.
    let opening = frames.find("the opening list", |_| true).await;
    assert_eq!(
        opening["sessions"].as_array().map_or(1, Vec::len),
        0,
        "no session exists yet: {opening}"
    );

    let created = c
        .node(0)
        .post_when_serving("/sessions", &new_session("hi"))
        .await;
    // The body before the status, so a create that failed says why rather than
    // leaving a bare number to guess from.
    let status = created.status();
    let body: serde_json::Value = created.json().await.unwrap();
    assert_eq!(status, 201, "node 0 should create the session: {body}");
    let id = body["session"]["id"].as_str().expect("a session id");

    let listed = frames
        .find("the new session", |frame| {
            frame["sessions"].as_array().is_some_and(|s| !s.is_empty())
        })
        .await;
    assert_eq!(
        listed["sessions"][0]["id"].as_str(),
        Some(id),
        "node 1's feed must carry the session node 0 created"
    );
}

/// A turn on one node moves a message reader on another.
///
/// The asymmetric half of the same bug, and the one no `ask` could fix by
/// itself: the *session* actor moves an agent's revision, the *supervisor*
/// answers reads of it, and the shard model places those two independently. The
/// transcript always crossed — reading it is an ask — so a reader received
/// everything that existed when it connected and then nothing ever again,
/// having parked forever on a counter nobody on its node was moving.
#[tokio::test]
async fn a_turn_on_one_node_moves_a_message_reader_on_another() {
    let Some(c) = Cluster::start().await else {
        eprintln!("skipped: needs HORSIE_TEST_POSTGRES_URL and HORSIE_TEST_REDIS_URL");
        return;
    };

    let created = c
        .node(0)
        .post_when_serving("/sessions", &new_session("hi"))
        .await;
    // The body before the status, so a create that failed says why rather than
    // leaving a bare number to guess from.
    let status = created.status();
    let body: serde_json::Value = created.json().await.unwrap();
    assert_eq!(status, 201, "node 0 should create the session: {body}");
    let id = body["session"]["id"]
        .as_str()
        .expect("a session id")
        .to_string();

    // The opening turn has to be over before the reader connects. A reader that
    // is still working through a backlog picks the next message up on its way
    // past, with nothing crossing a node — and this test would then pass on a
    // build where the counter never arrives.
    c.node(2).await_turns(&id, 1).await;

    // Node 2 reads, node 0 writes, and the session is placed by the cluster —
    // so neither is necessarily the node it runs on, which is the point.
    let mut frames = c
        .node(2)
        .subscribe(&format!("/sessions/{id}/messages"))
        .await;
    frames
        .find("the transcript so far", |frame| {
            frame.to_string().contains("hi")
        })
        .await;

    let marker = "a-second-turn-started-on-another-node";
    let sent = c
        .node(0)
        .post_when_serving(
            &format!("/sessions/{id}/messages"),
            &serde_json::json!({ "text": marker }),
        )
        .await;
    assert_eq!(sent.status(), 202, "node 0 should accept the message");

    frames
        .find("the second turn", |frame| {
            frame.to_string().contains(marker)
        })
        .await;
}
