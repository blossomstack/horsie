//! Projects on one server, and no way for any of them to reach another.
//!
//! The runtime-tier twin of `project_isolation.rs`. That one proves the *rows*
//! are separate; this one proves everything above them is — the session list,
//! the live event stream, and the vendor map a session picks its runtime from.
//!
//! Two axes, and both matter:
//!
//! * **Two accounts.** The older one. This is the file that fails when a
//!   handler keeps reaching for something process-global.
//! * **Two projects of one account.** The new one, and the one with no
//!   credential boundary underneath it: `a` and `a2` below present the *same
//!   token*, so nothing about the request distinguishes them except the
//!   `{project}` segment. Every guarantee here has to hold on that evidence
//!   alone.
//!
//! The accounts here are two tokens rather than two `auth_users` rows, because
//! `AuthStore::create_user` enforces the single-account rule this repo ships
//! with — provisioning accounts is left to the deployment.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use horsie_server::auth::{AuthStore, Principal, TokenKind, UserId};
use horsie_server::projects::{ProjectId, ProjectRegistry};
use horsie_server::runtime_vendor::fake::FakeRuntimeVendor;
use std::net::SocketAddr;
use std::sync::Arc;

/// One project, the credential that reaches it, and a fake vendor process
/// connected to it — under the same name as everyone else's.
struct Client {
    user: UserId,
    project: ProjectId,
    token: String,
    /// Kept alive: dropping it closes the fake vendor's transport.
    _vendor: FakeRuntimeVendor,
}

struct Fixture {
    addr: SocketAddr,
    client: reqwest::Client,
    projects: Arc<ProjectRegistry>,
    a: Client,
    /// A second project of **`a`'s account**, reached with `a`'s own token. The
    /// path segment is the only thing that tells the two apart.
    a2: Client,
    b: Client,
    task: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn shutdown(self) {
        self.task.abort();
    }

    /// The bundle a given client's requests resolve to.
    async fn services(&self, c: &Client) -> Arc<horsie_server::projects::ProjectServices> {
        self.projects.get(&c.project).await.unwrap()
    }

    /// `path` is relative to the project, as every scoped route is.
    fn url(&self, c: &Client, path: &str) -> String {
        format!("http://{}/api/p/{}{path}", self.addr, c.project)
    }

    /// The same, but in a project the caller may not own — the shape of every
    /// "can B reach A's things" assertion below.
    fn url_in(&self, target: &Client, path: &str) -> String {
        format!("http://{}/api/p/{}{path}", self.addr, target.project)
    }

    async fn get(&self, c: &Client, path: &str) -> reqwest::Response {
        self.client
            .get(self.url(c, path))
            .bearer_auth(&c.token)
            .send()
            .await
            .unwrap()
    }

    /// `as_who` presents the credential; `at` names the project in the path.
    async fn get_at(&self, as_who: &Client, at: &Client, path: &str) -> reqwest::Response {
        self.client
            .get(self.url_in(at, path))
            .bearer_auth(&as_who.token)
            .send()
            .await
            .unwrap()
    }

    async fn post(&self, c: &Client, path: &str, body: &serde_json::Value) -> reqwest::Response {
        self.client
            .post(self.url(c, path))
            .bearer_auth(&c.token)
            .json(body)
            .send()
            .await
            .unwrap()
    }
}

/// A fresh account, its default project, a token that reaches it, and a vendor
/// process named `main`.
///
/// Every project uses the *same* vendor name on purpose: that collision is the
/// bug the per-scope map exists to make impossible.
async fn client(projects: &ProjectRegistry, store: &AuthStore) -> Client {
    let user = UserId::generate();
    // `Access`, not `Agent`: what these tests need is a *login* scoped to an
    // account, and a machine token is not one — it reaches one project's vendor
    // socket and nothing else. The vendor below is served in process and never
    // presents this credential anyway.
    let token = horsie_server::auth::generate(TokenKind::Access);
    store
        .insert_token(
            &format!("tok-{}", user.as_str()),
            TokenKind::Access,
            &Principal::User(user.clone()),
            &token.hash,
            Some("test"),
            None,
            None,
            0,
        )
        .await
        .unwrap();

    let project = projects
        .shared()
        .project_service
        .default_project(&user)
        .await
        .expect("a fresh account gets a default project")
        .id;
    with_vendor(projects, user, project, token.secret).await
}

/// A second project of `owner`'s account, reached with `owner`'s own token.
///
/// The point of the whole file's new half: nothing distinguishes a request for
/// this project from one for `owner`'s except the path.
async fn second_project_of(projects: &ProjectRegistry, owner: &Client) -> Client {
    let project = projects
        .shared()
        .project_service
        .create(&owner.user, "second")
        .await
        .expect("a second project is created")
        .id;
    with_vendor(projects, owner.user.clone(), project, owner.token.clone()).await
}

async fn with_vendor(
    projects: &ProjectRegistry,
    user: UserId,
    project: ProjectId,
    token: String,
) -> Client {
    let vendor = FakeRuntimeVendor::builder("main")
        .serve_in_process()
        .await
        .expect("fake vendor");
    projects
        .get(&project)
        .await
        .unwrap()
        .connected_vendors
        .publish(vendor.link())
        .expect("`main` is unclaimed in every project, not just the first");

    Client {
        user,
        project,
        token,
        _vendor: vendor,
    }
}

async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap().keep();
    // The default backend, not a SQLite file: nothing here restarts, so this
    // suite has no reason to pin one — and taking the default means the
    // PostgreSQL CI run covers the isolation queries too.
    let built = horsie_server::testing::state(&dir)
        .auth(horsie_server::auth::AuthMode::Password)
        .build()
        .await;
    let projects = built.state.projects.clone();

    // Two tokens rather than two `auth_users` rows: `create_user` enforces the
    // single-account rule this repo ships with, and a token *is* the scope.
    let store = AuthStore::new(built.state.shared.db.clone());
    let a = client(&projects, &store).await;
    let a2 = second_project_of(&projects, &a).await;
    let b = client(&projects, &store).await;

    let (addr, task) = built.serve().await;

    Fixture {
        addr,
        client: reqwest::Client::new(),
        projects,
        a,
        a2,
        b,
        task,
    }
}

/// Create a session on `main` — the vendor name both accounts claimed.
async fn create_session(f: &Fixture, c: &Client) -> String {
    let res = f
        .post(
            c,
            "/sessions",
            &serde_json::json!({
                "agent": { "model": "mock", "use_plugins": false },
                "environment": {"type": "Runtime", "value": {"vendor": "main"}},
                "message": "hi"
            }),
        )
        .await;
    assert_eq!(res.status().as_u16(), 201);
    let v: serde_json::Value = res.json().await.unwrap();
    v["session"]["id"].as_str().unwrap().to_string()
}

/// The session list is the supervisor's event-sourced state, one actor per
/// account. Not a filtered view of one list — two lists.
#[tokio::test]
async fn a_session_belongs_to_one_account_and_is_invisible_to_the_other() {
    let f = fixture().await;
    let id = create_session(&f, &f.a).await;

    let mine: serde_json::Value = f.get(&f.a, "/sessions").await.json().await.unwrap();
    assert_eq!(mine["sessions"].as_array().unwrap().len(), 1);

    let theirs: serde_json::Value = f.get(&f.b, "/sessions").await.json().await.unwrap();
    assert!(
        theirs["sessions"].as_array().unwrap().is_empty(),
        "another account's session list must not carry mine: {theirs}"
    );

    // Not merely hidden from the list — unreachable by id, which is the harder
    // half: the id is a UUID the other account could have been told.
    let res = f.get(&f.b, &format!("/sessions/{id}")).await;
    assert_eq!(res.status().as_u16(), 404);
    let res = f
        .post(
            &f.b,
            &format!("/sessions/{id}/messages"),
            &serde_json::json!({ "text": "hello" }),
        )
        .await;
    assert_eq!(res.status().as_u16(), 404);

    // And deleting is not a way around reading.
    let res = f
        .client
        .delete(f.url_in(&f.a, &format!("/sessions/{id}")))
        .bearer_auth(&f.b.token)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 404);
    let mine: serde_json::Value = f.get(&f.a, "/sessions").await.json().await.unwrap();
    assert_eq!(
        mine["sessions"].as_array().unwrap().len(),
        1,
        "the owner's session survived the other account's delete"
    );

    f.shutdown().await;
}

/// The one boundary a session's own id cannot supply.
///
/// `SessionActor`'s persistence id is `("session", <uuid>)` with **no account in
/// it**, so nothing about the actor says whose it is. The supervisor's
/// session-list check is the entirety of the isolation on this path, and this
/// is the test that fails when it is removed.
///
/// It carries more weight than it used to. An agent's revision now travels
/// between nodes on a per-session topic, and every wait still goes through the
/// supervisor for exactly this reason — letting a reader follow a session's
/// topic directly would be fewer hops and would move the boundary onto an id
/// that another account could simply have been told.
#[tokio::test]
async fn one_account_cannot_open_anothers_message_stream() {
    let f = fixture().await;
    let id = create_session(&f, &f.a).await;

    // The owner can, which is what makes the refusal below a statement about
    // the account rather than about the stream being unavailable to anyone.
    let mine = f.get(&f.a, &format!("/sessions/{id}/messages")).await;
    assert_eq!(
        mine.status().as_u16(),
        200,
        "the owner must be able to read its own stream"
    );
    drop(mine);

    let theirs = f.get(&f.b, &format!("/sessions/{id}/messages")).await;
    assert_eq!(
        theirs.status().as_u16(),
        404,
        "another account must not be able to open this stream"
    );

    f.shutdown().await;
}

/// Two agents, both announcing `main`. Before per-account maps the second was
/// refused outright — and had it been accepted, the first account's session
/// would have run its tools on the second's machine.
#[tokio::test]
async fn the_same_vendor_name_in_two_accounts_is_two_runtimes() {
    let f = fixture().await;

    let sa = f.services(&f.a).await;
    let sb = f.services(&f.b).await;
    assert_eq!(
        sa.connected_vendors.connected_names(),
        vec!["main".to_string()]
    );
    assert_eq!(
        sb.connected_vendors.connected_names(),
        vec!["main".to_string()]
    );

    // Identity is a websocket-vendor property, so it is read from the typed
    // table the registry publishes rather than from the trait.
    let (a_link, b_link) = (
        sa.connected_vendors
            .links()
            .lock()
            .unwrap()
            .get("main")
            .cloned()
            .unwrap(),
        sb.connected_vendors
            .links()
            .lock()
            .unwrap()
            .get("main")
            .cloned()
            .unwrap(),
    );
    assert_ne!(
        a_link.instance_id(),
        b_link.instance_id(),
        "`main` must resolve to each account's own vendor process"
    );

    // Both accounts can create on it, which is the denial-of-service the flat
    // map caused in miniature: the first claimant used to own the name forever.
    create_session(&f, &f.a).await;
    create_session(&f, &f.b).await;

    f.shutdown().await;
}

/// `/api/events` reports one account's session list and never another's.
///
/// One revision per account, so there is no path between them for a filter to
/// have to police. The counter replaced a per-account broadcast channel; the
/// isolation argument is unchanged and so is this test's shape — two objects,
/// not one object and a scope check.
#[tokio::test]
async fn the_global_event_stream_carries_only_its_own_accounts_changes() {
    let f = fixture().await;
    let a = f.services(&f.a).await;
    let b = f.services(&f.b).await;

    let a_before = *a.revisions.list().borrow();
    let b_before = *b.revisions.list().borrow();

    create_session(&f, &f.a).await;

    assert_ne!(
        *a.revisions.list().borrow(),
        a_before,
        "the owner's list must move"
    );
    assert_eq!(
        *b.revisions.list().borrow(),
        b_before,
        "another account's list must not move, not merely be filtered"
    );

    f.shutdown().await;
}

/// Configuration and presets are per-account too. The data tier proves the rows
/// are separate; this proves the HTTP surface hands back the right ones — and
/// that a name taken in one account is still free in the other, which is the
/// half that "hidden" alone would not give.
#[tokio::test]
async fn configuration_and_presets_do_not_cross_accounts() {
    let f = fixture().await;
    // Two writes now, one per resource, and both must be scoped to the caller.
    let put_config = async |account: &Client, key: &str| {
        let res = f
            .client
            .put(f.url(account, "/config/model-providers/p"))
            .bearer_auth(&account.token)
            .json(&serde_json::json!({
                "name": "p", "kind": "anthropic",
                "baseUrl": "http://127.0.0.1:1", "apiKey": key
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status().as_u16(), 200, "provider write");
        f.client
            .put(f.url(account, "/config/models/shared-alias"))
            .bearer_auth(&account.token)
            .json(&serde_json::json!({
                "alias": "shared-alias", "provider": "p", "modelId": "m"
            }))
            .send()
            .await
            .unwrap()
    };

    assert_eq!(put_config(&f.a, "sk-a").await.status().as_u16(), 200);

    let theirs: serde_json::Value = f.get(&f.b, "/config").await.json().await.unwrap();
    assert!(
        theirs["models"].as_array().unwrap().is_empty(),
        "another account's models must not appear: {theirs}"
    );
    assert!(theirs["providers"].as_array().unwrap().is_empty());

    // The same provider name and model alias are still free here — `providers`
    // and `models` are keyed on `(user_id, name)`, not on name alone.
    assert_eq!(put_config(&f.b, "sk-b").await.status().as_u16(), 200);
    for account in [&f.a, &f.b] {
        let view: serde_json::Value = f.get(account, "/config").await.json().await.unwrap();
        assert_eq!(view["models"].as_array().unwrap().len(), 1);
        assert_eq!(view["providers"].as_array().unwrap().len(), 1);
    }

    let preset = serde_json::json!({ "name": "reviewer", "model": "shared-alias" });
    assert_eq!(
        f.post(&f.a, "/agents", &preset).await.status().as_u16(),
        201
    );
    let theirs: serde_json::Value = f.get(&f.b, "/agents").await.json().await.unwrap();
    assert!(
        theirs.as_array().unwrap().is_empty(),
        "another account's presets must not appear: {theirs}"
    );
    assert_eq!(
        f.post(&f.b, "/agents", &preset).await.status().as_u16(),
        201,
        "a preset name taken in one account must still be free in another"
    );

    f.shutdown().await;
}

/// The axis with no credential behind it.
///
/// `a` and `a2` are one account's two projects and present the *same token*, so
/// nothing about either request distinguishes them except the `{project}`
/// segment. Every isolation guarantee in this file has to hold on that evidence
/// alone — which is why the scope is the project rather than the principal, and
/// why `Scope` resolves a bundle per project rather than filtering one.
#[tokio::test]
async fn two_projects_of_one_account_cannot_see_each_other() {
    let f = fixture().await;
    assert_eq!(f.a.token, f.a2.token, "the premise: one credential");
    assert_ne!(f.a.project, f.a2.project);

    let id = create_session(&f, &f.a).await;

    let mine: serde_json::Value = f.get(&f.a, "/sessions").await.json().await.unwrap();
    assert_eq!(mine["sessions"].as_array().unwrap().len(), 1);

    let sibling: serde_json::Value = f.get(&f.a2, "/sessions").await.json().await.unwrap();
    assert!(
        sibling["sessions"].as_array().unwrap().is_empty(),
        "one project's session list must not carry another's: {sibling}"
    );

    // Naming the session in the sibling project must not reach it either. This
    // is the assertion a filter would pass and a wrong scope key would fail:
    // the token owns both projects, so a principal check alone says yes.
    let res = f
        .client
        .get(f.url_in(&f.a2, &format!("/sessions/{id}")))
        .bearer_auth(&f.a2.token)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 404);

    // Configuration too: a provider written in one is absent from the other,
    // and its name is still free there.
    let provider = serde_json::json!({
        "name": "p", "kind": "anthropic",
        "baseUrl": "http://127.0.0.1:1", "apiKey": "sk-a"
    });
    for c in [&f.a, &f.a2] {
        let res = f
            .client
            .put(f.url(c, "/config/model-providers/p"))
            .bearer_auth(&c.token)
            .json(&provider)
            .send()
            .await
            .unwrap();
        assert_eq!(
            res.status().as_u16(),
            200,
            "a provider name taken in one project must still be free in the next"
        );
    }

    // Asserted at the supervisor as well as through the list, because the two
    // can fail for different reasons: a filter in the handler would pass the
    // HTTP check while both projects still shared one actor. Two supervisors
    // is the property; the empty list is only its symptom.
    let sessions = f
        .services(&f.a2)
        .await
        .supervisor
        .ask(|reply| horsie_server::sessions::supervisor::SessionSupervisorCommand::List { reply })
        .await
        .unwrap();
    assert!(
        sessions.is_empty(),
        "the sibling project's supervisor must hold no sessions: {sessions:?}"
    );

    // And the vendor map: `main` in one project is not `main` in the other.
    let (sa, sa2) = (f.services(&f.a).await, f.services(&f.a2).await);
    let link = |s: &Arc<horsie_server::projects::ProjectServices>| {
        s.connected_vendors
            .links()
            .lock()
            .unwrap()
            .get("main")
            .cloned()
            .unwrap()
    };
    assert_ne!(
        link(&sa).instance_id(),
        link(&sa2).instance_id(),
        "`main` must resolve to each project's own vendor process"
    );

    f.shutdown().await;
}

/// A project id belonging to somebody else answers 404, not 403.
///
/// A 403 would confirm the id names a real project, which is the one thing an
/// unguessable id is for. The distinction is testable because `b` holds a real
/// id it did not mint — the situation a leaked link produces.
#[tokio::test]
async fn another_accounts_project_is_indistinguishable_from_one_that_does_not_exist() {
    let f = fixture().await;
    create_session(&f, &f.a).await;

    let foreign = f.get_at(&f.b, &f.a, "/sessions").await;
    assert_eq!(foreign.status().as_u16(), 404);

    // The same answer for an id nobody has ever had. The two must be
    // indistinguishable, body included — a difference here is the disclosure
    // the 404 was chosen to avoid.
    let invented = f
        .client
        .get(format!("http://{}/api/p/zzzzzzzzzzzz/sessions", f.addr))
        .bearer_auth(&f.b.token)
        .send()
        .await
        .unwrap();
    assert_eq!(invented.status().as_u16(), 404);
    assert_eq!(
        foreign.text().await.unwrap(),
        invented.text().await.unwrap(),
        "a real project and an invented one must answer identically"
    );

    // And nothing was built for either: naming an id must not materialise a
    // supervisor, a dial secret or an orphan sweep.
    assert!(
        !f.projects
            .is_built(&horsie_server::projects::ProjectId::new("zzzzzzzzzzzz")),
        "an id nobody owns must not have built a bundle"
    );

    f.shutdown().await;
}
