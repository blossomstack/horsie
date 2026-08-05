# Per-user scoping, runtime tier — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the process-global half of the server per-account, so a deployment with two accounts shares a pool and a port and nothing else.

**Architecture:** `main.rs`'s composition root becomes `build_user(user, &Shared) -> Arc<UserServices>`, held in a lazy registry. A `Scope` extractor turns the `Principal` already in request extensions into that account's bundle. The supervisor, the vendor map, the event channel, the journal and every scoped service move inside it; the pool, auth, the artifact store and the routine timer stay outside.

**Tech Stack:** Rust 2024, axum 0.8, tokio, sqlx 0.9 over `sqlx::Any`.

Source spec: `docs/superpowers/specs/2026-08-05-per-user-scoping-runtime-tier.md`. Covers items 4–7 of `2026-08-04-per-user-scoping-design.md` plus the routine scheduler, and closes #225 and #226. Item 9 (`routes()`, `UserResolver`, `UsagePolicy`) is **out of scope** and gets its own issue.

## Global Constraints

- **Iterate with `cargo test -p horsie-server --lib <filter>`.** Run `make check` once before opening the PR, not per step.
- **`.fl` edits regenerate BOTH type trees** — `clients/web` and `clients/ts`. CI only drift-checks `clients/ts`, so the web one is the easy miss.
- **`clients/web` installs with `bun install --frozen-lockfile`, never `npm ci`.**
- **Never `-c user.name` / `-c user.email` on a commit.**
- **No `sqlx::query!` macros**; all SQL is a literal passed through `db.q(...)`, in SQLite placeholder style.
- Any new SQL touching a scoped table must name `user_id` or `server/src/db/scope_audit.rs` fails.

---

### Task 1: Drop the file journal backend

**Files:** `server/src/bin/horsie-server/config.rs`, `main.rs`; `actor/src/file_journal.rs`, `actor/src/lib.rs`, `actor/src/testkit.rs`, `actor/tests/journal_conformance.rs`, `actor/tests/journal_corruption.rs`; `models/*.fl` (`ServerInfo`), both generated type trees; `clients/web` settings info row; `server/tests/sql_journal.rs` and `tests/tests/session_server_e2e.rs` doc comments.

- [x] Delete `JournalConfig`, `JournalBackend`, `journal_backend()` and the `journal` field on `BootConfig`. `SqlJournal` becomes unconditional.
- [x] Delete `FileJournal`, its re-export, the testkit helpers that write its format, `actor/tests/journal_corruption.rs`, and the `FileJournal` half of `actor/tests/journal_conformance.rs` (including its five `#[ignore]`d red tests).
- [x] Remove `journal_backend` from `ServerInfo` in the fluorite schema; regenerate both type trees; drop the row from the web Settings info panel.
- [x] Correct the stale comment in `server/src/db/journal.rs` claiming the CLI uses `FileJournal` — it uses no journal at all.

**Verify:** `cargo test -p horsie-actor` and `-p horsie-server --lib` pass; `rg -n FileJournal` returns only history.

---

### Task 2: Delete the dead per-session state dir

**Files:** `server/src/runtime_manager.rs`, `server/src/sessions/spec.rs`, every construction site of `RuntimeDeps`/`ServerDeps` (session_actor, supervisor, http tests, e2e).

- [x] Remove the `create_dir_all(<state_dir>/sessions/<id>)` in `runtime_spec` — nothing reads that path.
- [x] Remove `RuntimeDeps::state_dir` and `ServerDeps::state_dir` and fix the construction sites.

**Verify:** `cargo test -p horsie-server --lib` passes; `rg -n 'state_dir' server/src` hits only `auth/service.rs` (the initial-password file) and boot config.

---

### Task 3: `Shared`, `UserServices`, and the lazy registry

**Files:** create `server/src/users.rs`; `server/src/lib.rs`.

**Interfaces produced:**
- `Shared` — `db`, `auth`, `artifacts: Arc<ArtifactStore>`, `artifact_secret`, `info: ServerInfo`, `model_card_seed: Arc<[ModelCardInput]>`, `model_card_seed_hash: String`, `anonymous: UserId`.
- `UserServices` — one public field per thing `AppState` holds today, minus what `Shared` owns.
- `UserRegistry::get(&self, user: &UserId) -> Result<Arc<UserServices>, ServerError>`.

- [x] Write `build_user`, lifting `main.rs` lines 148–282 verbatim where possible: `DbConfigStore::open_on` → `SqlJournal::new(db, user)` → services → `RuntimeManager` → `ServerDeps` → `broadcast::channel` → `spawn_root(SessionSupervisor::new(deps, tx, user))`.
- [x] Registry state is `RwLock<HashMap<UserId, Arc<OnceCell<Arc<UserServices>>>>>`: take the write lock only to insert the empty cell, then `get_or_try_init` outside it.
- [x] **Test:** two concurrent `get` calls for one account return `Arc::ptr_eq` bundles — the assertion that guards against two supervisors on one persistence id.
- [x] **Test:** two accounts get bundles whose vendor maps are distinct `Arc`s.

**Verify:** `cargo test -p horsie-server --lib users`.

---

### Task 4: Per-user supervisor, event channel, and vendor publish

**Files:** `server/src/sessions/supervisor.rs`, `server/src/http/sse.rs`, `server/src/http/vendor_connect.rs`.

- [x] `SessionSupervisor::new`/`with_config` take a `UserId`; `persistence_id()` returns `PersistenceId::new("session-supervisor", user.as_str())`.
- [x] **Test:** two supervisors on one journal with different users recover disjoint session lists.
- [x] `sse::global_events` reads the sender off the scope, not `AppState`.
- [x] `vendor_connect` resolves the owner's bundle from the `Principal` it already holds and publishes into that registry. Keep registry gate 2.

**Verify:** `cargo test -p horsie-server --lib supervisor sse vendor`.

---

### Task 5: Split `AppState` and add the `Scope` extractor

**Files:** `server/src/http/mod.rs` and every module under it.

- [x] `AppState { auth, web_dir, shared: Arc<Shared>, users: Arc<UserRegistry> }`.
- [x] `Scope(Arc<UserServices>)` implementing `FromRequestParts<AppState>`: `Principal::User(id)` → id, `Anonymous` → `shared.anonymous`, missing → 401. Registry failure → 500 through `Api::internal`.
- [x] Move every handler that reads a scoped service onto `Scope`. `/api/plugin-artifacts/{file}` moves onto `shared.artifacts` + `shared.artifact_secret` so it still serves ahead of the auth layer.
- [x] Rebuild `http::tests::test_state` on the new shape.

**Verify:** `cargo test -p horsie-server --lib http`.

---

### Task 6: One timer for every account

**Files:** `server/src/routines/scheduler.rs`, `server/src/routines/service.rs` (if `due` loses its last caller).

- [x] `RoutineScheduler::new(db, registry)`; `tick` calls `RoutineStore::due_across_all_users(&db, now)` and, per row, resolves the owner's bundle to `arm` then `run`.
- [x] **Test:** two accounts each with a due routine — both fire, each in its own scope.

**Verify:** `cargo test -p horsie-server --lib routines`.

---

### Task 7: Lazy model-card seeding

**Files:** `server/src/config/model_cards.rs`, `server/src/users.rs`.

- [x] Hash the resolved seed set once at boot (bundled + `--model-cards-seed`), into `Shared`.
- [x] `build_user` reads `settings` key `model_cards_seed` for that account; if it differs, run `seed_if_missing` and write the hash back. A DB error warns and does not fail the build — the admin API stays usable to fix state, as at boot today.
- [x] **Test:** first build seeds and second does not; a changed seed set reseeds; two accounts each get their own copy.

**Verify:** `cargo test -p horsie-server --lib model_cards`.

---

### Task 8: The composition root and a boot test

**Files:** `server/src/bin/horsie-server/main.rs`, plus a test module beside it.

- [x] `run()` shrinks to: resolve config → open db → bootstrap auth → build `Shared` → build the registry → spawn the scheduler → bind → serve.
- [x] Split the bind/serve tail so a test can hold the bound `TcpListener` and its address.
- [x] **Test (#226):** boot the real composition root against a `tempdir` with auth disabled, `GET /api/health` → 200, and create a session over HTTP so the lazy bundle is actually built.

**Verify:** `cargo test -p horsie-server --bin horsie-server`.

---

### Task 9: The HTTP isolation test

**Files:** create `tests/tests/user_isolation_http.rs`.

- [x] Two accounts via `AuthStore::create_user` + `insert_token`, auth enabled, over a real bound server.
- [x] A creates a session; B's `GET /api/sessions` is empty and `GET /api/sessions/{a_id}` is 404.
- [x] B's `/api/events` receives none of A's frames while A's session changes status.
- [x] Two fake vendor agents both announcing `main`, one per account, both register; each account's session resolves its own link.
- [x] B's `/api/config`, `/api/agents`, `/api/routines` do not show A's rows.

**Verify:** `cargo test -p horsie-tests --test user_isolation_http`.

---

### Task 10: Green and ship

- [x] `make check` once (fmt-check, clippy `-D warnings`, `cargo test --workspace`).
- [x] Web type-tree drift check and `bun run build` in `clients/web`.
- [x] Open the PR against `main`. Do **not** enable auto-merge.
- [x] File the follow-up issue for work-breakdown item 9.
