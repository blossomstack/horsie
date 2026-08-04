# Session Annotations & Groups Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Sessions carry key-value annotations; a `group` annotation drives grouped, reorderable sidebar sections backed by a groups CRUD API.

**Architecture:** Annotations live on `SessionRecord` and groups in `SessionSupervisorState` (both journaled by the event-sourced `SessionSupervisor` — no DB tables). New supervisor events/commands implement group CRUD with rename/delete fixups applied in the event fold. HTTP exposes flat endpoints; the React sidebar unions the groups API with annotation values, renders collapsible sections (Ungrouped is frontend-only), and persists section order in localStorage.

**Tech Stack:** Rust (axum, sqlx, event-sourced `horsie-actor`), fluorite schemas → Rust + TS codegen, React 19 + react-query + Tailwind 4, vitest, Playwright.

**Spec:** `docs/superpowers/specs/2026-08-04-session-annotations-groups-design.md`

## Global Constraints

- Work happens in the worktree `.horsie/worktrees/session-groups` on branch `feat/session-annotations-groups`.
- Rust tests: `cargo test --workspace` (single-crate `-p` fails on feature gating). For server-only iteration: `cargo test -p horsie-server --features test-util`.
- `cargo fmt` with the **stable** toolchain only, never nightly.
- Production Rust denies `unwrap_used`, `expect_used`, `panic`; test modules opt out with `#![cfg_attr(test, allow(...))]` (the existing `#![allow(...)]` attribute block above `mod tests`).
- Pre-PR: `cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt --check`, `cargo test --workspace` must pass.
- Wire types come only from fluorite `.fl` schemas; after editing them, regenerate TS with `cd clients/web && bun run generate-types`.
- No new npm dependencies (no DnD or menu libraries).
- Conventional commit messages.

---

### Task 1: Fluorite wire types (annotations + groups)

**Files:**
- Modify: `models/fluorite/session.fl` (`SessionSummary`, `SessionDetail`, new `AnnotationEntry`)
- Modify: `models/fluorite/session_api.fl` (group request/response types, `SetAnnotationsRequest`)
- Regenerate: `clients/web/src/generated/*` (via `bun run generate-types`)

**Interfaces:**
- Produces: Rust `horsie_models::session::AnnotationEntry { key: String, value: String }`; `SessionSummary.annotations: Vec<AnnotationEntry>`; `SessionDetail.annotations: Vec<AnnotationEntry>`; `horsie_models::session_api::{SessionGroupView { name }, CreateGroupRequest { name }, RenameGroupRequest { name }, CreateGroupResponse { group }, ListGroupsResponse { groups }, SetAnnotationsRequest { set: Vec<AnnotationEntry>, remove: Vec<String> } }` and their TS equivalents (camelCase fields).

- [ ] **Step 1: Add `AnnotationEntry` and `annotations` to `session.fl`**

In `models/fluorite/session.fl`, add after the `SessionStatusKind` enum:

```text
/// One key-value annotation on a session. Annotations ride as a vec of
/// entries (fluorite has no map type); keys are unique per session.
struct AnnotationEntry {
    key: String,
    value: String,
}
```

Add to `SessionSummary`, after `last_error`:

```text
    /// User-set key-value metadata (e.g. `group=<name>`). Empty when none.
    annotations: Vec<AnnotationEntry>,
```

Add the same field to `SessionDetail`, after `last_error`.

- [ ] **Step 2: Add group types to `session_api.fl`**

Add `use session.AnnotationEntry;` to the use block, and append:

```text
/// A registered session group. Groups may exist with zero sessions; a group
/// referenced only by annotations is not registered but still lists.
struct SessionGroupView { name: String }

struct CreateGroupRequest { name: String }
struct CreateGroupResponse { group: SessionGroupView }
/// Rename a group; sessions annotated with the old name follow.
struct RenameGroupRequest { name: String }
struct ListGroupsResponse { groups: Vec<SessionGroupView> }

/// Merge-update a session's annotations: every `set` entry upserts a key,
/// every `remove` entry drops one. Keys not mentioned are untouched.
struct SetAnnotationsRequest {
    set: Vec<AnnotationEntry>,
    remove: Vec<String>,
}
```

- [ ] **Step 3: Verify the Rust codegen compiles**

Run: `cargo build -p horsie-models`
Expected: PASS (build.rs runs fluorite_codegen; new types exist).

- [ ] **Step 4: Regenerate the TS types**

Run: `cd clients/web && bun run generate-types`
Expected: `src/generated/session.ts` gains `annotations: AnnotationEntry[]` on both summary and detail; `session_api.ts` gains the group types.

- [ ] **Step 5: Commit**

```bash
git add models/fluorite/session.fl models/fluorite/session_api.fl clients/web/src/generated
git commit -m "feat(models): session annotations and group wire types"
```

---

### Task 2: Supervisor state, events, and folds

**Files:**
- Modify: `server/src/sessions/supervisor.rs` (`SessionRecord`, `GroupRecord`, `SessionSupervisorState`, `SessionSupervisorEvent`, `apply_event`)
- Test: same file, `mod tests`

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces: `SessionRecord.annotations: BTreeMap<String, String>`; `SessionSupervisorState.groups: BTreeMap<String, GroupRecord>`; `GroupRecord { created_at: u64 }`; events `GroupCreated { name, created_at }`, `GroupRenamed { old, new }`, `GroupDeleted { name }`, `SessionAnnotationsSet { id, set: BTreeMap<String,String>, remove: Vec<String> }` — Task 3's commands persist these; the fold rewrites/strips `group` annotations on rename/delete.

- [ ] **Step 1: Write the failing fold tests**

Add to `mod tests` in `supervisor.rs`:

```rust
    fn created_session(s: SessionSupervisorState, id: &str) -> SessionSupervisorState {
        SessionSupervisor::apply_event(
            s,
            SessionSupervisorEvent::SessionCreated {
                id: id.into(),
                spec: spec_fixture(),
                created_at: 1,
            },
        )
    }

    #[test]
    fn annotations_set_and_removed_fold() {
        let s = created_session(SessionSupervisorState::default(), "s1");
        let s = SessionSupervisor::apply_event(
            s,
            SessionSupervisorEvent::SessionAnnotationsSet {
                id: "s1".into(),
                set: BTreeMap::from([("group".to_string(), "web".to_string())]),
                remove: vec![],
            },
        );
        assert_eq!(
            s.sessions.get("s1").unwrap().annotations.get("group"),
            Some(&"web".to_string())
        );
        let s = SessionSupervisor::apply_event(
            s,
            SessionSupervisorEvent::SessionAnnotationsSet {
                id: "s1".into(),
                set: BTreeMap::new(),
                remove: vec!["group".to_string()],
            },
        );
        assert!(s.sessions.get("s1").unwrap().annotations.is_empty());
    }

    #[test]
    fn group_rename_rewrites_annotations() {
        let s = created_session(SessionSupervisorState::default(), "s1");
        let s = SessionSupervisor::apply_event(
            s,
            SessionSupervisorEvent::GroupCreated { name: "web".into(), created_at: 1 },
        );
        let s = SessionSupervisor::apply_event(
            s,
            SessionSupervisorEvent::SessionAnnotationsSet {
                id: "s1".into(),
                set: BTreeMap::from([("group".to_string(), "web".to_string())]),
                remove: vec![],
            },
        );
        let s = SessionSupervisor::apply_event(
            s,
            SessionSupervisorEvent::GroupRenamed { old: "web".into(), new: "frontend".into() },
        );
        assert!(s.groups.contains_key("frontend"));
        assert!(!s.groups.contains_key("web"));
        assert_eq!(
            s.sessions.get("s1").unwrap().annotations.get("group"),
            Some(&"frontend".to_string())
        );
    }

    #[test]
    fn group_delete_strips_annotations() {
        let s = created_session(SessionSupervisorState::default(), "s1");
        let s = SessionSupervisor::apply_event(
            s,
            SessionSupervisorEvent::GroupCreated { name: "web".into(), created_at: 1 },
        );
        let s = SessionSupervisor::apply_event(
            s,
            SessionSupervisorEvent::SessionAnnotationsSet {
                id: "s1".into(),
                set: BTreeMap::from([("group".to_string(), "web".to_string())]),
                remove: vec![],
            },
        );
        let s = SessionSupervisor::apply_event(
            s,
            SessionSupervisorEvent::GroupDeleted { name: "web".into() },
        );
        assert!(s.groups.is_empty());
        assert!(s.sessions.get("s1").unwrap().annotations.is_empty());
    }

    #[test]
    fn session_delete_drops_its_annotations() {
        let s = created_session(SessionSupervisorState::default(), "s1");
        let s = SessionSupervisor::apply_event(
            s,
            SessionSupervisorEvent::SessionAnnotationsSet {
                id: "s1".into(),
                set: BTreeMap::from([("group".to_string(), "web".to_string())]),
                remove: vec![],
            },
        );
        let s = SessionSupervisor::apply_event(
            s,
            SessionSupervisorEvent::SessionDeleted { id: "s1".into() },
        );
        assert!(s.sessions.is_empty());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p horsie-server --features test-util sessions::supervisor`
Expected: FAIL — `SessionAnnotationsSet`, `GroupCreated`, etc. do not exist.

- [ ] **Step 3: Implement the state, events, and folds**

In `supervisor.rs`:

Add `annotations` to `SessionRecord`:

```rust
/// One registry row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub spec: SessionSpec,
    pub created_at: u64,
    /// User-set key-value metadata (group, future provenance keys). Field-level
    /// default so pre-annotations journal rows load with an empty map.
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
}
```

Add `GroupRecord` and the `groups` field:

```rust
/// One registered group. Registration is optional metadata: a group can exist
/// with zero sessions, and an annotation can name a group that was never
/// registered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupRecord {
    pub created_at: u64,
}
```

```rust
pub struct SessionSupervisorState {
    pub sessions: BTreeMap<SessionId, SessionRecord>,
    /// Registered groups, name-keyed.
    #[serde(default)]
    pub groups: BTreeMap<String, GroupRecord>,
}
```

Add to `SessionSupervisorEvent`:

```rust
    GroupCreated {
        name: String,
        created_at: u64,
    },
    /// Renames the registry key and rewrites `group=<old>` annotations; both
    /// ride one event so the fixup is atomic with the rename.
    GroupRenamed {
        old: String,
        new: String,
    },
    /// Removes the registry key and strips `group=<name>` annotations.
    GroupDeleted {
        name: String,
    },
    /// Merge-update of one session's annotations: `set` upserts, `remove` drops.
    SessionAnnotationsSet {
        id: SessionId,
        set: BTreeMap<String, String>,
        remove: Vec<String>,
    },
```

Extend `apply_event` — update the `SessionCreated` arm to include `annotations: BTreeMap::new()` in the constructed `SessionRecord`, and add:

```rust
            SessionSupervisorEvent::GroupCreated { name, created_at } => {
                state.groups.insert(name, GroupRecord { created_at });
            }
            SessionSupervisorEvent::GroupRenamed { old, new } => {
                if let Some(rec) = state.groups.remove(&old) {
                    state.groups.insert(new.clone(), rec);
                }
                for rec in state.sessions.values_mut() {
                    if rec.annotations.get("group") == Some(&old) {
                        rec.annotations.insert("group".to_string(), new.clone());
                    }
                }
            }
            SessionSupervisorEvent::GroupDeleted { name } => {
                state.groups.remove(&name);
                for rec in state.sessions.values_mut() {
                    if rec.annotations.get("group") == Some(&name) {
                        rec.annotations.remove("group");
                    }
                }
            }
            SessionSupervisorEvent::SessionAnnotationsSet { id, set, remove } => {
                if let Some(rec) = state.sessions.get_mut(&id) {
                    for key in &remove {
                        rec.annotations.remove(key);
                    }
                    rec.annotations.extend(set);
                }
            }
```

Any other construction site of `SessionRecord` (e.g. handlers.rs `create_session` builds `SessionRecord { spec, created_at }`) gains `annotations: BTreeMap::new()` — but do **not** touch handlers.rs yet; note the compile error and fix it in Task 4. To keep this task compiling, `create_session` in handlers.rs must be updated now: `SessionRecord { spec, created_at, annotations: BTreeMap::new() }` with `use std::collections::BTreeMap;` if missing. (The `summary()` annotation merge is still Task 4.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p horsie-server --features test-util sessions::supervisor`
Expected: PASS (including the pre-existing fold test).

- [ ] **Step 5: Commit**

```bash
git add server/src/sessions/supervisor.rs server/src/http/handlers.rs
git commit -m "feat(server): journal events and folds for groups and annotations"
```

---

### Task 3: Supervisor commands (group CRUD + annotation set)

**Files:**
- Modify: `server/src/sessions/supervisor.rs` (`SessionSupervisorCommand`, `GroupError`, `handle_command`)
- Test: same file, `mod tests`

**Interfaces:**
- Consumes: Task 2's events/state.
- Produces: `GroupError { NotFound(String), NameTaken(String), Invalid(String) }` (implements `Display` + `std::error::Error`); commands `CreateGroup { name, created_at, reply: oneshot::Sender<Result<(), GroupError>> }`, `RenameGroup { old, new, reply }` (same reply type), `DeleteGroup { name, reply }` (same), `ListGroups { reply: oneshot::Sender<Vec<(String, GroupRecord)>> }`, `SetSessionAnnotations { id, set: BTreeMap<String,String>, remove: Vec<String>, reply: oneshot::Sender<Result<(), String>> }`. Task 4 maps `GroupError` to HTTP.

- [ ] **Step 1: Write the failing command tests**

Add to `mod tests` (these use the async `fixture()` + `spawn_root` pattern already in the file; `sup` is built as in `boot_loads_nothing`):

```rust
    async fn spawn_supervisor(f: &Fixture) -> ActorRef<SessionSupervisorCommand> {
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let clock: Arc<TestClock> = Arc::new(TestClock::new());
        let (gtx, _) = broadcast::channel(16);
        spawn_root(
            SessionSupervisor::with_config(f.deps.clone(), gtx, manual_config(&clock)),
            journal,
        )
    }

    #[tokio::test]
    async fn group_create_list_and_duplicate_conflict() {
        let f = fixture().await;
        let sup = spawn_supervisor(&f).await;

        sup.ask(|reply| SessionSupervisorCommand::CreateGroup {
            name: "web".into(),
            created_at: 1,
            reply,
        })
        .await
        .unwrap()
        .unwrap();

        let dup = sup
            .ask(|reply| SessionSupervisorCommand::CreateGroup {
                name: "web".into(),
                created_at: 2,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(dup, Err(GroupError::NameTaken("web".into())));

        let groups = sup
            .ask(|reply| SessionSupervisorCommand::ListGroups { reply })
            .await
            .unwrap();
        assert_eq!(groups, vec![("web".to_string(), GroupRecord { created_at: 1 })]);
    }

    #[tokio::test]
    async fn group_validation_rejects_empty_and_overlong_names() {
        let f = fixture().await;
        let sup = spawn_supervisor(&f).await;
        for bad in ["", &"x".repeat(129)] {
            let err = sup
                .ask(|reply| SessionSupervisorCommand::CreateGroup {
                    name: bad.to_string(),
                    created_at: 1,
                    reply,
                })
                .await
                .unwrap();
            assert!(matches!(err, Err(GroupError::Invalid(_))));
        }
    }

    #[tokio::test]
    async fn rename_unregistered_group_rewrites_annotations() {
        let f = fixture().await;
        let sup = spawn_supervisor(&f).await;
        let id = create(&sup).await;
        sup.ask(|reply| SessionSupervisorCommand::SetSessionAnnotations {
            id: id.clone(),
            set: BTreeMap::from([("group".to_string(), "web".to_string())]),
            remove: vec![],
            reply,
        })
        .await
        .unwrap()
        .unwrap();

        // "web" was never registered; the rename still fixes the annotation.
        sup.ask(|reply| SessionSupervisorCommand::RenameGroup {
            old: "web".into(),
            new: "frontend".into(),
            reply,
        })
        .await
        .unwrap()
        .unwrap();

        let sessions = sup
            .ask(|reply| SessionSupervisorCommand::List { reply })
            .await
            .unwrap();
        assert_eq!(
            sessions[0].1.annotations.get("group"),
            Some(&"frontend".to_string())
        );
        assert!(sup
            .ask(|reply| SessionSupervisorCommand::ListGroups { reply })
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn unknown_group_rename_and_delete_are_not_found() {
        let f = fixture().await;
        let sup = spawn_supervisor(&f).await;
        let err = sup
            .ask(|reply| SessionSupervisorCommand::RenameGroup {
                old: "nope".into(),
                new: "x".into(),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(err, Err(GroupError::NotFound("nope".into())));
        let err = sup
            .ask(|reply| SessionSupervisorCommand::DeleteGroup { name: "nope".into(), reply })
            .await
            .unwrap();
        assert_eq!(err, Err(GroupError::NotFound("nope".into())));
    }

    #[tokio::test]
    async fn set_annotations_on_unknown_session_errors() {
        let f = fixture().await;
        let sup = spawn_supervisor(&f).await;
        let err = sup
            .ask(|reply| SessionSupervisorCommand::SetSessionAnnotations {
                id: "nope".into(),
                set: BTreeMap::new(),
                remove: vec![],
                reply,
            })
            .await
            .unwrap();
        assert!(err.is_err());
    }
```

Note: `create(&sup)` uses the existing helper, which provisions a mock runtime — fine. `BTreeMap` must be imported in the tests module (`use std::collections::BTreeMap;` — the file already imports `BTreeMap` at top; check).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p horsie-server --features test-util sessions::supervisor`
Expected: FAIL — commands don't exist.

- [ ] **Step 3: Implement `GroupError`, commands, and handlers**

After `SessionSupervisorEvent`:

```rust
/// Why a group command was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupError {
    /// Neither registered nor referenced by any session annotation.
    NotFound(String),
    /// The name is already taken (create, or rename target).
    NameTaken(String),
    /// Empty or over-long.
    Invalid(String),
}

impl std::fmt::Display for GroupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GroupError::NotFound(name) => write!(f, "no such group: {name}"),
            GroupError::NameTaken(name) => write!(f, "group already exists: {name}"),
            GroupError::Invalid(reason) => write!(f, "invalid group name: {reason}"),
        }
    }
}

impl std::error::Error for GroupError {}

const GROUP_NAME_MAX_LEN: usize = 128;

fn validate_group_name(name: &str) -> Result<(), GroupError> {
    if name.is_empty() {
        return Err(GroupError::Invalid("empty".into()));
    }
    if name.len() > GROUP_NAME_MAX_LEN {
        return Err(GroupError::Invalid(format!(
            "longer than {GROUP_NAME_MAX_LEN} characters"
        )));
    }
    Ok(())
}

/// Whether the group is registered or any session carries `group=<name>`.
fn group_exists(state: &SessionSupervisorState, name: &str) -> bool {
    state.groups.contains_key(name)
        || state
            .sessions
            .values()
            .any(|rec| rec.annotations.get("group").is_some_and(|g| g == name))
}
```

Add to `SessionSupervisorCommand`:

```rust
    /// Register a group. `created_at` is unix epoch millis (caller-supplied for
    /// deterministic tests, like `Create`).
    CreateGroup {
        name: String,
        created_at: u64,
        reply: oneshot::Sender<Result<(), GroupError>>,
    },
    /// Rename a registered *or annotation-only* group; sessions follow.
    RenameGroup {
        old: String,
        new: String,
        reply: oneshot::Sender<Result<(), GroupError>>,
    },
    /// Delete a group and strip its annotation from every session.
    DeleteGroup {
        name: String,
        reply: oneshot::Sender<Result<(), GroupError>>,
    },
    /// The group registry, name-sorted.
    ListGroups {
        reply: oneshot::Sender<Vec<(String, GroupRecord)>>,
    },
    /// Merge-update one session's annotations. Err when the session is unknown.
    SetSessionAnnotations {
        id: SessionId,
        set: BTreeMap<String, String>,
        remove: Vec<String>,
        reply: oneshot::Sender<Result<(), String>>,
    },
```

Add arms in `handle_command` (before the closing of the match; follow the `Delete` arm's reply-then-persist pattern):

```rust
            SessionSupervisorCommand::CreateGroup { name, created_at, reply } => {
                if let Err(e) = validate_group_name(&name) {
                    let _ = reply.send(Err(e));
                    return CommandEffect::none();
                }
                if state.groups.contains_key(&name) {
                    let _ = reply.send(Err(GroupError::NameTaken(name)));
                    return CommandEffect::none();
                }
                let _ = reply.send(Ok(()));
                CommandEffect::persist(vec![SessionSupervisorEvent::GroupCreated { name, created_at }])
            }
            SessionSupervisorCommand::RenameGroup { old, new, reply } => {
                if let Err(e) = validate_group_name(&new) {
                    let _ = reply.send(Err(e));
                    return CommandEffect::none();
                }
                if state.groups.contains_key(&new) {
                    let _ = reply.send(Err(GroupError::NameTaken(new)));
                    return CommandEffect::none();
                }
                if !group_exists(state, &old) {
                    let _ = reply.send(Err(GroupError::NotFound(old)));
                    return CommandEffect::none();
                }
                let _ = reply.send(Ok(()));
                CommandEffect::persist(vec![SessionSupervisorEvent::GroupRenamed { old, new }])
            }
            SessionSupervisorCommand::DeleteGroup { name, reply } => {
                if !group_exists(state, &name) {
                    let _ = reply.send(Err(GroupError::NotFound(name)));
                    return CommandEffect::none();
                }
                let _ = reply.send(Ok(()));
                CommandEffect::persist(vec![SessionSupervisorEvent::GroupDeleted { name }])
            }
            SessionSupervisorCommand::ListGroups { reply } => {
                let _ = reply.send(
                    state
                        .groups
                        .iter()
                        .map(|(name, rec)| (name.clone(), rec.clone()))
                        .collect(),
                );
                CommandEffect::none()
            }
            SessionSupervisorCommand::SetSessionAnnotations { id, set, remove, reply } => {
                if !state.sessions.contains_key(&id) {
                    let _ = reply.send(Err(format!("no such session: {id}")));
                    return CommandEffect::none();
                }
                let _ = reply.send(Ok(()));
                CommandEffect::persist(vec![SessionSupervisorEvent::SessionAnnotationsSet { id, set, remove }])
            }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p horsie-server --features test-util sessions::supervisor`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add server/src/sessions/supervisor.rs
git commit -m "feat(server): supervisor commands for group CRUD and annotations"
```

---

### Task 4: HTTP layer — routes, handlers, annotation merge

**Files:**
- Create: `server/src/http/groups.rs`
- Modify: `server/src/http/mod.rs` (register `mod groups;`, routes, tests)
- Modify: `server/src/http/handlers.rs` (`summary()` and `get_session` merge annotations)
- Test: `server/src/http/mod.rs` `mod tests`

**Interfaces:**
- Consumes: Task 1 wire types, Task 3 commands/`GroupError`, existing `ask()` helper and `Api` error type.
- Produces: routes `GET|POST /api/session-groups`, `PUT|DELETE /api/session-groups/:name`, `PUT /api/sessions/:id/annotations`; `SessionSummary.annotations` populated for Tasks 5+.

- [ ] **Step 1: Write the failing HTTP tests**

Add to `mod tests` in `server/src/http/mod.rs` (helpers `get`, `delete`, and a JSON-posting pattern already exist; add a `put_json` helper next to them if absent):

```rust
    fn put_json(uri: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .method("PUT")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn post_json(uri: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    async fn body_json(res: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Create a session through the API and return its id.
    async fn create_session_via_api(app: &Router) -> String {
        let res = app
            .clone()
            .oneshot(post_json(
                "/api/sessions",
                serde_json::json!({ "agent": { "model": "mock" } }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        body_json(res).await["session"]["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn group_crud_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp).await;
        let app = app(state);

        // Create → 201, listed.
        let res = app
            .clone()
            .oneshot(post_json("/api/session-groups", serde_json::json!({ "name": "web" })))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);

        let res = app.clone().oneshot(get("/api/session-groups")).await.unwrap();
        let body = body_json(res).await;
        assert_eq!(body["groups"][0]["name"], "web");

        // Duplicate → 409.
        let res = app
            .clone()
            .oneshot(post_json("/api/session-groups", serde_json::json!({ "name": "web" })))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);

        // Rename → 200, new name listed.
        let res = app
            .clone()
            .oneshot(put_json(
                "/api/session-groups/web",
                serde_json::json!({ "name": "frontend" }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Delete → 200, gone; deleting again → 404.
        let res = app
            .clone()
            .oneshot(delete("/api/session-groups/frontend"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res = app
            .clone()
            .oneshot(delete("/api/session-groups/frontend"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn annotations_ride_the_session_list_and_follow_group_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp).await;
        let app = app(state);
        let id = create_session_via_api(&app).await;

        // Assign a group; the list carries it.
        let res = app
            .clone()
            .oneshot(put_json(
                &format!("/api/sessions/{id}/annotations"),
                serde_json::json!({ "set": [{ "key": "group", "value": "web" }], "remove": [] }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let res = app.clone().oneshot(get("/api/sessions")).await.unwrap();
        let body = body_json(res).await;
        assert_eq!(
            body["sessions"][0]["annotations"],
            serde_json::json!([{ "key": "group", "value": "web" }])
        );

        // Rename the (unregistered) group; the annotation follows.
        let res = app
            .clone()
            .oneshot(put_json(
                "/api/session-groups/web",
                serde_json::json!({ "name": "frontend" }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res = app.clone().oneshot(get("/api/sessions")).await.unwrap();
        let body = body_json(res).await;
        assert_eq!(
            body["sessions"][0]["annotations"],
            serde_json::json!([{ "key": "group", "value": "frontend" }])
        );

        // Delete strips it; the session detail agrees.
        let res = app
            .clone()
            .oneshot(delete("/api/session-groups/frontend"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res = app
            .clone()
            .oneshot(get(&format!("/api/sessions/{id}")))
            .await
            .unwrap();
        let body = body_json(res).await;
        assert_eq!(body["session"]["annotations"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn annotations_on_unknown_session_are_404() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp).await;
        let app = app(state);
        let res = app
            .clone()
            .oneshot(put_json(
                "/api/sessions/nope/annotations",
                serde_json::json!({ "set": [], "remove": ["group"] }),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }
```

(Check the tests module for an existing `post`/`post_json` helper first — reuse it if present rather than adding a duplicate.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p horsie-server --features test-util http::tests::group http::tests::annotations`
Expected: FAIL — routes 404.

- [ ] **Step 3: Implement `server/src/http/groups.rs`**

```rust
//! Session groups and session annotations. Both are supervisor-journal state,
//! so every handler is a thin ask-and-map over `SessionSupervisorCommand`.

use crate::http::AppState;
use crate::http::error::Api;
use crate::http::handlers::ask;
use crate::sessions::supervisor::{GroupError, SessionSupervisorCommand};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use horsie_models::now_ms;
use horsie_models::session::AnnotationEntry;
use horsie_models::session_api::{
    Ack, CreateGroupRequest, CreateGroupResponse, ListGroupsResponse, RenameGroupRequest,
    SessionGroupView, SetAnnotationsRequest,
};
use std::collections::BTreeMap;

fn group_error(e: GroupError) -> Api {
    match e {
        GroupError::NotFound(m) => Api::not_found(m),
        GroupError::NameTaken(m) => Api::conflict("name_taken", m),
        GroupError::Invalid(m) => Api::unprocessable(m),
    }
}

pub async fn list_groups(State(state): State<AppState>) -> Result<impl IntoResponse, Api> {
    let groups = ask(&state, |reply| SessionSupervisorCommand::ListGroups { reply }).await?;
    let groups = groups
        .into_iter()
        .map(|(name, _)| SessionGroupView { name })
        .collect();
    Ok(Json(ListGroupsResponse { groups }))
}

pub async fn create_group(
    State(state): State<AppState>,
    Json(req): Json<CreateGroupRequest>,
) -> Result<impl IntoResponse, Api> {
    ask(&state, |reply| SessionSupervisorCommand::CreateGroup {
        name: req.name.clone(),
        created_at: now_ms(),
        reply,
    })
    .await?
    .map_err(group_error)?;
    Ok((
        StatusCode::CREATED,
        Json(CreateGroupResponse {
            group: SessionGroupView { name: req.name },
        }),
    ))
}

pub async fn rename_group(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<RenameGroupRequest>,
) -> Result<impl IntoResponse, Api> {
    ask(&state, |reply| SessionSupervisorCommand::RenameGroup {
        old: name,
        new: req.name,
        reply,
    })
    .await?
    .map_err(group_error)?;
    Ok(Json(Ack {}))
}

pub async fn delete_group(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, Api> {
    ask(&state, |reply| SessionSupervisorCommand::DeleteGroup { name, reply })
        .await?
        .map_err(group_error)?;
    Ok(Json(Ack {}))
}

/// Annotation keys are machine-facing: lowercase slug characters only.
fn valid_annotation_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 128
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

pub async fn set_annotations(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SetAnnotationsRequest>,
) -> Result<impl IntoResponse, Api> {
    if req.set.iter().any(|e| !valid_annotation_key(&e.key))
        || req.remove.iter().any(|k| !valid_annotation_key(k))
    {
        return Err(Api::unprocessable(
            "annotation keys must be 1-128 chars of [a-z0-9._-]",
        ));
    }
    let set: BTreeMap<String, String> = req.set.into_iter().map(|e| (e.key, e.value)).collect();
    ask(&state, |reply| SessionSupervisorCommand::SetSessionAnnotations {
        id,
        set,
        remove: req.remove,
        reply,
    })
    .await?
    .map_err(Api::not_found)?;
    Ok(Json(Ack {}))
}
```

Register in `server/src/http/mod.rs`: add `pub mod groups;` with the other module declarations, and routes after the `/api/sessions/:id` route:

```rust
        .route(
            "/api/sessions/:id/annotations",
            put(groups::set_annotations),
        )
        .route(
            "/api/session-groups",
            get(groups::list_groups).post(groups::create_group),
        )
        .route(
            "/api/session-groups/:name",
            put(groups::rename_group).delete(groups::delete_group),
        )
```

In `handlers.rs`, merge annotations into both views. In `summary()`:

```rust
        annotations: rec
            .annotations
            .iter()
            .map(|(key, value)| AnnotationEntry {
                key: key.clone(),
                value: value.clone(),
            })
            .collect(),
```

Add `AnnotationEntry` to the `horsie_models::session` import. Add the same mapping to the `SessionDetail` construction in `get_session` (field `annotations`, placed after `last_error`).

Also confirm `create_session`'s `SessionRecord` literal now has `annotations: BTreeMap::new()` (Task 2) — and since `summary()` reads `rec.annotations`, that record now carries an empty map, which is correct for a fresh session.

Note: `handlers::ask` is `pub(crate)` — groups.rs is in the same crate, fine. `AnnotationEntry` import in groups.rs is only needed if referenced; drop it if unused (it is unused — remove from the snippet when writing the file).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p horsie-server --features test-util http::`
Expected: PASS (all http tests, old and new).

- [ ] **Step 5: Commit**

```bash
git add server/src/http/groups.rs server/src/http/mod.rs server/src/http/handlers.rs
git commit -m "feat(server): group CRUD and session annotations HTTP API"
```

---

### Task 5: Web API client and react-query hooks

**Files:**
- Modify: `clients/web/src/api/client.ts` (`sessionGroups`, `sessions.setAnnotations`)
- Create: `clients/web/src/hooks/useGroups.ts`
- Test: `clients/web/src/hooks/useGroups.test.tsx` (light — hook wiring via a mocked `api`)

**Interfaces:**
- Consumes: Task 1 TS types.
- Produces: `api.sessionGroups.{list,create,rename,remove}`, `api.sessions.setAnnotations(id, body)`; hooks `useGroupList(): UseQueryResult<string[]>`, `useCreateGroup()`, `useRenameGroup()`, `useDeleteGroup()`, `useSetSessionAnnotations()`; query key `qk.groups = ["groups"]`. Mutations invalidate `qk.groups` and `qk.sessions`. Task 8 consumes all of these.

- [ ] **Step 1: Write the failing hook test**

`clients/web/src/hooks/useGroups.test.tsx` — follow the mocking style of an existing hook test (check `useSessionDraft.test.tsx`/`useAgentDraft.test.tsx` for the local pattern; if none mock `api`, use vi.mock):

```tsx
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderHook, waitFor } from "@testing-library/react";
import type { ReactNode } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { api } from "../api/client";
import { useDeleteGroup, useGroupList, useRenameGroup } from "./useGroups";

vi.mock("../api/client", () => ({
  api: {
    sessionGroups: {
      list: vi.fn(),
      create: vi.fn(),
      rename: vi.fn(),
      remove: vi.fn(),
    },
  },
}));

function wrapper(client: QueryClient) {
  return ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
}

afterEach(() => vi.clearAllMocks());

describe("useGroupList", () => {
  it("returns group names", async () => {
    vi.mocked(api.sessionGroups.list).mockResolvedValue({
      groups: [{ name: "web" }, { name: "api" }],
    });
    const client = new QueryClient();
    const { result } = renderHook(() => useGroupList(), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(result.current.data).toEqual(["web", "api"]));
  });
});

describe("group mutations", () => {
  it("rename invalidates the groups and sessions queries", async () => {
    vi.mocked(api.sessionGroups.rename).mockResolvedValue({});
    const client = new QueryClient();
    const groupsSpy = vi.spyOn(client, "invalidateQueries");
    const { result } = renderHook(() => useRenameGroup(), {
      wrapper: wrapper(client),
    });
    await result.current.mutateAsync({ oldName: "web", name: "frontend" });
    expect(api.sessionGroups.rename).toHaveBeenCalledWith("web", "frontend");
    expect(groupsSpy).toHaveBeenCalledWith({ queryKey: ["groups"] });
    expect(groupsSpy).toHaveBeenCalledWith({ queryKey: ["sessions"] });
  });

  it("delete invalidates the groups and sessions queries", async () => {
    vi.mocked(api.sessionGroups.remove).mockResolvedValue({});
    const client = new QueryClient();
    const spy = vi.spyOn(client, "invalidateQueries");
    const { result } = renderHook(() => useDeleteGroup(), {
      wrapper: wrapper(client),
    });
    await result.current.mutateAsync("web");
    expect(spy).toHaveBeenCalledWith({ queryKey: ["groups"] });
    expect(spy).toHaveBeenCalledWith({ queryKey: ["sessions"] });
  });
});
```

Note: `api.sessionGroups.rename` resolves `Ack` (`{}`); if the generated `Ack` type is an empty interface, `mockResolvedValue({})` may need `as never` — adjust to the existing test style's tolerance.

- [ ] **Step 2: Run test to verify it fails**

Run: `cd clients/web && bun run test:unit -- useGroups`
Expected: FAIL — `api.sessionGroups` and the hooks don't exist.

- [ ] **Step 3: Implement client + hooks**

In `clients/web/src/api/client.ts`, add imports `CreateGroupRequest, CreateGroupResponse, ListGroupsResponse, RenameGroupRequest, SetAnnotationsRequest` to the type import block, add to the `sessions` object:

```ts
    /** Merge-update a session's annotations (set upserts, remove drops). */
    setAnnotations: (id: string, body: SetAnnotationsRequest): Promise<Ack> =>
      request(`/sessions/${encodeURIComponent(id)}/annotations`, {
        method: "PUT",
        body: JSON.stringify(body),
      }),
```

and add a new top-level section after `sessions`:

```ts
  sessionGroups: {
    list: (): Promise<ListGroupsResponse> => request("/session-groups"),

    create: (name: string): Promise<CreateGroupResponse> =>
      request("/session-groups", {
        method: "POST",
        body: JSON.stringify({ name } satisfies CreateGroupRequest),
      }),

    rename: (oldName: string, name: string): Promise<Ack> =>
      request(`/session-groups/${encodeURIComponent(oldName)}`, {
        method: "PUT",
        body: JSON.stringify({ name } satisfies RenameGroupRequest),
      }),

    remove: (name: string): Promise<Ack> =>
      request(`/session-groups/${encodeURIComponent(name)}`, { method: "DELETE" }),
  },
```

Create `clients/web/src/hooks/useGroups.ts`:

```ts
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import { qk } from "./useSessions";

export const groupQk = {
  groups: ["groups"] as const,
};

/** Registered group names. The sidebar unions these with the `group`
 * annotations seen in the session list — an annotation-only group still
 * renders. */
export function useGroupList() {
  return useQuery({
    queryKey: groupQk.groups,
    queryFn: () => api.sessionGroups.list(),
    select: (r) => r.groups.map((g) => g.name),
  });
}

function useInvalidatingMutation<TVars>(
  fn: (vars: TVars) => Promise<unknown>,
  extra: (client: ReturnType<typeof useQueryClient>, vars: TVars) => void = () => {},
) {
  const client = useQueryClient();
  return useMutation({
    mutationFn: fn,
    onSuccess: (_r, vars) => {
      client.invalidateQueries({ queryKey: groupQk.groups });
      client.invalidateQueries({ queryKey: qk.sessions });
      extra(client, vars);
    },
  });
}

export function useCreateGroup() {
  return useInvalidatingMutation((name: string) => api.sessionGroups.create(name));
}

export function useRenameGroup() {
  return useInvalidatingMutation(
    ({ oldName, name }: { oldName: string; name: string }) =>
      api.sessionGroups.rename(oldName, name),
  );
}

export function useDeleteGroup() {
  return useInvalidatingMutation((name: string) => api.sessionGroups.remove(name));
}

/** Move a session: `set: [{key:"group",value}]` into a group, `remove:["group"]`
 * back to Ungrouped. */
export function useSetSessionAnnotations() {
  return useInvalidatingMutation(
    ({ id, set, remove }: { id: string; set: { key: string; value: string }[]; remove: string[] }) =>
      api.sessions.setAnnotations(id, { set, remove }),
    (client, { id }) => client.invalidateQueries({ queryKey: qk.session(id) }),
  );
}
```

- [ ] **Step 4: Run tests + typecheck**

Run: `cd clients/web && bun run test:unit -- useGroups && bun run typecheck`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add clients/web/src/api/client.ts clients/web/src/hooks/useGroups.ts clients/web/src/hooks/useGroups.test.tsx
git commit -m "feat(web): session-groups api client and hooks"
```

---

### Task 6: Sidebar grouping logic (pure functions)

**Files:**
- Create: `clients/web/src/lib/sessionGroups.ts`
- Test: `clients/web/src/lib/sessionGroups.test.ts`

**Interfaces:**
- Consumes: `SessionSummary` type.
- Produces (Task 8 uses all): `UNGROUPED = "ungrouped"`; `sessionGroup(s: SessionSummary): string | undefined`; `unionGroups(registered: string[], sessions: SessionSummary[]): string[]`; `partitionSessions(sessions: SessionSummary[]): Map<string, SessionSummary[]>` (keys are group names + `UNGROUPED`); `reconcileOrder(saved: string[], groups: string[]): string[]`; `moveBefore(order: string[], entry: string, target: string): string[]`.

Semantics:
- `unionGroups`: registered first (sorted), then annotation-only names (sorted), deduped. A group named literally `ungrouped` is allowed but collides with the sentinel — filter it out of `unionGroups` results (server allows the name; the frontend reserves the word).
- `partitionSessions`: every union group gets an entry (possibly empty); sessions without a `group` annotation go under `UNGROUPED`.
- `reconcileOrder`: keep saved entries that are still live groups or `UNGROUPED`, in saved order; append live groups not in `saved`, sorted; ensure `UNGROUPED` appears exactly once (append at end if missing).
- `moveBefore`: remove `entry`, reinsert immediately before `target`; if `target` absent, append.

- [ ] **Step 1: Write the failing tests**

`clients/web/src/lib/sessionGroups.test.ts`:

```ts
import { describe, expect, it } from "vitest";
import type { SessionSummary } from "../api/types";
import {
  UNGROUPED,
  moveBefore,
  partitionSessions,
  reconcileOrder,
  sessionGroup,
  unionGroups,
} from "./sessionGroups";

function session(id: string, group?: string): SessionSummary {
  return {
    id,
    name: null,
    status: null,
    createdAt: 1,
    lastError: null,
    annotations: group ? [{ key: "group", value: group }] : [],
  };
}

describe("sessionGroup", () => {
  it("reads the group annotation", () => {
    expect(sessionGroup(session("a", "web"))).toBe("web");
    expect(sessionGroup(session("a"))).toBeUndefined();
  });
});

describe("unionGroups", () => {
  it("unions registered groups with annotation-only groups, deduped, reserved word dropped", () => {
    const sessions = [session("a", "web"), session("b", "ops")];
    expect(unionGroups(["api", "web", UNGROUPED], sessions)).toEqual([
      "api",
      "web",
      "ops",
    ]);
  });
});

describe("partitionSessions", () => {
  it("buckets every session and keeps empty groups present", () => {
    const sessions = [session("a", "web"), session("b")];
    const parts = partitionSessions(sessions, ["web", "empty"]);
    expect(parts.get("web")?.map((s) => s.id)).toEqual(["a"]);
    expect(parts.get(UNGROUPED)?.map((s) => s.id)).toEqual(["b"]);
    expect(parts.get("empty")).toEqual([]);
  });
});

describe("reconcileOrder", () => {
  it("keeps saved live entries, appends new groups sorted, ungrouped exactly once", () => {
    expect(
      reconcileOrder(["gone", "web", UNGROUPED], ["web", "api", "ops"]),
    ).toEqual(["web", UNGROUPED, "api", "ops"]);
  });

  it("appends ungrouped when the saved order lacks it", () => {
    expect(reconcileOrder(["web"], ["web"])).toEqual(["web", UNGROUPED]);
  });
});

describe("moveBefore", () => {
  it("reinserts the entry before the target", () => {
    expect(moveBefore(["a", "b", "c"], "c", "a")).toEqual(["c", "a", "b"]);
    expect(moveBefore(["a", "b"], "a", "missing")).toEqual(["b", "a"]);
  });
});
```

Note the `partitionSessions` signature in the test takes `(sessions, groups)` — implement it that way: `partitionSessions(sessions: SessionSummary[], groups: string[]): Map<string, SessionSummary[]>`.

- [ ] **Step 2: Run to verify failure**

Run: `cd clients/web && bun run test:unit -- sessionGroups`
Expected: FAIL — module missing.

- [ ] **Step 3: Implement `clients/web/src/lib/sessionGroups.ts`**

```ts
import type { SessionSummary } from "../api/types";

/** The frontend-only section for sessions without a group annotation. Never
 * sent to the API; a real group with this name is filtered out of the union. */
export const UNGROUPED = "ungrouped";

/** The session's group, from its `group` annotation. */
export function sessionGroup(s: SessionSummary): string | undefined {
  return s.annotations.find((a) => a.key === "group")?.value;
}

/** Every group the sidebar renders: registered groups plus names seen only in
 * annotations, deduped; `ungrouped` is a reserved word, never a real group. */
export function unionGroups(
  registered: string[],
  sessions: SessionSummary[],
): string[] {
  const names = new Set(registered);
  for (const s of sessions) {
    const g = sessionGroup(s);
    if (g) names.add(g);
  }
  names.delete(UNGROUPED);
  return [...names].sort();
}

/** Bucket sessions by group; every listed group gets an entry, even empty. */
export function partitionSessions(
  sessions: SessionSummary[],
  groups: string[],
): Map<string, SessionSummary[]> {
  const parts = new Map<string, SessionSummary[]>();
  for (const g of [...groups, UNGROUPED]) parts.set(g, []);
  for (const s of sessions) {
    const g = sessionGroup(s);
    parts.get(g && parts.has(g) ? g : UNGROUPED)?.push(s);
  }
  return parts;
}

/** Merge the persisted order with the live group list: drop stale entries,
 * append new groups sorted, keep `ungrouped` exactly once. */
export function reconcileOrder(saved: string[], groups: string[]): string[] {
  const live = new Set([...groups, UNGROUPED]);
  const order = saved.filter((g, i) => live.has(g) && saved.indexOf(g) === i);
  for (const g of groups) if (!order.includes(g)) order.push(g);
  if (!order.includes(UNGROUPED)) order.push(UNGROUPED);
  return order;
}

/** Move `entry` to immediately before `target` (append if target is absent). */
export function moveBefore(
  order: string[],
  entry: string,
  target: string,
): string[] {
  const rest = order.filter((g) => g !== entry);
  const at = rest.indexOf(target);
  if (at === -1) return [...rest, entry];
  return [...rest.slice(0, at), entry, ...rest.slice(at)];
}
```

Adjust the `partitionSessions` test call above to match (`partitionSessions(sessions, ["web", "empty"])` — already written that way).

- [ ] **Step 4: Run to verify pass**

Run: `cd clients/web && bun run test:unit -- sessionGroups`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add clients/web/src/lib/sessionGroups.ts clients/web/src/lib/sessionGroups.test.ts
git commit -m "feat(web): sidebar grouping pure logic"
```

---

### Task 7: Dropdown menu component

**Files:**
- Create: `clients/web/src/components/Menu.tsx`
- Test: `clients/web/src/components/Menu.test.tsx`

**Interfaces:**
- Produces: `Menu({ label, children }: { label: ReactNode; children: ReactNode })` — renders a `key-icon` trigger button (the `...`) and an absolutely positioned panel; `MenuItem({ onSelect, danger?, children })`. Closes on item select, Escape, or outside pointerdown. Task 8 composes these.

- [ ] **Step 1: Write the failing test**

`clients/web/src/components/Menu.test.tsx`:

```tsx
import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { Menu, MenuItem } from "./Menu";

describe("Menu", () => {
  it("opens on trigger, selects an item, and closes", () => {
    const onSelect = vi.fn();
    render(
      <Menu label="group actions">
        <MenuItem onSelect={onSelect}>Rename</MenuItem>
      </Menu>,
    );
    expect(screen.queryByRole("menu")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "group actions" }));
    fireEvent.click(screen.getByRole("menuitem", { name: "Rename" }));
    expect(onSelect).toHaveBeenCalledOnce();
    expect(screen.queryByRole("menu")).toBeNull();
  });

  it("closes on Escape and on outside pointerdown", () => {
    render(
      <div>
        <span data-testid="outside">outside</span>
        <Menu label="group actions">
          <MenuItem onSelect={() => {}}>Rename</MenuItem>
        </Menu>
      </div>,
    );
    const trigger = screen.getByRole("button", { name: "group actions" });
    fireEvent.click(trigger);
    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByRole("menu")).toBeNull();
    fireEvent.click(trigger);
    fireEvent.pointerDown(screen.getByTestId("outside"));
    expect(screen.queryByRole("menu")).toBeNull();
  });
});
```

- [ ] **Step 2: Run to verify failure**

Run: `cd clients/web && bun run test:unit -- Menu`
Expected: FAIL.

- [ ] **Step 3: Implement `clients/web/src/components/Menu.tsx`**

```tsx
import { MoreHorizontal } from "lucide-react";
import {
  createContext,
  useContext,
  useEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { cn } from "../lib/cn";

const CloseContext = createContext<() => void>(() => {});

/** A minimal `...` dropdown: no dependency, skin-native. The panel anchors to
 * the trigger's right edge and closes on select, Escape, or outside click. */
export function Menu({
  label,
  testId,
  children,
}: {
  label: string;
  testId?: string;
  children: ReactNode;
}) {
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    const onPointer = (e: PointerEvent) => {
      if (!root.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("keydown", onKey);
    document.addEventListener("pointerdown", onPointer);
    return () => {
      document.removeEventListener("keydown", onKey);
      document.removeEventListener("pointerdown", onPointer);
    };
  }, [open]);

  return (
    <div className="relative" ref={root}>
      <button
        type="button"
        className="key-icon !h-6 !w-6"
        aria-label={label}
        aria-haspopup="menu"
        aria-expanded={open}
        data-testid={testId}
        onClick={(e) => {
          e.preventDefault();
          e.stopPropagation();
          setOpen((v) => !v);
        }}
      >
        <MoreHorizontal size={14} aria-hidden />
      </button>
      {open && (
        <div
          role="menu"
          className="absolute right-0 top-full z-50 mt-1 min-w-36 rounded-[var(--radius-control)] border bg-panel py-1 shadow-lg"
          onClick={(e) => {
            e.preventDefault();
            e.stopPropagation();
          }}
        >
          <CloseContext.Provider value={() => setOpen(false)}>
            {children}
          </CloseContext.Provider>
        </div>
      )}
    </div>
  );
}

export function MenuItem({
  onSelect,
  danger,
  testId,
  children,
}: {
  onSelect: () => void;
  danger?: boolean;
  testId?: string;
  children: ReactNode;
}) {
  const close = useContext(CloseContext);
  return (
    <button
      type="button"
      role="menuitem"
      data-testid={testId}
      className={cn(
        "block w-full px-3 py-1.5 text-left text-[13px] transition-colors hover:bg-raised",
        danger ? "text-red-ink" : "text-legend",
      )}
      onClick={(e) => {
        e.preventDefault();
        e.stopPropagation();
        close();
        onSelect();
      }}
    >
      {children}
    </button>
  );
}
```

(`e.preventDefault/stopPropagation` matter: menu triggers sit inside `NavLink`s in the sidebar.)

- [ ] **Step 4: Run to verify pass**

Run: `cd clients/web && bun run test:unit -- Menu`
Expected: PASS. If jsdom's `pointerdown` handling differs, dispatch `new PointerEvent("pointerdown", { bubbles: true })` via fireEvent instead.

- [ ] **Step 5: Commit**

```bash
git add clients/web/src/components/Menu.tsx clients/web/src/components/Menu.test.tsx
git commit -m "feat(web): minimal dropdown menu component"
```

---

### Task 8: Sidebar — sections, CRUD UI, drag-and-drop, order persistence

**Files:**
- Modify: `clients/web/src/components/Sidebar.tsx`
- Create: `clients/web/src/components/SessionRow.tsx` (moved out of Sidebar, gains menu + drag source)
- Create: `clients/web/src/components/SessionGroupSection.tsx` (header + collapsible rows + drop target)
- Test: `clients/web/src/components/Sidebar.test.tsx` (extend if it exists; else create)

**Interfaces:**
- Consumes: Tasks 5–7 (`useGroupList`, mutations, `sessionGroups` lib, `Menu`).
- Produces: the shipped UI + testids used by Task 9: `new-group-button`, `group-name-input`, `group-section` (with `data-group-name`), `group-menu-button`, `group-rename-input`, `session-row-menu`, `move-to-group-item` (with `data-group-name`).

Behavior details:
- **Order persistence**: `usePersistentState<string[]>("horsie.session-group-order", [])`; on every render compute `orderedGroups = reconcileOrder(saved, unionGroups(apiGroups, sessions))`; persist only via explicit reorder (don't write on reconcile — reconcile is pure display).
- **Add group**: `FolderPlus` icon button left of the `+` button in the Sessions header. Click → inline input appears at the top of the nav; Enter submits `useCreateGroup`, Escape cancels, empty input no-ops.
- **Group header**: collapse chevron (ephemeral `useState` per section), name, count optional; `Menu` with `Rename` and `Delete`:
  - Rename → header swaps to an input pre-filled with the name; Enter commits `useRenameGroup`, Escape cancels.
  - Delete → first click arms the item (label becomes "Confirm delete?"), second click calls `useDeleteGroup`; menu closes after select either way (arming state lives in the section component, reset when menu reopens).
  - Ungrouped section: no menu, not draggable as a group member? It IS reorderable (draggable header) but has no `...` menu.
- **Session row menu**: `Menu` at row right (visible on hover via `group` class + `group-hover:opacity-100 opacity-0`); items: "Ungrouped" + every union group, each calling `useSetSessionAnnotations` (`set`/`remove` per current membership). The menu button must stop propagation so the NavLink doesn't navigate (Menu already does).
- **Drag-and-drop** (HTML5, no lib):
  - Session row: `draggable`, `onDragStart` → `e.dataTransfer.setData("application/x-horsie-session", s.id)`, `effectAllowed = "move"`.
  - Group header (and the collapsed/expanded section header row): `onDragOver` → if types include the session mime, `preventDefault`; `onDrop` → assign that group (or remove, for Ungrouped) via `useSetSessionAnnotations`.
  - Group reorder: section header `draggable` with mime `application/x-horsie-group` carrying the group name; `onDrop` with that mime → `setOrder(moveBefore(saved, dragged, target))` persisted. Reorder operates on the *persisted* order reconciled with live groups: `setOrder(moveBefore(orderedGroups, dragged, target))`.
- Sessions within a group keep the existing sort (list order from `useSessionList`).
- `isLoading`/`isError`/empty states stay as-is, outside the sections.

- [ ] **Step 1: Write the failing component test**

`clients/web/src/components/Sidebar.test.tsx` (check for an existing file first and extend it; wrap with QueryClientProvider + MemoryRouter, mock `../api/client`):

```tsx
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";
import { api } from "../api/client";
import { Sidebar } from "./Sidebar";

vi.mock("../api/client", () => ({
  api: {
    sessions: {
      list: vi.fn(),
      setAnnotations: vi.fn(),
    },
    sessionGroups: {
      list: vi.fn(),
      create: vi.fn(),
      rename: vi.fn(),
      remove: vi.fn(),
    },
    globalEventsUrl: () => "/api/events",
  },
}));

function renderSidebar() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <MemoryRouter>
        <Sidebar />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

function session(id: string, group?: string) {
  return {
    id,
    name: `session ${id}`,
    status: null,
    createdAt: 1,
    lastError: null,
    annotations: group ? [{ key: "group", value: group }] : [],
  };
}

afterEach(() => vi.clearAllMocks());

describe("Sidebar groups", () => {
  it("renders union sections: registered, annotation-only, ungrouped", async () => {
    vi.mocked(api.sessionGroups.list).mockResolvedValue({ groups: [{ name: "api" }] });
    vi.mocked(api.sessions.list).mockResolvedValue({
      sessions: [session("1", "web"), session("2")],
    });
    renderSidebar();
    await waitFor(() => {
      expect(screen.getByTestId("group-section-api")).toBeDefined();
      expect(screen.getByTestId("group-section-web")).toBeDefined();
      expect(screen.getByTestId("group-section-ungrouped")).toBeDefined();
    });
    expect(
      screen.getByTestId("group-section-ungrouped").textContent,
    ).toContain("session 2");
  });

  it("creates a group from the header button", async () => {
    vi.mocked(api.sessionGroups.list).mockResolvedValue({ groups: [] });
    vi.mocked(api.sessionGroups.create).mockResolvedValue({ group: { name: "web" } });
    vi.mocked(api.sessions.list).mockResolvedValue({ sessions: [] });
    renderSidebar();
    fireEvent.click(await screen.findByTestId("new-group-button"));
    fireEvent.change(screen.getByTestId("group-name-input"), {
      target: { value: "web" },
    });
    fireEvent.keyDown(screen.getByTestId("group-name-input"), { key: "Enter" });
    await waitFor(() =>
      expect(api.sessionGroups.create).toHaveBeenCalledWith("web"),
    );
  });

  it("moves a session to a group from the row menu", async () => {
    vi.mocked(api.sessionGroups.list).mockResolvedValue({ groups: [{ name: "web" }] });
    vi.mocked(api.sessions.list).mockResolvedValue({ sessions: [session("1")] });
    vi.mocked(api.sessions.setAnnotations).mockResolvedValue({});
    renderSidebar();
    fireEvent.click(await screen.findByTestId("session-row-menu-1"));
    fireEvent.click(screen.getByTestId("move-to-group-web"));
    await waitFor(() =>
      expect(api.sessions.setAnnotations).toHaveBeenCalledWith("1", {
        set: [{ key: "group", value: "web" }],
        remove: [],
      }),
    );
  });

  it("deletes a group after the two-step confirm", async () => {
    vi.mocked(api.sessionGroups.list).mockResolvedValue({ groups: [{ name: "web" }] });
    vi.mocked(api.sessionGroups.remove).mockResolvedValue({});
    vi.mocked(api.sessions.list).mockResolvedValue({ sessions: [] });
    renderSidebar();
    fireEvent.click(await screen.findByTestId("group-menu-button-web"));
    fireEvent.click(screen.getByTestId("delete-group-item"));
    fireEvent.click(await screen.findByTestId("group-menu-button-web"));
    fireEvent.click(screen.getByTestId("confirm-delete-group-item"));
    await waitFor(() => expect(api.sessionGroups.remove).toHaveBeenCalledWith("web"));
  });
});
```

Notes for the implementer:
- Use per-section testids: `group-section-{name}`, `group-menu-button-{name}`, `session-row-menu-{id}`, `move-to-group-{name}` (with `ungrouped` for the sentinel menu item).
- The two-step delete: since the menu closes on select, "arm" state lives in the section; first item testid `delete-group-item`, and when armed the item renders as `confirm-delete-group-item` with label "Confirm delete?".
- `usePersistentState` reads localStorage — tests run in jsdom, which provides it; clear between tests (`localStorage.clear()` in `afterEach`).
- `useGlobalSessionFeed` is mounted elsewhere (check where — if the Sidebar mounts it, mock `globalEventsUrl` as above and jsdom's lack of EventSource may require `vi.stubGlobal("EventSource", class { close(){} } })`; follow what an existing Sidebar/App test does, if any).

- [ ] **Step 2: Run to verify failure**

Run: `cd clients/web && bun run test:unit -- Sidebar`
Expected: FAIL.

- [ ] **Step 3: Implement**

Move `SessionRow` from `Sidebar.tsx` into `SessionRow.tsx`, adding props `groups: string[]` and wiring the row `Menu` + drag source. Create `SessionGroupSection.tsx`:

```tsx
// Skeleton — fill in per the behavior list above.
export function SessionGroupSection({
  name,            // group name, or UNGROUPED
  sessions,        // already-partitioned rows
  groups,          // union list for the row menus
  ungrouped,       // name === UNGROUPED
}: { ... }) { ... }
```

Sidebar orchestration:

```tsx
const { data: sessions, isLoading, isError } = useSessionList();
const { data: registeredGroups } = useGroupList();
const [savedOrder, setSavedOrder] = usePersistentState<string[]>(
  "horsie.session-group-order",
  [],
  { deserialize: (raw) => (Array.isArray(raw) && raw.every((x) => typeof x === "string") ? raw : undefined) },
);
const groups = useMemo(
  () => unionGroups(registeredGroups ?? [], sessions ?? []),
  [registeredGroups, sessions],
);
const ordered = useMemo(() => reconcileOrder(savedOrder, groups), [savedOrder, groups]);
const parts = useMemo(
  () => partitionSessions(sessions ?? [], groups),
  [sessions, groups],
);
```

Render `ordered.map((g) => <SessionGroupSection ... />)` inside the existing `<nav>`, after the loading/error/empty states.

- [ ] **Step 4: Run tests + typecheck**

Run: `cd clients/web && bun run test:unit && bun run typecheck`
Expected: PASS (whole unit suite).

- [ ] **Step 5: Commit**

```bash
git add clients/web/src/components
git commit -m "feat(web): grouped, reorderable session sidebar with group CRUD"
```

---

### Task 9: E2E spec

**Files:**
- Create: `clients/web/e2e/s-session-groups.spec.ts`

**Interfaces:**
- Consumes: Task 8 testids; e2e `fixtures` (`page`, `appBase`, `mock`) and `helpers` (`createSession`, `sendMessage`).

- [ ] **Step 1: Write the spec**

```ts
// Group S — session groups: CRUD from the sidebar, membership, and the
// rename/delete annotation fixups end to end against the real server.

import { expect, test } from "./fixtures";
import { createSession, sendMessage } from "./helpers";

test.beforeEach(async ({ mock }) => {
  await mock.reset();
});

test("S1: group CRUD and session membership", async ({ page, appBase, mock }) => {
  await mock.queueText("hello");
  await createSession(page, appBase);
  const id = await sendMessage(page, "hi");
  const row = page.locator(`[data-testid="session-row"][data-session-id="${id}"]`);

  // Create a group from the Sessions header.
  await page.getByTestId("new-group-button").click();
  await page.getByTestId("group-name-input").fill("web");
  await page.getByTestId("group-name-input").press("Enter");
  await expect(page.getByTestId("group-section-web")).toBeVisible();

  // Move the session into it via the row menu.
  await page.getByTestId(`session-row-menu-${id}`).click();
  await page.getByTestId("move-to-group-web").click();
  await expect(
    page.getByTestId("group-section-web").locator(`[data-session-id="${id}"]`),
  ).toBeVisible();

  // Rename; the session follows.
  await page.getByTestId("group-menu-button-web").click();
  await page.getByTestId("rename-group-item").click();
  await page.getByTestId("group-rename-input").fill("frontend");
  await page.getByTestId("group-rename-input").press("Enter");
  await expect(
    page.getByTestId("group-section-frontend").locator(`[data-session-id="${id}"]`),
  ).toBeVisible();

  // Delete (two-step); the session lands back in Ungrouped.
  await page.getByTestId("group-menu-button-frontend").click();
  await page.getByTestId("delete-group-item").click();
  await page.getByTestId("group-menu-button-frontend").click();
  await page.getByTestId("confirm-delete-group-item").click();
  await expect(
    page.getByTestId("group-section-ungrouped").locator(`[data-session-id="${id}"]`),
  ).toBeVisible();
  await expect(row).toBeVisible();
});
```

(The rename item needs testid `rename-group-item` — add it in Task 8's menu; if Task 8 already shipped without it, add the testid as part of this task.)

- [ ] **Step 2: Run the spec**

Run: `cd clients/web && bun run test:e2e -- s-session-groups`
Expected: PASS. (This boots the real server + mock LLM via global-setup; ensure the workspace build is current: `cargo build -p horsie-server` first if the harness expects a binary — check `e2e/global-setup.ts` and build whatever it spawns.)

- [ ] **Step 3: Commit**

```bash
git add clients/web/e2e/s-session-groups.spec.ts
git commit -m "test(web): session groups e2e"
```

---

### Task 10: Full verification + PR

- [ ] **Step 1: Rust gates**

Run:
```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
cargo test --workspace
```
Expected: all PASS. Fix any fallout. Known construction sites that gain an `annotations` field: `SessionRecord` literals (`grep -rn "SessionRecord {" --include=*.rs`) and direct `SessionSummary` literals — `server/src/routines/runner.rs:162` builds one and must add `annotations: <map from rec, or vec![]>` (it has the `SessionRecord` at hand; use the same mapping as `handlers::summary`). If this fails compilation earlier, fix it in Task 4 rather than waiting for this task.

- [ ] **Step 2: Web gates**

Run: `cd clients/web && bun run typecheck && bun run test:unit && bun run build && bun run test:e2e`
Expected: PASS.

- [ ] **Step 3: Push and open the PR**

```bash
git push -u origin feat/session-annotations-groups
gh pr create --title "feat: session annotations and sidebar groups" --body "..."
```

Body: why/what plus bullets (annotations on `SessionRecord` via journal events; groups CRUD with fold-time fixups; sidebar union sections; frontend-only ordering; no DB migration). Then watch CI (`gh pr checks --watch`) and fix failures until green.
