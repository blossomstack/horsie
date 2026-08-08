//! Two accounts on one server, and no way for either to reach the other.
//!
//! The runtime-tier twin of `user_isolation.rs`. That one proves the *rows* are
//! separate; this one proves everything above them is — the session list, the
//! live event stream, and the vendor map a session picks its runtime from.
//!
//! This is the file that fails when a handler keeps reaching for something
//! process-global. Like its data-tier twin it is load-bearing rather than
//! belt-and-braces, and for the same reason: no route in this repo creates a
//! second account, so nothing else in CI exercises two of anything.
//!
//! The accounts here are two tokens rather than two `auth_users` rows, because
//! `AuthStore::create_user` enforces the single-account rule this repo ships
//! with — provisioning accounts is left to the deployment. A token *is* the
//! scope: `auth_tokens.principal` is what every request resolves through.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use horsie_server::auth::{Principal, TokenKind, UserId};
use horsie_server::db::Db;
use horsie_server::http::{AppState, app};
use horsie_server::runtime_vendor::fake::FakeRuntimeVendor;
use horsie_server::sessions::supervisor::SupervisorConfig;
use horsie_server::users::{Shared, UserRegistry};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// One account: its id, a bearer token that resolves to it, and a fake vendor
/// agent it has connected — under the same name as everyone else's.
struct Account {
    user: UserId,
    token: String,
    /// Kept alive: dropping it closes the fake vendor's transport.
    _vendor: FakeRuntimeVendor,
}

struct Fixture {
    addr: SocketAddr,
    client: reqwest::Client,
    users: Arc<UserRegistry>,
    a: Account,
    b: Account,
    task: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn shutdown(self) {
        self.task.abort();
    }

    /// The bundle a given account's requests resolve to.
    async fn services(&self, account: &Account) -> Arc<horsie_server::users::UserServices> {
        self.users.get(&account.user).await.unwrap()
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    async fn get(&self, account: &Account, path: &str) -> reqwest::Response {
        self.client
            .get(self.url(path))
            .bearer_auth(&account.token)
            .send()
            .await
            .unwrap()
    }

    async fn post(
        &self,
        account: &Account,
        path: &str,
        body: &serde_json::Value,
    ) -> reqwest::Response {
        self.client
            .post(self.url(path))
            .bearer_auth(&account.token)
            .json(body)
            .send()
            .await
            .unwrap()
    }
}

/// Mint a token for a fresh account id and connect a vendor agent named
/// `runtime` for it. Both accounts use the *same* vendor name on purpose:
/// that collision is the bug this whole change exists to make impossible.
async fn account(users: &UserRegistry, store: &horsie_server::auth::AuthStore) -> Account {
    let user = UserId::generate();
    let token = horsie_server::auth::generate(TokenKind::Agent);
    store
        .insert_token(
            &format!("tok-{}", user.as_str()),
            TokenKind::Agent,
            &Principal::User(user.clone()),
            &token.hash,
            Some("test"),
            None,
            None,
            0,
        )
        .await
        .unwrap();

    let vendor = FakeRuntimeVendor::builder("main")
        .serve_in_process()
        .await
        .expect("fake vendor");
    users
        .get(&user)
        .await
        .unwrap()
        .vendor_agents
        .publish(vendor.link())
        .expect("`main` is unclaimed in every account, not just the first");

    Account {
        user,
        token: token.secret,
        _vendor: vendor,
    }
}

async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap().keep();
    let db = Db::open(&format!("sqlite://{}/config.db", dir.display()), 5)
        .await
        .unwrap();
    let auth = Arc::new(horsie_server::auth::AuthService::new(
        horsie_server::auth::AuthStore::new(db.clone()),
        horsie_server::auth::AuthDeps {
            mode: horsie_server::auth::AuthMode::Password,
            state_dir: dir.clone(),
        },
    ));
    let shared = Arc::new(Shared {
        db: db.clone(),
        artifacts: Arc::new(horsie_server::plugins::ArtifactStore::new(
            dir.join("artifacts"),
        )),
        artifact_secret: Arc::new(b"isolation-secret".to_vec()),
        info: horsie_models::settings::ServerInfo {
            config_path: String::new(),
            database: String::new(),
            state_dir: String::new(),
            data_dir: String::new(),
            plugins_dir: String::new(),
            version: "test".into(),
        },
        model_card_seed: Arc::new(Vec::new()),
        model_card_seed_marker: horsie_server::config::model_cards::seed_marker(&[]),
        anonymous: UserId::bootstrap(),
        supervisor: SupervisorConfig::default(),
    });
    let users = Arc::new(UserRegistry::new(shared.clone()));
    let store = horsie_server::auth::AuthStore::new(db);
    let a = account(&users, &store).await;
    let b = account(&users, &store).await;

    let state = AppState {
        auth,
        shared,
        users: users.clone(),
        web_dir: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app(state)).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    Fixture {
        addr,
        client: reqwest::Client::new(),
        users,
        a,
        b,
        task,
    }
}

/// Create a session on `main` — the vendor name both accounts claimed.
async fn create_session(f: &Fixture, account: &Account) -> String {
    let res = f
        .post(
            account,
            "/api/sessions",
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

    let mine: serde_json::Value = f.get(&f.a, "/api/sessions").await.json().await.unwrap();
    assert_eq!(mine["sessions"].as_array().unwrap().len(), 1);

    let theirs: serde_json::Value = f.get(&f.b, "/api/sessions").await.json().await.unwrap();
    assert!(
        theirs["sessions"].as_array().unwrap().is_empty(),
        "another account's session list must not carry mine: {theirs}"
    );

    // Not merely hidden from the list — unreachable by id, which is the harder
    // half: the id is a UUID the other account could have been told.
    let res = f.get(&f.b, &format!("/api/sessions/{id}")).await;
    assert_eq!(res.status().as_u16(), 404);
    let res = f
        .post(
            &f.b,
            &format!("/api/sessions/{id}/messages"),
            &serde_json::json!({ "text": "hello" }),
        )
        .await;
    assert_eq!(res.status().as_u16(), 404);

    // And deleting is not a way around reading.
    let res = f
        .client
        .delete(f.url(&format!("/api/sessions/{id}")))
        .bearer_auth(&f.b.token)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 404);
    let mine: serde_json::Value = f.get(&f.a, "/api/sessions").await.json().await.unwrap();
    assert_eq!(
        mine["sessions"].as_array().unwrap().len(),
        1,
        "the owner's session survived the other account's delete"
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
    assert_eq!(sa.vendor_agents.connected_names(), vec!["main".to_string()]);
    assert_eq!(sb.vendor_agents.connected_names(), vec!["main".to_string()]);

    // Identity is a websocket-vendor property, so it is read from the typed
    // table the registry publishes rather than from the trait.
    let (a_link, b_link) = (
        sa.vendor_agents
            .links()
            .lock()
            .unwrap()
            .get("main")
            .cloned()
            .unwrap(),
        sb.vendor_agents
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

/// `/api/events` carries session titles and status transitions. One channel per
/// account, so there is no path between them for a filter to have to police.
#[tokio::test]
async fn the_global_event_stream_carries_only_its_own_accounts_frames() {
    use horsie_models::session::{GlobalSessionEvent, GlobalSessionTitleEvent};

    let f = fixture().await;
    let watching_b = f.services(&f.b).await.global_events.subscribe();
    let mut watching_a = f.services(&f.a).await.global_events.subscribe();

    f.services(&f.a)
        .await
        .global_events
        .send(GlobalSessionEvent::TitleChanged(GlobalSessionTitleEvent {
            session_id: "s-1".into(),
            name: "a's private title".into(),
        }))
        .expect("a subscriber is listening");

    match watching_a.try_recv() {
        Ok(GlobalSessionEvent::TitleChanged(e)) => assert_eq!(e.name, "a's private title"),
        other => panic!("the owner must see their own frame, got {other:?}"),
    }
    assert_eq!(
        watching_b.len(),
        0,
        "another account's stream must be empty, not filtered"
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
    let put_config = async |account: &Account, key: &str| {
        let res = f
            .client
            .put(f.url("/api/config/model-providers/p"))
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
            .put(f.url("/api/config/models/shared-alias"))
            .bearer_auth(&account.token)
            .json(&serde_json::json!({
                "alias": "shared-alias", "provider": "p", "modelId": "m"
            }))
            .send()
            .await
            .unwrap()
    };

    assert_eq!(put_config(&f.a, "sk-a").await.status().as_u16(), 200);

    let theirs: serde_json::Value = f.get(&f.b, "/api/config").await.json().await.unwrap();
    assert!(
        theirs["models"].as_array().unwrap().is_empty(),
        "another account's models must not appear: {theirs}"
    );
    assert!(theirs["providers"].as_array().unwrap().is_empty());

    // The same provider name and model alias are still free here — `providers`
    // and `models` are keyed on `(user_id, name)`, not on name alone.
    assert_eq!(put_config(&f.b, "sk-b").await.status().as_u16(), 200);
    for account in [&f.a, &f.b] {
        let view: serde_json::Value = f.get(account, "/api/config").await.json().await.unwrap();
        assert_eq!(view["models"].as_array().unwrap().len(), 1);
        assert_eq!(view["providers"].as_array().unwrap().len(), 1);
    }

    let preset = serde_json::json!({ "name": "reviewer", "model": "shared-alias" });
    assert_eq!(
        f.post(&f.a, "/api/agents", &preset).await.status().as_u16(),
        201
    );
    let theirs: serde_json::Value = f.get(&f.b, "/api/agents").await.json().await.unwrap();
    assert!(
        theirs.as_array().unwrap().is_empty(),
        "another account's presets must not appear: {theirs}"
    );
    assert_eq!(
        f.post(&f.b, "/api/agents", &preset).await.status().as_u16(),
        201,
        "a preset name taken in one account must still be free in another"
    );

    f.shutdown().await;
}
