# In-Server Runtime Vendors Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let horsie-server provision sandboxes itself — configured in the web UI — instead of requiring a separate `horsie connect` process for every vendor.

**Architecture:** Two small traits in `crates/runtime-vendor`, used on both sides of the wire: `RuntimeVendor` (create / get / hibernate / delete) and `RuntimeHandle` (id / relay / relay_oneway / closed). Every operation takes a `RuntimeProgressSink` and returns its *first observation* as a `RuntimeProgress`; anything later arrives on the sink, never before the call returns. `RuntimeProgress::Ready` carries the handle. A per-account `RuntimeManager` — a plain struct, not an actor — drains the sink, owns live handles, and lands runtime dial-backs from `/api/runtime/connect`.

**Tech Stack:** Rust 2024, tokio, axum 0.8, tokio-tungstenite 0.30, sqlx, fluorite schemas → generated Rust + two TypeScript trees.

**Spec:** `docs/superpowers/specs/2026-08-08-in-server-runtime-vendors-design.md`

## Global Constraints

- **Trait minimalism is the governing constraint.** `RuntimeVendor` has six members, `RuntimeHandle` four. A capability difference between substrates goes inside an implementation, never into a trait. Adding a default-implemented method later is non-breaking; changing a signature is not.
- **Naming.** Nothing is bare "Vendor". Every type carries `Runtime` or `RuntimeVendor`. `RuntimeManager` keeps its name.
- **Lints.** Production code denies `unwrap_used`, `expect_used`, `panic`, `wildcard_enum_match_arm`. Test modules opt out with the existing `#[allow(...)]` block.
- **Tests** live in `#[cfg(test)] mod tests` beside the source; full-stack tests in `crates/tests/tests/`.
- **Protocol** changes edit `crates/models/fluorite/*.fl`, never generated Rust. Then regenerate **both** trees: `cd clients/ts && npm run generate-types` and `cd clients/web && bun run generate-types`. CI guards only `clients/ts`, so skipping the web tree fails silently forever.
- **Database writes** use `Db::begin_write()`, never `pool().begin()`.
- **Pre-PR gate:** `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --workspace`. Iterate with `-p <crate> --lib`; run the workspace suite once before pushing.
- **No backward compatibility** is owed to older `horsie connect` builds or wire shapes.
- Package names: `horsie` (CLI), `horsie-server`, `horsie-runtime`, `horsie-runtime-vendor`, `horsie-runtime-client`, `horsie-velos-runtime`, `horsie-support`, `integration-tests`.

---

### Task 1: The two traits — **done** (`e66d514`, `7f54e06`)

Landed in `crates/runtime-vendor/src/runtime_vendor.rs`: `RuntimeVendor`,
`RuntimeHandle`, `RuntimeProgress`, `RuntimeEvent`, `RuntimeProgressSink`,
`RuntimeVendorError`. Four behavioural tests cover the contract — an immediate
vendor returning `Ready` without touching the sink, a slow vendor returning
`Starting` then finishing on the sink, one sink serving two runtimes, and
trait-object usability.

Also landed: `RuntimeVendor` (the vendor-crate struct) → `RuntimeVendorClient`,
freeing the name; `RuntimeVendorLink` → `WebsocketRuntimeVendor` in
`crates/server/src/runtime_vendor/proxy.rs`; `VendorError` →
`RuntimeVendorError`; the server's duplicate `VendorCapabilities` deleted in
favour of the wire type.

Exposed as `pub mod runtime_vendor` rather than a root re-export, because the
old `provider::RuntimeHandle` still exists and two traits of that name would be
a real ambiguity. The old one dies as each vendor is ported.

---

### Task 1b: `WebsocketRuntimeVendor` implements the traits

**Files:**
- Modify: `crates/server/src/runtime_vendor/proxy.rs`
- Modify: `crates/server/src/runtime_manager.rs`, `crates/server/src/http/vendor_connect.rs`
- Modify: `crates/server/src/sessions/spec.rs`

**Interfaces:**
- Consumes: `RuntimeVendor`, `RuntimeHandle`, `RuntimeProgress`, `RuntimeProgressSink`
- Produces: `RuntimeHandleImpl`; `RuntimeVendorMap = Arc<RwLock<HashMap<String, Arc<dyn RuntimeVendor>>>>`

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn create_returns_ready_because_a_vendor_process_replies_only_when_its_runtime_is_up() {
    let (vendor, mut script) = proxy_vendor().await;
    script.answer_create("s1").await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let progress = vendor.create("s1", &spec(), tx).await.unwrap();
    assert!(matches!(progress, RuntimeProgress::Ready(_)));
    assert!(rx.try_recv().is_err(), "a vendor with nothing to wait for emits nothing");
}

#[tokio::test]
async fn a_handle_resolves_the_live_link_per_call_so_a_reconnect_is_invisible() {
    // #187: a client is acquired once per run and baked into the toolbox, so a
    // handle that captured its link failed every remaining tool call in the
    // turn after a vendor reconnected. Resolving through the map per call is
    // what makes a reconnect mid-turn survivable.
    let (vendor, mut script) = proxy_vendor().await;
    script.answer_create("s1").await;
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let RuntimeProgress::Ready(handle) = vendor.create("s1", &spec(), tx).await.unwrap()
    else { panic!("expected Ready") };

    let reconnected = script.reconnect_under_the_same_name().await;
    reconnected.answer_relay("s1").await;
    assert!(handle.relay(ping()).await.is_ok());
}

#[tokio::test]
async fn a_relay_to_an_absent_vendor_is_retryable_not_terminal() {
    let (vendor, script) = proxy_vendor().await;
    script.answer_create("s1").await;
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let RuntimeProgress::Ready(handle) = vendor.create("s1", &spec(), tx).await.unwrap()
    else { panic!("expected Ready") };

    script.disconnect().await;
    // Never `Disconnected`: that means "can never work again", and the vendor
    // may well be back before the model asks for its next tool.
    assert!(matches!(handle.relay(ping()).await, Err(TransportError::SendFailed(_))));
}
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p horsie-server --lib runtime_vendor::proxy`
Expected: FAIL — `WebsocketRuntimeVendor` has inherent methods, not the trait.

- [ ] **Step 3: Implement**

`RuntimeHandleImpl` wraps a `runtime_id`, an `Arc<dyn RuntimeTransport>` and a
closed-signal. `WebsocketRuntimeVendor` supplies the existing
`RuntimeVendorTransport`, which resolves the live link through the vendor map on
every call; `WebsocketRuntimeVendor::start` gains the map so it can build one.

`create`/`get` return `Ready(handle)` and never touch the sink: a `horsie
connect` process replies only once its runtime is up, so there is no
intermediate state to report. `hibernate` returns `Stopped`, `delete` returns
`Gone`.

- [ ] **Step 4: Full gate, then commit**

Run: `cargo fmt --all && cargo clippy --all-targets --all-features -- -D warnings && cargo test --workspace`

```bash
git add -A && git commit -m "feat: the proxy vendor implements the runtime vendor traits"
```

---

### Task 2: Finish the naming pass (#234)

Renames only; the suite is the test.

- [ ] **Step 1: Rename the remaining types and fields**

`SharedVendors` → `RuntimeVendorMap` (done in Task 1), `VendorError` → `RuntimeVendorError`, delete the server's `VendorCapabilities` in favour of the wire type, `UserServices::vendor_agents` → `runtime_vendors`.

- [ ] **Step 2: Fix the user-facing strings**

`registry.rs::client_reason` → `format!("runtime vendor name \"{name}\" is already in use by another vendor process")`. `http/vendor_connect.rs` log lines → "runtime vendor connected" / "runtime vendor handshake failed". `registry.rs` → "runtime vendor disconnected, name released".

- [ ] **Step 3: Fix the guides**

`docs/guide/getting-started.md`: "`horsie connect` is a **vendor agent**" → "`horsie connect` runs a **runtime vendor**". Same substitution in `runtime-vendors.md` and `settings-reference.md`.

- [ ] **Step 4: Verify nothing calls a vendor an agent**

Run:
```bash
grep -rn 'vendor agent\|vendor_agents\|VendorError\|VendorCapabilities\|SharedVendors' crates/ docs/ clients/ --include='*.rs' --include='*.md' --include='*.ts' --include='*.tsx'
```
Expected: no output.

- [ ] **Step 5: Full gate, then commit**

```bash
git add -A && git commit -m "refactor: a runtime vendor is not an agent"
```

---

### Task 3: `RuntimeManager` becomes an actor

**Files:**
- Modify: `crates/server/src/runtime_manager.rs`
- Modify: `crates/server/src/users.rs` — the manager owns the registry
- Delete: the `lifecycle_locks` machinery in `crates/runtime-vendor/src/vendor.rs`

**Interfaces:**
- Consumes: `RuntimeVendor`, `RuntimeHandle`, `RuntimeProgress` (Task 1)
- Produces: `RuntimeManagerCommand::{Acquire, Provision, Release, Delete, RuntimeDialedBack}`; `RuntimeState::{Pending, Live, Hibernated}`

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn a_pending_runtime_becomes_live_when_it_dials_back() {
    let m = manager_with(PendingThenReadyVendor::new()).await;
    let acquiring = tokio::spawn({ let m = m.clone(); async move { m.acquire("s1").await } });
    m.dialed_back("s1", fake_handle("s1")).await;
    assert!(acquiring.await.unwrap().is_ok());
}

#[tokio::test]
async fn a_runtime_that_never_dials_back_fails_the_acquisition() {
    // It must fail, not hang: a session parked forever on a create is
    // indistinguishable from a deadlock, which is how #191's fake daemon bug
    // presented.
    let m = manager_with(SilentVendor).await;
    assert!(matches!(m.acquire_with_deadline("s1", SHORT).await, Err(RuntimeError::Provision(_))));
}

#[tokio::test]
async fn one_poll_covers_every_runtime_on_a_vendor() {
    let vendor = CountingVendor::new();
    let m = manager_with(vendor.clone()).await;
    m.acquire("s1").await.ok();
    m.acquire("s2").await.ok();
    m.tick_poll().await;
    assert_eq!(vendor.poll_calls(), 1, "poll granularity is per vendor, not per runtime");
}

#[tokio::test]
async fn a_closed_handle_is_dropped_so_no_turn_receives_it() {
    let m = manager_with(ReadyVendor).await;
    let h = m.acquire("s1").await.unwrap();
    h.close();
    tokio::task::yield_now().await;
    assert!(m.live_handle("s1").await.is_none());
}
```

- [ ] **Step 2: Run, implement, run**

The actor holds the vendor map, `HashMap<String, RuntimeState>`, and one poll task per vendor with backoff. `acquire` returns the live handle, or registers a waiter and drives `create`/`poll` until `Ready` or the provision window expires. It journals nothing — `SessionActor` already owns durable provisioning intent and re-sends `Provision` on load.

- [ ] **Step 3: Full gate, then commit**

```bash
git add -A && git commit -m "feat: the runtime manager owns runtime state, per account"
```

---

### Task 4: The derived dial token

**Files:**
- Create: `crates/support/src/dial_token.rs`; modify `crates/support/src/lib.rs`, `crates/support/Cargo.toml` (add `hmac = "0.13"`)
- Modify: `crates/runtime/src/main.rs` (`--connect-token`), `crates/runtime-vendor/src/listener.rs` (verify)

**Interfaces:**
- Produces: `horsie_support::dial_token::{mint, verify, DialClaims, DialTokenError}`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_minted_token_verifies_back_to_its_claims() {
    let claims = DialClaims { user_id: "u1".into(), runtime_id: "s1".into() };
    assert_eq!(verify(b"secret", &mint(b"secret", &claims)).unwrap(), claims);
}

#[test]
fn a_token_for_one_runtime_does_not_verify_as_another() {
    // Possession authorises exactly one runtime. Swapping the id must break
    // the tag, not silently re-address the token.
    let t = mint(b"secret", &DialClaims { user_id: "u1".into(), runtime_id: "s1".into() });
    assert!(matches!(verify(b"secret", &t.replace("s1", "s2")), Err(DialTokenError::BadSignature)));
}

#[test]
fn rotating_the_secret_invalidates_every_outstanding_token() {
    let t = mint(b"old", &DialClaims { user_id: "u1".into(), runtime_id: "s1".into() });
    assert!(matches!(verify(b"new", &t), Err(DialTokenError::BadSignature)));
}

#[test]
fn a_malformed_token_is_rejected_without_panicking() {
    for bad in ["", ".", "no-dot", "a.b.c.d"] {
        assert!(verify(b"secret", bad).is_err());
    }
}
```

- [ ] **Step 2: Implement**

`token = "<user_id>.<runtime_id>.<hex HMAC-SHA256(secret, \"<user_id>.<runtime_id>\")>"`, compared in constant time. hmac 0.13 puts `new_from_slice` on `KeyInit`, so the call is `<Hmac<Sha256> as Mac>::new_from_slice`.

- [ ] **Step 3: Verify on the standalone listener, present from the runtime**

`serve_runtime_connections` / `handle_runtime_connection` take a `secret: Vec<u8>`; reject a dial with no or bad bearer before the upgrade, and reject an announced id that differs from `claims.runtime_id`. `crates/runtime/src/main.rs` gains `--connect-token` and sets `authorization: Bearer …` via `IntoClientRequest`.

- [ ] **Step 4: Full gate, then commit**

```bash
git add -A && git commit -m "feat: authenticate the runtime dial-back"
```

---

### Task 5: `/api/runtime/connect`

**Files:**
- Create: `crates/server/src/http/runtime_connect.rs`; modify `crates/server/src/http/mod.rs`
- Modify: `crates/server/src/config/store.rs` — generate-once `runtime_dial_secret`

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn a_valid_dial_reaches_the_owning_accounts_manager() { /* … */ }

#[tokio::test]
async fn a_dial_with_no_token_is_rejected_before_the_upgrade() { /* … */ }

#[tokio::test]
async fn one_accounts_runtime_never_lands_in_anothers_manager() {
    // The token names the account, so there is no lookup to get wrong. Assert
    // the property directly rather than trusting the wiring.
}
```

- [ ] **Step 2: Implement, run, commit**

The handler verifies the bearer, resolves the account via `UserRegistry::get(user_id)`, upgrades, and sends `RuntimeManagerCommand::RuntimeDialedBack`.

```bash
git add -A && git commit -m "feat: runtimes dial the server, per account"
```

---

### Task 6: Acquisition carries its spec; `spec.json` deleted

- [ ] **Step 1: Change the schema**

In `crates/models/fluorite/runtime_vendor.fl`, `GetRuntimeRequest` gains `spec: RuntimeSpec`, with a doc comment saying the server is the only durable holder of it.

- [ ] **Step 2: Regenerate both TypeScript trees**

```bash
cd clients/ts && npm install --no-audit --no-fund && npm run generate-types && cd ../..
cd clients/web && bun install && bun run generate-types && cd ../..
git status --porcelain clients/
```
Expected: changes under **both** `clients/ts/src/generated` and `clients/web/src/generated`.

- [ ] **Step 3: Delete the duplicated state, run, commit**

Delete `spec_path`, `write_spec_file`, `persisted_spec` from `crates/runtime-vendor/src/vendor.rs`. Revival is gated on `supports_provisioning`, not on a file existing.

```bash
git add -A && git commit -m "feat: acquisition carries its spec, so vendors keep no disk state"
```

---

### Task 7: `runtime_vendors` table, JIT publication, settings UI

- [ ] **Step 1: Migration + config store**

`runtime_vendors(user_id, name, kind, credential, settings_json, callback_url)`, primary key `(user_id, name)`. `DbConfigStore::open` publishes one vendor per row; `update` rebuilds affected entries inside `begin_write`. An identity-class edit (kind, or a credential naming a different substrate account) warns with the count of sessions referencing that vendor; a `localhost` callback URL is refused at save time.

- [ ] **Step 2: Settings section**

Mirror the Providers section in `clients/web/src/pages/settings/`. Verify with `cd clients/web && bun run build && TMPDIR=/tmp bun run test:e2e` — `TMPDIR=/tmp` is required or Playwright's global setup dies on the macOS socket-path limit.

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "feat: runtime vendors configured per account"
```

---

## Follow-up plan

Spec steps 6–9 — `FlyRuntimeVendor` with volumes, the runtime reconnect loop, the velos port and `crates/velos-runtime` deletion, and the orphan sweep — get their own plan once this one has landed and the seam has run.
