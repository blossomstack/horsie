//! Two projects, the same names, and no way for either to see the other.
//!
//! This is the load-bearing test of the whole scoping design. It matters more
//! since projects than it did before: an account can now have several, and two
//! projects of *one* account are exactly as separate as two accounts' — the
//! stores below cannot tell the two situations apart, because a project id is
//! all any of them binds.
//!
//! It is also the half the CI scope audit (`db/scope_audit.rs`) cannot do: that
//! one reads the SQL, this one runs it. A statement that says `project_id = ?`
//! and forgets to `.bind()` the value passes the audit and fails here — which
//! has already happened once, in `MemoryStore::list_memories`.
//!
//! Every store that binds a scope gets a case here. The shape is always the
//! same three questions: can the other project *read* it, can it *clobber* it
//! by reusing the name, and can it *delete* it.
//!
//! The journal is the one exception: it binds no scope at all any more, so its
//! case at the end asserts the guarantee that replaced the one it lost rather
//! than a scope it no longer has.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    // `journal_logs_are_separated_by_their_id_alone` reads a `Journal`
    // directly, which is the subject there rather than a violation. Everywhere
    // else the ban stands: only an actor reads its own journal, and only to
    // recover.
    clippy::disallowed_methods
)]

use horsie_server::db::Db;
use horsie_server::db::testing;
use horsie_server::projects::ProjectId;

/// Two projects that are not each other, and are not `'1'` by accident — the
/// id `0040_projects.sql` gives a migrated deployment's first project, which
/// every backfilled row carries.
///
/// No `projects` rows behind them, deliberately. A store binds an id and
/// nothing else, so what is under test here is the binding; whether the id
/// names a real project is [`ProjectRegistry`]'s question and
/// `project_isolation_http.rs`'s.
///
/// [`ProjectRegistry`]: horsie_server::projects::ProjectRegistry
fn two() -> (ProjectId, ProjectId) {
    (ProjectId::generate(), ProjectId::generate())
}

const T: &str = "2026-01-01 00:00:00";

#[tokio::test]
async fn memory_spaces_and_memories_are_isolated() {
    use horsie_server::memory::{MemoryRow, MemorySpaceRow, MemoryStore};
    let db = testing::db().await;
    let (a, b) = two();
    let mine = MemoryStore::new(db.clone(), a);
    let theirs = MemoryStore::new(db, b);

    let space = |desc: &str| MemorySpaceRow {
        name: "notes".into(),
        description: desc.into(),
        created_at: T.into(),
        updated_at: T.into(),
    };
    mine.create_space(&space("mine")).await.unwrap();

    // Read.
    assert!(theirs.get_space("notes").await.unwrap().is_none());
    assert!(theirs.list_spaces().await.unwrap().is_empty());

    // Write: the same name is free for the other project.
    theirs.create_space(&space("theirs")).await.unwrap();
    assert_eq!(
        mine.get_space("notes").await.unwrap().unwrap().description,
        "mine"
    );

    // Memories inside those same-named spaces stay apart too.
    let memory = |content: &str| MemoryRow {
        id: 0,
        space: "notes".into(),
        name: "todo".into(),
        description: String::new(),
        content: content.into(),
        created_at: T.into(),
        updated_at: T.into(),
    };
    mine.create_memory(&memory("mine")).await.unwrap();
    theirs.create_memory(&memory("theirs")).await.unwrap();
    assert_eq!(mine.list_memories(None).await.unwrap().len(), 1);
    assert_eq!(
        mine.get_memory_by_ref("notes", "todo")
            .await
            .unwrap()
            .unwrap()
            .content,
        "mine"
    );

    // Delete does not reach across.
    assert!(theirs.delete_space("notes").await.unwrap());
    assert!(mine.get_space("notes").await.unwrap().is_some());
    assert_eq!(mine.list_memories(None).await.unwrap().len(), 1);
}

#[tokio::test]
async fn model_cards_are_isolated() {
    use horsie_models::model_cards::ModelCardInput;
    use horsie_server::config::model_cards::ModelCardStore;
    let db = testing::db().await;
    let (a, b) = two();
    let mine = ModelCardStore::new(db.clone(), a);
    let theirs = ModelCardStore::new(db, b);

    let card = |name: &str| ModelCardInput {
        model_id: "claude-opus-5".into(),
        name: name.into(),
        context_window: Some(200_000),
        max_tokens: Some(64_000),
        thinking_efforts: None,
        default_thinking_effort: None,
        thinking_dialect: None,
        base_url: None,
        forced_tools_disable_thinking: None,
    };
    mine.insert(&card("mine")).await.unwrap();

    assert!(theirs.get("claude-opus-5").await.unwrap().is_none());
    assert!(theirs.list().await.unwrap().is_empty());
    assert!(theirs.search_by_prefix("claude").await.unwrap().is_empty());

    // The same model id is free for the other project.
    theirs.insert(&card("theirs")).await.unwrap();
    assert_eq!(
        mine.get("claude-opus-5").await.unwrap().unwrap().name,
        "mine"
    );

    theirs.delete("claude-opus-5").await.unwrap();
    assert!(mine.get("claude-opus-5").await.unwrap().is_some());
}

#[tokio::test]
async fn agents_are_isolated() {
    use horsie_server::agents::{AgentRow, AgentStore};
    let db = testing::db().await;
    let (a, b) = two();
    let mine = AgentStore::new(db.clone(), a);
    let theirs = AgentStore::new(db, b);

    let agent = |model: &str| AgentRow {
        name: "reviewer".into(),
        description: String::new(),
        instructions: None,
        model: model.into(),
        plugins: vec![],
        mcp_servers: vec![],
        memory_spaces: vec![],
        thinking_effort: None,
        created_at: T.into(),
        updated_at: T.into(),
        auto_compact: None,
        allowed_tools: None,
    };
    mine.insert(&agent("mine")).await.unwrap();

    assert!(theirs.get("reviewer").await.unwrap().is_none());
    assert!(theirs.list().await.unwrap().is_empty());

    theirs.insert(&agent("theirs")).await.unwrap();
    assert_eq!(mine.get("reviewer").await.unwrap().unwrap().model, "mine");

    // A replace against a name the other project owns matches nothing.
    assert!(theirs.replace(&agent("clobbered")).await.unwrap());
    assert_eq!(mine.get("reviewer").await.unwrap().unwrap().model, "mine");

    assert!(theirs.delete("reviewer").await.unwrap());
    assert!(mine.get("reviewer").await.unwrap().is_some());
}

#[tokio::test]
async fn workflows_are_isolated() {
    use horsie_server::workflows::{WorkflowRow, WorkflowStore};
    let db = testing::db().await;
    let (a, b) = two();
    let mine = WorkflowStore::new(db.clone(), a);
    let theirs = WorkflowStore::new(db, b);

    let wf = |start: &str| WorkflowRow {
        name: "release".into(),
        description: String::new(),
        start: start.into(),
        steps: vec![],
        max_steps: None,
        created_at: T.into(),
        updated_at: T.into(),
    };
    mine.insert(&wf("mine")).await.unwrap();

    assert!(theirs.get("release").await.unwrap().is_none());
    assert!(theirs.list().await.unwrap().is_empty());

    theirs.insert(&wf("theirs")).await.unwrap();
    assert_eq!(mine.get("release").await.unwrap().unwrap().start, "mine");

    assert!(theirs.delete("release").await.unwrap());
    assert!(mine.get("release").await.unwrap().is_some());
}

#[tokio::test]
async fn environments_are_isolated() {
    use horsie_server::environments::{EnvironmentRow, EnvironmentStore};
    let db = testing::db().await;
    let (a, b) = two();
    let mine = EnvironmentStore::new(db.clone(), a);
    let theirs = EnvironmentStore::new(db, b);

    let env = |vendor: &str| EnvironmentRow {
        name: "staging".into(),
        description: String::new(),
        vendor: vendor.into(),
        repos: vec![],
        env_vars: vec![],
        provision: vec![],
        created_at: T.into(),
        updated_at: T.into(),
    };
    mine.insert(&env("mine")).await.unwrap();

    assert!(theirs.get("staging").await.unwrap().is_none());
    assert!(theirs.list().await.unwrap().is_empty());

    theirs.insert(&env("theirs")).await.unwrap();
    assert_eq!(mine.get("staging").await.unwrap().unwrap().vendor, "mine");

    assert!(theirs.delete("staging").await.unwrap());
    assert!(mine.get("staging").await.unwrap().is_some());
}

#[tokio::test]
async fn mcp_servers_are_isolated() {
    use horsie_models::mcp::{McpAuthInput, McpNoAuth, McpServerInput};
    use horsie_server::mcp::McpStore;
    let db = testing::db().await;
    let (a, b) = two();
    let mine = McpStore::new(db.clone(), a);
    let theirs = McpStore::new(db, b);

    let server = |url: &str| McpServerInput {
        name: "jira".into(),
        url: url.into(),
        auth: McpAuthInput::None(McpNoAuth {}),
    };
    mine.upsert(&server("https://mine.example")).await.unwrap();

    assert!(theirs.get("jira").await.unwrap().is_none());
    assert!(theirs.list().await.unwrap().is_empty());

    theirs
        .upsert(&server("https://theirs.example"))
        .await
        .unwrap();
    assert_eq!(
        mine.get("jira").await.unwrap().unwrap().url,
        "https://mine.example"
    );

    // A status write against the other project's name changes nothing.
    theirs
        .set_status("jira", true, Some(7), None)
        .await
        .unwrap();
    assert_eq!(mine.get("jira").await.unwrap().unwrap().tool_count, None);

    theirs.delete("jira").await.unwrap();
    assert!(mine.get("jira").await.unwrap().is_some());
}

#[tokio::test]
async fn github_credentials_are_isolated_but_the_app_is_shared() {
    use horsie_models::github::GitHubAppConfigInput;
    use horsie_server::github::{CredentialsRow, GithubStore};
    let db = testing::db().await;
    let (a, b) = two();
    let mine = GithubStore::new(db.clone(), a);
    let theirs = GithubStore::new(db, b);

    let creds = |login: &str| CredentialsRow {
        login: login.into(),
        access_token: "ghu_token".to_string().into(),
        refresh_token: None,
        expires_at: None,
        installation_id: Some(42),
    };
    mine.save_credentials(&creds("mine")).await.unwrap();

    assert!(theirs.credentials().await.unwrap().is_none());
    theirs.save_credentials(&creds("theirs")).await.unwrap();
    assert_eq!(mine.credentials().await.unwrap().unwrap().login, "mine");

    theirs.clear_credentials().await.unwrap();
    assert!(mine.credentials().await.unwrap().is_some());

    // The App registration is deployment config, so it is deliberately shared:
    // one callback URL, one client id, bound to this server's public address.
    mine.save_app_config(&GitHubAppConfigInput {
        client_id: "Iv1.deployment".into(),
        client_secret: None,
        app_id: Some(1),
        private_key: None,
        app_slug: None,
        callback_base: None,
    })
    .await
    .unwrap();
    assert_eq!(
        theirs.app_config().await.unwrap().unwrap().client_id,
        "Iv1.deployment",
        "the App registration is the deployment's, not an project's"
    );
}

#[tokio::test]
async fn marketplaces_are_isolated() {
    use horsie_server::plugins::{MarketplaceRow, MarketplaceStore};
    let db = testing::db().await;
    let (a, b) = two();
    let mine = MarketplaceStore::new(db.clone(), a);
    let theirs = MarketplaceStore::new(db, b);

    let market = |url: &str| MarketplaceRow {
        name: "official".into(),
        source_url: url.into(),
        source_ref: None,
        sha: None,
        entries: vec![],
        skipped: vec![],
        created_at: T.into(),
        updated_at: T.into(),
    };
    mine.upsert(&market("https://mine.example")).await.unwrap();

    assert!(theirs.get("official").await.unwrap().is_none());
    assert!(theirs.list().await.unwrap().is_empty());

    theirs
        .upsert(&market("https://theirs.example"))
        .await
        .unwrap();
    assert_eq!(
        mine.get("official").await.unwrap().unwrap().source_url,
        "https://mine.example"
    );

    theirs.delete("official").await.unwrap();
    assert!(mine.get("official").await.unwrap().is_some());
}

#[tokio::test]
async fn plugin_bundles_are_isolated() {
    let db = testing::db().await;
    let (a, b) = two();
    let mine = horsie_server::plugins::PluginStore::new(db.clone(), a);
    let theirs = horsie_server::plugins::PluginStore::new(db, b);

    mine.upsert(&plugin("impeccable", "hash-a")).await.unwrap();

    assert!(theirs.get("impeccable").await.unwrap().is_none());
    assert!(theirs.list().await.unwrap().is_empty());

    theirs
        .upsert(&plugin("impeccable", "hash-b"))
        .await
        .unwrap();
    assert_eq!(
        mine.get("impeccable").await.unwrap().unwrap().digest,
        "hash-a"
    );

    theirs.delete("impeccable").await.unwrap();
    assert!(mine.get("impeccable").await.unwrap().is_some());
}

/// The two reads that are deliberately unscoped, asserted to *stay* that way.
///
/// Both destroy data if "fixed": scoping the GC keep-set deletes bundle bytes
/// another project still references, and scoping the scheduler's read stops
/// every other project's routines from ever firing.
#[tokio::test]
async fn the_deliberate_exceptions_still_cross_projects() {
    use horsie_server::plugins::PluginStore;
    use horsie_server::routines::RoutineStore;
    let db = testing::db().await;
    let (a, b) = two();

    let mine = PluginStore::new(db.clone(), a.clone());
    let theirs = PluginStore::new(db.clone(), b.clone());
    mine.upsert(&plugin("one", "hash-a")).await.unwrap();
    theirs.upsert(&plugin("two", "hash-b")).await.unwrap();
    let keep = mine.referenced_hashes().await.unwrap();
    assert!(
        keep.contains("hash-a") && keep.contains("hash-b"),
        "artifact GC must see every project's hashes: {keep:?}"
    );

    let my_routines = RoutineStore::new(db.clone(), a.clone());
    let their_routines = RoutineStore::new(db.clone(), b.clone());
    my_routines.insert(&routine("nightly")).await.unwrap();
    their_routines.insert(&routine("nightly")).await.unwrap();

    let due = RoutineStore::due_across_all_users(&db, 1_000)
        .await
        .unwrap();
    let owners: Vec<&str> = due.iter().map(|(u, _)| u.as_str()).collect();
    assert_eq!(due.len(), 2, "one timer serves every project: {owners:?}");
    assert!(owners.contains(&a.as_str()) && owners.contains(&b.as_str()));

    // The scoped read still sees only its own.
    assert_eq!(my_routines.list().await.unwrap().len(), 1);
}

/// The journal is the one table here that gave its `project_id` up, and what
/// separates two projects' logs now is the id itself.
///
/// A `Journal` method receives a `PersistenceId` and nothing else, and that id
/// is fixed when its actor is constructed — before a byte of its history has
/// been read — so an project could only reach the journal by being packed into
/// the id. What the column bought was that another project's log stayed
/// unreachable even given its id; that is now bought by the id being a uuid
/// nothing hands out. Deleting an project's data walks its supervisor's session
/// list and clears each log, so what has to hold is that clearing one log
/// touches nothing else, not that a filter caught it.
#[tokio::test]
async fn journal_logs_are_separated_by_their_id_alone() {
    use futures_util::StreamExt;
    use horsie_actor::{Journal, PersistenceId};
    use horsie_server::db::journal::SqlJournal;
    let db: Db = testing::db().await;
    let journal = SqlJournal::new(db);
    // Two sessions of two projects: distinct because a session id is a uuid,
    // which is the whole of the protection.
    let mine = PersistenceId::new("session", "6f1e3f1c-0d2a-4a4e-9d1b-2f8c5a7e0001");
    let theirs = PersistenceId::new("session", "b3d9c0a2-77f4-4f6d-8c31-19ae4b620002");

    journal
        .persist(&mine, &[b"mine".to_vec()], 0)
        .await
        .unwrap();
    journal
        .persist(&theirs, &[b"theirs".to_vec()], 0)
        .await
        .unwrap();

    async fn read(j: &SqlJournal, pid: &PersistenceId) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut s = j.replay(pid, 0).await;
        while let Some(item) = s.next().await {
            out.push(item.unwrap().1);
        }
        out
    }
    assert_eq!(read(&journal, &mine).await, vec![b"mine".to_vec()]);
    assert_eq!(read(&journal, &theirs).await, vec![b"theirs".to_vec()]);

    // The project-deletion path: clearing one log leaves every other one whole.
    journal.clear(&theirs).await.unwrap();
    assert_eq!(read(&journal, &mine).await, vec![b"mine".to_vec()]);
    assert_eq!(journal.last_seq(&mine).await.unwrap(), 1);
}

fn plugin(name: &str, hash: &str) -> horsie_server::plugins::PluginRow {
    horsie_server::plugins::PluginRow {
        name: name.into(),
        kind: horsie_models::plugins::PluginKind::Claude(
            horsie_models::plugins::ExternalOrigin {
                url: "https://example.com/x.git".into(),
                git_ref: None,
                subpath: None,
                marketplace: None,
                marketplace_entry: None,
            },
        ),
        version: None,
        description: None,
        catalog: Vec::new(),
        has_hooks: false,
        digest: hash.into(),
        artifact_size: 1,
        enabled_default: false,
        created_at: T.into(),
        updated_at: T.into(),
    }
}

fn routine(name: &str) -> horsie_server::routines::RoutineRow {
    horsie_server::routines::RoutineRow {
        environment: horsie_models::environments::EnvironmentSpec::Runtime(
            horsie_models::environments::RuntimeEnvironment {
                vendor: "mock".into(),
                repos: None,
            },
        ),
        name: name.into(),
        description: String::new(),
        agent: "reviewer".into(),
        prompt: "go".into(),
        schedule: horsie_models::routines::RoutineSchedule::Manual(
            horsie_models::routines::ManualSchedule {},
        ),
        enabled: true,
        next_run_at_ms: Some(100),
        last_run_at_ms: None,
        last_session_id: None,
        last_error: None,
        created_at: T.into(),
        updated_at: T.into(),
    }
}
