# In-Server Runtime Vendors Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let horsie-server provision sandboxes itself — configured in the web UI — instead of requiring a separate `horsie connect` process for every vendor.

**Architecture:** One `RuntimeVendor` trait in the server with two implementations: `RemoteRuntimeVendor` (today's WebSocket link to a `horsie connect` process) and `InProcessRuntimeVendor<P: RuntimeProvider>` (drives a provider directly). Runtimes dial the server at `/api/runtime/connect` with a derived bearer token; their transports land in a per-account `ConnectedRuntimeRegistry` inside `UserServices`. The session actor keeps owning lifecycle, so no new actor and no second journal.

**Tech Stack:** Rust 2024, tokio, axum 0.8, tokio-tungstenite 0.30, sqlx (`Db::begin_write`), fluorite schemas → generated Rust + two TypeScript trees, React web UI (bun).

**Spec:** `docs/superpowers/specs/2026-08-08-in-server-runtime-vendors-design.md`

## Global Constraints

- **Naming.** Nothing is called bare "Vendor". Every type carries `Runtime` or `RuntimeVendor`. `RuntimeProvider`, `RuntimeHandle` and `RuntimeManager` keep their names.
- **Lints.** Production code denies `unwrap_used`, `expect_used`, `panic`, `wildcard_enum_match_arm`. Test modules opt out with the existing `#[allow(...)]` block pattern.
- **Tests.** Unit tests live in `#[cfg(test)] mod tests` in the same `.rs`. Full-stack tests go in `crates/tests/tests/`.
- **Protocol.** Any wire type change edits `crates/models/fluorite/*.fl`, never the generated Rust. After editing a `.fl`, regenerate **both** TypeScript trees: `cd clients/ts && npm run generate-types` and `cd clients/web && bun run generate-types`. CI only guards `clients/ts`, so skipping `clients/web` fails silently forever.
- **Never** use fluorite for persisted structures. Database rows are hand-written.
- **Database writes** always use `Db::begin_write()`, never `pool().begin()`.
- **Pre-PR gate:** `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --workspace`. Iterate with `-p <crate> --lib`; run the full workspace suite once before pushing.
- **No backward compatibility** is owed to older `horsie connect` builds or older wire shapes.

---

### Task 1: The `RuntimeVendor` trait, over the existing link

Introduces the trait with exactly one implementation, so nothing behaves differently. This is the seam every later task plugs into.

**Files:**
- Modify: `crates/runtime-vendor/src/vendor.rs` — rename `RuntimeVendor` struct → `RuntimeVendorClient`
- Modify: `crates/runtime-vendor/src/lib.rs` — re-export the new name
- Modify: `crates/server/src/runtime_vendor/mod.rs` — add the trait, delete `VendorCapabilities`, rename `VendorError`
- Modify: `crates/server/src/runtime_vendor/link.rs` — `impl RuntimeVendor for RuntimeVendorLink`
- Modify: `crates/server/src/sessions/spec.rs:16-21` — retype the map alias
- Modify: `crates/server/src/runtime_manager.rs`, `crates/server/src/runtime_vendor/transport.rs`
- Modify: `crates/cli/src/connect.rs`, `crates/velos-runtime/src/main.rs` — construct `RuntimeVendorClient`

**Interfaces:**
- Produces: `horsie_server::runtime_vendor::RuntimeVendor` (trait), `RuntimeVendorError`, `RuntimeVendorMap = Arc<RwLock<HashMap<String, Arc<dyn RuntimeVendor>>>>`
- Consumes: nothing new

- [ ] **Step 1: Rename the vendor-process struct so the server trait can take the name**

```bash
cd crates/runtime-vendor
grep -rln 'RuntimeVendor\b' src/ tests/
```

In `src/vendor.rs` rename `pub struct RuntimeVendor` → `pub struct RuntimeVendorClient` and every `impl RuntimeVendor` / `RuntimeVendor::` reference. Update the module doc to say "the client half of a runtime vendor: dials a server and serves the commands it sends". In `src/lib.rs` change the re-export to `RuntimeVendorClient`.

- [ ] **Step 2: Verify the rename compiles across dependents**

Run: `cargo check -p horsie-runtime-vendor -p horsie -p velos-runtime`
Expected: PASS. `crates/cli/src/connect.rs` and `crates/velos-runtime/src/main.rs` need `RuntimeVendorClient::new(...)`.

- [ ] **Step 3: Commit the rename on its own**

```bash
git add -A && git commit -m "refactor: RuntimeVendor -> RuntimeVendorClient in the vendor crate"
```

- [ ] **Step 4: Write the failing test for a trait-object vendor map**

Add to `crates/server/src/runtime_vendor/mod.rs`:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, RwLock};

    /// A vendor that is not a socket. The point of the trait: the map holds
    /// `dyn RuntimeVendor`, so an implementation with no WebSocket behind it
    /// is storable and selectable exactly like a connected one.
    struct AlwaysGone;

    #[async_trait::async_trait]
    impl RuntimeVendor for AlwaysGone {
        fn capabilities(&self) -> RuntimeVendorCapabilities {
            RuntimeVendorCapabilities { supports_provisioning: true }
        }
        fn is_connected(&self) -> bool { true }
        async fn create(&self, _: &str, _: &RuntimeSpec) -> Result<(), RuntimeVendorError> {
            Err(RuntimeVendorError::Gone("no".into()))
        }
        async fn get(&self, _: &str) -> Result<(), RuntimeVendorError> {
            Err(RuntimeVendorError::Gone("no".into()))
        }
        async fn hibernate(&self, _: &str) {}
        async fn delete(&self, _: &str) {}
        async fn relay(&self, _: &str, _: RuntimeInboundMessage)
            -> Result<RuntimeOutboundMessage, TransportError> {
            Err(TransportError::SendFailed("no".into()))
        }
        async fn relay_oneway(&self, _: &str, _: RuntimeInboundMessage)
            -> Result<(), TransportError> {
            Err(TransportError::SendFailed("no".into()))
        }
    }

    #[tokio::test]
    async fn the_map_holds_a_vendor_that_is_not_a_socket() {
        let map: RuntimeVendorMap = Arc::new(RwLock::new(HashMap::new()));
        map.write().unwrap().insert("fly".into(), Arc::new(AlwaysGone) as Arc<dyn RuntimeVendor>);
        let vendor = map.read().unwrap().get("fly").cloned().unwrap();
        assert!(vendor.capabilities().supports_provisioning);
        assert!(matches!(vendor.get("s1").await, Err(RuntimeVendorError::Gone(_))));
    }
}
```

- [ ] **Step 5: Run it and watch it fail**

Run: `cargo test -p horsie --lib runtime_vendor::tests`
Expected: FAIL — `RuntimeVendor` and `RuntimeVendorMap` do not exist.

- [ ] **Step 6: Define the trait and the error**

In `crates/server/src/runtime_vendor/mod.rs`, delete `pub struct VendorCapabilities` entirely and re-export the wire type instead. Rename `VendorError` → `RuntimeVendorError` (variants `Provision`, `Gone`, `Unavailable` unchanged). Add:

```rust
pub use horsie_models::runtime_vendor::RuntimeVendorCapabilities;

/// A named source of runtimes, as the session layer sees it.
///
/// Two implementations: [`RemoteRuntimeVendor`] relays to a `horsie connect`
/// process, and `InProcessRuntimeVendor` drives a [`RuntimeProvider`] inside
/// this server. Everything above this trait — `RuntimeManager`, the transport,
/// the settings view — is written against the trait and branches on neither.
#[async_trait::async_trait]
pub trait RuntimeVendor: Send + Sync {
    fn capabilities(&self) -> RuntimeVendorCapabilities;

    /// Whether this vendor can be reached right now. A remote vendor answers
    /// "is my socket alive"; an in-process one answers "is my provider
    /// configured", which is why this is on the trait and not a socket check.
    fn is_connected(&self) -> bool;

    async fn create(&self, runtime_id: &str, spec: &RuntimeSpec) -> Result<(), RuntimeVendorError>;
    async fn get(&self, runtime_id: &str) -> Result<(), RuntimeVendorError>;
    async fn hibernate(&self, runtime_id: &str);
    async fn delete(&self, runtime_id: &str);

    async fn relay(
        &self,
        runtime_id: &str,
        message: RuntimeInboundMessage,
    ) -> Result<RuntimeOutboundMessage, TransportError>;

    async fn relay_oneway(
        &self,
        runtime_id: &str,
        message: RuntimeInboundMessage,
    ) -> Result<(), TransportError>;
}
```

In `crates/server/src/sessions/spec.rs`, replace the alias:

```rust
/// Runtime vendors keyed by name, behind a shared lock so a settings edit can
/// activate, reconfigure or retire one without a restart.
///
/// Not "Shared": since per-account services there is one of these per account,
/// and a name saying "shared" would read as deployment-wide — the opposite of
/// what it is, on the type where being wrong means running tool calls on
/// someone else's machine.
pub type RuntimeVendorMap = Arc<RwLock<HashMap<String, Arc<dyn RuntimeVendor>>>>;
```

- [ ] **Step 7: Implement the trait for the existing link**

In `crates/server/src/runtime_vendor/link.rs`, move the inherent `create`/`get`/`hibernate`/`delete`/`capabilities`/`is_connected` methods into `impl RuntimeVendor for RuntimeVendorLink`, and add relay by moving the body of `RuntimeVendorTransport::relay`/`send_oneway`:

```rust
#[async_trait::async_trait]
impl RuntimeVendor for RuntimeVendorLink {
    fn capabilities(&self) -> RuntimeVendorCapabilities { self.capabilities.clone() }

    fn is_connected(&self) -> bool { self.connected.load(Ordering::Relaxed) }

    async fn relay(
        &self,
        runtime_id: &str,
        message: RuntimeInboundMessage,
    ) -> Result<RuntimeOutboundMessage, TransportError> {
        let command = RuntimeVendorCommand::Runtime(RuntimeRelayRequest {
            runtime_id: runtime_id.to_string(),
            message,
        });
        match self.request(command).await.map_err(TransportError::SendFailed)? {
            RuntimeVendorEvent::Runtime(ev) => Ok(ev.message),
            RuntimeVendorEvent::Ready(_)
            | RuntimeVendorEvent::CreateRuntime(_)
            | RuntimeVendorEvent::GetRuntime(_)
            | RuntimeVendorEvent::HibernateRuntime(_)
            | RuntimeVendorEvent::DeleteRuntime(_)
            | RuntimeVendorEvent::QueryRuntimes(_)
            | RuntimeVendorEvent::RequestFailed(_)
            | RuntimeVendorEvent::RuntimeStateChanged(_) => Err(TransportError::SendFailed(
                "the vendor answered a relayed runtime request with a lifecycle event".to_string(),
            )),
        }
    }

    async fn relay_oneway(
        &self,
        runtime_id: &str,
        message: RuntimeInboundMessage,
    ) -> Result<(), TransportError> {
        self.send_oneway(RuntimeVendorCommand::Runtime(RuntimeRelayRequest {
            runtime_id: runtime_id.to_string(),
            message,
        }))
        .await
        .map_err(TransportError::SendFailed)
    }

    // create / get / hibernate / delete move here verbatim, with
    // `VendorError` renamed to `RuntimeVendorError`.
}
```

`RuntimeVendorTransport` keeps resolving the map per call — that is what makes a reconnect invisible to a turn in flight (#187) — but now delegates:

```rust
async fn vendor(&self) -> Result<Arc<dyn RuntimeVendor>, TransportError> { /* unchanged body, new type */ }

#[async_trait]
impl RuntimeTransport for RuntimeVendorTransport {
    async fn relay(&self, message: RuntimeInboundMessage)
        -> Result<RuntimeOutboundMessage, TransportError> {
        self.vendor().await?.relay(&self.runtime_id, message).await
    }
    async fn send_oneway(&self, message: RuntimeInboundMessage) -> Result<(), TransportError> {
        self.vendor().await?.relay_oneway(&self.runtime_id, message).await
    }
}
```

`RuntimeManager::vendor` returns `Arc<dyn RuntimeVendor>`; its body is otherwise unchanged.

- [ ] **Step 8: Run the test and the crate suite**

Run: `cargo test -p horsie --lib runtime_vendor`
Expected: PASS, including the pre-existing link and transport tests.

- [ ] **Step 9: Commit**

```bash
git add -A && git commit -m "feat: one RuntimeVendor trait behind the vendor map"
```

---

### Task 2: Finish the naming pass (#234)

Renames only. No behaviour changes, so the whole suite is the test.

**Files:**
- Modify: `crates/server/src/runtime_vendor/link.rs`, `registry.rs`, `transport.rs`, `mod.rs`
- Modify: `crates/server/src/users.rs` (`vendor_agents` → `runtime_vendors`), `crates/server/src/http/vendor_connect.rs`
- Modify: `docs/guide/getting-started.md`, `docs/guide/runtime-vendors.md`, `docs/guide/settings-reference.md`

**Interfaces:**
- Produces: `RemoteRuntimeVendor` (was `RuntimeVendorLink`)
- Consumes: `RuntimeVendor`, `RuntimeVendorMap` from Task 1

- [ ] **Step 1: Rename the link type**

```bash
grep -rl 'RuntimeVendorLink' crates/ | xargs sed -i '' 's/RuntimeVendorLink/RemoteRuntimeVendor/g'
git mv crates/server/src/runtime_vendor/link.rs crates/server/src/runtime_vendor/remote.rs
```

Update `mod.rs`: `mod remote;` and `pub use remote::RemoteRuntimeVendor;`.

- [ ] **Step 2: Rename the `UserServices` field**

In `crates/server/src/users.rs`, `pub vendors: SharedVendors` → `pub runtime_vendors: RuntimeVendorMap`, and fix every construction site. Update the doc comment to stop saying "vendor agents".

- [ ] **Step 3: Fix the user-facing strings**

In `registry.rs`, `client_reason` becomes:

```rust
format!("runtime vendor name \"{name}\" is already in use by another vendor process")
```

In `http/vendor_connect.rs`, log lines become `"runtime vendor connected"` / `"runtime vendor handshake failed"`. In `registry.rs`, `"vendor agent disconnected, name released"` → `"runtime vendor disconnected, name released"`.

- [ ] **Step 4: Fix the guides**

In `docs/guide/getting-started.md` replace "`horsie connect` is a **vendor agent**" with "`horsie connect` runs a **runtime vendor**". Same substitution in `runtime-vendors.md` and `settings-reference.md` ("Vendors are configured in their own agent process" → "A runtime vendor either runs in the server or in its own `horsie connect` process").

- [ ] **Step 5: Verify nothing calls a vendor an agent any more**

Run:
```bash
grep -rn 'vendor agent\|vendor_agents\|VendorError\|VendorCapabilities\|SharedVendors' crates/ docs/ clients/ --include='*.rs' --include='*.md' --include='*.ts' --include='*.tsx'
```
Expected: no output.

- [ ] **Step 6: Full gate, then commit**

Run: `cargo fmt --all && cargo clippy --all-targets --all-features -- -D warnings && cargo test --workspace`
Expected: PASS.

```bash
git add -A && git commit -m "refactor: a runtime vendor is not an agent"
```

---

### Task 3: A derived dial token, and a runtime that presents it

Closes #191's stated blocker on its own, before any in-server vendor exists.

**Files:**
- Create: `crates/support/src/dial_token.rs`
- Modify: `crates/support/src/lib.rs`
- Modify: `crates/runtime/src/main.rs` — `--connect-token`, sent as a bearer
- Modify: `crates/runtime-vendor/src/listener.rs` — verify
- Modify: `crates/runtime-vendor/src/vendor.rs` — mint per runtime, pass via argv

**Interfaces:**
- Produces: `horsie_support::dial_token::{mint, verify, DialClaims, DialTokenError}`
- Consumes: nothing from earlier tasks

- [ ] **Step 1: Write the failing token tests**

Create `crates/support/src/dial_token.rs`:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_minted_token_verifies_back_to_its_claims() {
        let claims = DialClaims { user_id: "u1".into(), runtime_id: "s1".into() };
        let token = mint(b"secret", &claims);
        assert_eq!(verify(b"secret", &token).unwrap(), claims);
    }

    #[test]
    fn a_token_for_one_runtime_does_not_verify_as_another() {
        // The whole point: possession authorises exactly one runtime. Swapping
        // the id must break the tag, not silently re-address the token.
        let token = mint(b"secret", &DialClaims { user_id: "u1".into(), runtime_id: "s1".into() });
        let forged = token.replace("s1", "s2");
        assert!(matches!(verify(b"secret", &forged), Err(DialTokenError::BadSignature)));
    }

    #[test]
    fn rotating_the_secret_invalidates_every_outstanding_token() {
        let token = mint(b"old", &DialClaims { user_id: "u1".into(), runtime_id: "s1".into() });
        assert!(matches!(verify(b"new", &token), Err(DialTokenError::BadSignature)));
    }

    #[test]
    fn a_malformed_token_is_rejected_without_panicking() {
        for bad in ["", ".", "no-dot", "a.b.c.d", "dTE.cnQx"] {
            assert!(verify(b"secret", bad).is_err());
        }
    }
}
```

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test -p horsie-support --lib dial_token`
Expected: FAIL — module not found.

- [ ] **Step 3: Implement the token**

Above the tests in the same file:

```rust
//! The credential a runtime presents when it dials back.
//!
//! Derived, never stored: there is no per-runtime row to migrate, nothing to
//! expire, a server restart changes nothing, and rotating one secret
//! invalidates every outstanding token at once.
//!
//! A token authorises exactly one `runtime_id`. That is enough because holding
//! it is already equivalent to *being* that runtime; what it buys is that a
//! stranger cannot become one.

use hmac::{Hmac, Mac};
use sha2::Sha256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialClaims {
    pub user_id: String,
    pub runtime_id: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DialTokenError {
    #[error("malformed dial token")]
    Malformed,
    #[error("dial token signature does not match")]
    BadSignature,
}

fn tag(secret: &[u8], payload: &str) -> String {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret)
        .unwrap_or_else(|_| unreachable!("HMAC accepts any key length"));
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// `<user_id>.<runtime_id>.<hex tag>`. Both ids are hex-free of `.` by
/// construction (UUIDs and account ids), so a plain split is unambiguous.
#[must_use]
pub fn mint(secret: &[u8], claims: &DialClaims) -> String {
    let payload = format!("{}.{}", claims.user_id, claims.runtime_id);
    let tag = tag(secret, &payload);
    format!("{payload}.{tag}")
}

pub fn verify(secret: &[u8], token: &str) -> Result<DialClaims, DialTokenError> {
    let parts: Vec<&str> = token.split('.').collect();
    let [user_id, runtime_id, presented] = parts.as_slice() else {
        return Err(DialTokenError::Malformed);
    };
    if user_id.is_empty() || runtime_id.is_empty() {
        return Err(DialTokenError::Malformed);
    }
    let payload = format!("{user_id}.{runtime_id}");
    let expected = tag(secret, &payload);
    // Constant-time: a byte-by-byte early exit leaks the tag one byte at a time.
    if !constant_time_eq(expected.as_bytes(), presented.as_bytes()) {
        return Err(DialTokenError::BadSignature);
    }
    Ok(DialClaims {
        user_id: (*user_id).to_string(),
        runtime_id: (*runtime_id).to_string(),
    })
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
```

Add `hmac = "0.13"` to `crates/support/Cargo.toml` (`sha2` and `hex` are already there), and `pub mod dial_token;` to `crates/support/src/lib.rs`.

Note: hmac 0.13 moved `new_from_slice` onto the `KeyInit` trait, which is why the import is `hmac::{Hmac, Mac}` plus the fully-qualified `<Hmac<Sha256> as Mac>::new_from_slice`.

- [ ] **Step 4: Run and watch it pass**

Run: `cargo test -p horsie-support --lib dial_token`
Expected: PASS, 4 tests.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat: a derived dial-back token"
```

- [ ] **Step 6: Write the failing listener-auth test**

In `crates/runtime-vendor/src/listener.rs` tests:

```rust
#[tokio::test]
async fn a_runtime_dialling_with_no_token_is_refused() {
    let (listener, addr) = bound_listener().await;
    let registry = Arc::new(ConnectedRuntimeRegistry::new());
    serve_runtime_connections(listener, registry.clone(), b"secret".to_vec(), CancellationToken::new());

    let refused = tokio_tungstenite::connect_async(format!("ws://{addr}/")).await;
    assert!(refused.is_err(), "an unauthenticated dial must not be upgraded");
}

#[tokio::test]
async fn a_runtime_dialling_for_another_runtime_id_is_refused() {
    // The pre-auth hole: whoever announced a runtime_id first was registered as
    // that runtime's transport. A token bound to `s1` must not register `s2`.
    let (listener, addr) = bound_listener().await;
    let registry = Arc::new(ConnectedRuntimeRegistry::new());
    serve_runtime_connections(listener, registry.clone(), b"secret".to_vec(), CancellationToken::new());

    let token = horsie_support::dial_token::mint(
        b"secret",
        &horsie_support::dial_token::DialClaims { user_id: "u1".into(), runtime_id: "s1".into() },
    );
    let mut req = format!("ws://{addr}/").into_client_request().unwrap();
    req.headers_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    announce(&mut ws, "s2").await;

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(registry.runtime_transport("s2").await.is_none());
}
```

- [ ] **Step 7: Run and watch it fail**

Run: `cargo test -p horsie-runtime-vendor --lib listener`
Expected: FAIL — `serve_runtime_connections` takes 3 arguments, not 4.

- [ ] **Step 8: Verify the token in the listener**

`serve_runtime_connections` and `handle_runtime_connection` take a `secret: Vec<u8>`. Before the WebSocket upgrade, read the `authorization` header; reject the connection when it is absent or `verify` fails. After the runtime announces its id, reject if it differs from `claims.runtime_id`.

- [ ] **Step 9: Add `--connect-token` to the runtime and mint it vendor-side**

In `crates/runtime/src/main.rs` add `#[arg(long)] connect_token: Option<String>` and build the WS request through `IntoClientRequest`, setting `authorization: Bearer <token>` when present. In `crates/runtime-vendor/src/vendor.rs::provision`, mint a token for the runtime and append `--connect-token <token>` to the spawned argv.

- [ ] **Step 10: Full gate, then commit**

Run: `cargo fmt --all && cargo clippy --all-targets --all-features -- -D warnings && cargo test --workspace`

```bash
git add -A && git commit -m "feat: authenticate the runtime dial-back"
```

---

### Task 4: `get` carries the spec; delete `spec.json` and `lifecycle_locks`

Removes the vendor's only durable state and its only ordering machinery, both of which duplicate what the server already has.

**Files:**
- Modify: `crates/models/fluorite/runtime_vendor.fl` — `GetRuntimeRequest` gains `spec`
- Regenerate: `clients/ts/src/generated`, `clients/web/src/generated`
- Modify: `crates/runtime-vendor/src/vendor.rs` — delete `spec_path`, `write_spec_file`, `persisted_spec`, `lifecycle_locks`
- Modify: `crates/server/src/runtime_vendor/remote.rs` — `get` sends the spec
- Modify: `crates/server/src/runtime_manager.rs` — assemble the spec for `get` too

**Interfaces:**
- Consumes: `RuntimeVendor` from Task 1
- Produces: `RuntimeVendor::get(&self, runtime_id: &str, spec: &RuntimeSpec)`

- [ ] **Step 1: Change the schema**

In `crates/models/fluorite/runtime_vendor.fl`, replace the `GetRuntimeRequest` struct and its doc comment:

```
/// Hand back an existing runtime, rebuilding it if the vendor hibernated or
/// lost it.
///
/// Carries the spec because the server is the only durable holder of it: the
/// session row persists the vendor, workspaces and provision steps, so a vendor
/// keeping its own copy on disk was duplicating them. A vendor that cannot
/// rebuild (`supports_provisioning: false`) still fails the request, and the
/// server turns that into a terminally unrecoverable session.
struct GetRuntimeRequest { runtime_id: String, spec: RuntimeSpec }
```

- [ ] **Step 2: Regenerate both TypeScript trees**

```bash
cd clients/ts && npm install --no-audit --no-fund && npm run generate-types && cd ../..
cd clients/web && bun install && bun run generate-types && cd ../..
git status --porcelain clients/
```
Expected: changes in **both** `clients/ts/src/generated` and `clients/web/src/generated`. If only one moved, the other command did not run — CI guards only `clients/ts`, so the web tree would go stale silently.

- [ ] **Step 3: Write the failing test that a get rebuilds from the passed spec**

In `crates/runtime-vendor/tests/vendor_conformance.rs`:

```rust
/// A vendor holds no durable record of a runtime. After a restart its map is
/// empty, and the *only* way a get can succeed is the spec the server sends —
/// which is the whole reason `spec.json` could be deleted.
#[tokio::test]
async fn a_get_rebuilds_a_runtime_the_vendor_has_never_seen() {
    let h = harness().await;
    h.create("rt-1", &spec_with_workspace("main")).await.unwrap();
    h.restart_vendor_losing_all_memory().await;

    h.get("rt-1", &spec_with_workspace("main")).await.unwrap();
    assert!(h.is_live("rt-1").await);
}
```

- [ ] **Step 4: Run and watch it fail**

Run: `cargo test -p horsie-runtime-vendor --test vendor_conformance`
Expected: FAIL — `get` takes one argument.

- [ ] **Step 5: Delete the duplicated state**

In `crates/runtime-vendor/src/vendor.rs`: delete `spec_path`, `write_spec_file`, `persisted_spec`, and the `respawnable` guard around writing them. `GetRuntime` now calls `provision(&cmd.runtime_id, &cmd.spec)` when the runtime is not live, gated on `supports_provisioning` rather than on a file existing. Delete `lifecycle_locks` and the `lifecycle_lock` helper — the session actor serialises per `runtime_id`, and `runtime_id` is the session id.

In `crates/server/src/runtime_manager.rs`, `get` assembles the spec exactly as `create` does (fresh GitHub and plugin tokens — a stale one is worse than none) and passes it through.

- [ ] **Step 6: Run and watch it pass**

Run: `cargo test -p horsie-runtime-vendor && cargo test -p horsie --lib runtime_vendor`
Expected: PASS.

- [ ] **Step 7: Full gate, then commit**

Run: `cargo fmt --all && cargo clippy --all-targets --all-features -- -D warnings && cargo test --workspace`

```bash
git add -A && git commit -m "feat: a get carries its spec, so vendors keep no disk state"
```

---

### Task 5: `/api/runtime/connect` and a per-account registry

The server accepts runtime dial-backs again, and owns where they land.

**Files:**
- Create: `crates/server/src/http/runtime_connect.rs`
- Modify: `crates/server/src/http/mod.rs` — route registration
- Modify: `crates/server/src/users.rs` — `connected_runtimes: Arc<ConnectedRuntimeRegistry>` in `UserServices`
- Modify: `crates/server/src/config/store.rs` — a generated-once `runtime_dial_secret` setting

**Interfaces:**
- Consumes: `horsie_support::dial_token` (Task 3), `RuntimeVendorMap` (Task 1)
- Produces: `UserServices::connected_runtimes`, route `GET /api/runtime/connect`

- [ ] **Step 1: Write the failing route tests**

In `crates/server/src/http/runtime_connect.rs`:

```rust
#[tokio::test]
async fn a_dial_with_a_valid_token_registers_the_transport_for_that_account() {
    let h = server_harness().await;
    let token = h.mint_dial_token("u1", "s1");
    let mut ws = h.dial_runtime_connect(&token).await.unwrap();
    announce_ready(&mut ws, "s1").await;

    let services = h.user_services("u1").await;
    assert!(services.connected_runtimes.runtime_transport("s1").await.is_some());
}

#[tokio::test]
async fn a_dial_with_no_token_is_rejected_before_the_upgrade() {
    let h = server_harness().await;
    assert!(h.dial_runtime_connect_raw(None).await.is_err());
}

#[tokio::test]
async fn one_accounts_runtime_never_lands_in_anothers_registry() {
    // The token names the account, so there is no lookup to get wrong. This
    // asserts the property directly rather than trusting the wiring.
    let h = server_harness().await;
    let token = h.mint_dial_token("u1", "s1");
    let mut ws = h.dial_runtime_connect(&token).await.unwrap();
    announce_ready(&mut ws, "s1").await;

    let other = h.user_services("u2").await;
    assert!(other.connected_runtimes.runtime_transport("s1").await.is_none());
}
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p horsie --lib http::runtime_connect`
Expected: FAIL — module does not exist.

- [ ] **Step 3: Add the registry to `UserServices`**

In `crates/server/src/users.rs` add `pub connected_runtimes: Arc<ConnectedRuntimeRegistry>`, constructed once per account alongside the vendor map, with the doc comment explaining it is per-account so a transport can never be looked up across accounts.

- [ ] **Step 4: Add the dial secret**

In `crates/server/src/config/store.rs`, read the `runtime_dial_secret` setting at open; when absent, generate 32 bytes with `rand::fill`, hex-encode, and persist inside a `Db::begin_write()` transaction.

- [ ] **Step 5: Implement the route**

```rust
//! `GET /api/runtime/connect` — the one endpoint *runtimes* dial.
//!
//! Distinct from `/api/vendor/connect`, which vendor processes dial. The bearer
//! is self-describing (`user.runtime.tag`), so this handler resolves the owning
//! account without a database read: a sandbox learning its own account id is
//! not a disclosure, it is that account's own sandbox.
pub async fn runtime_connect(
    State(state): State<Arc<Shared>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response { /* verify -> UserRegistry::get(user_id) -> upgrade -> register_transport */ }
```

Register it in `http/mod.rs` beside `/api/vendor/connect`.

- [ ] **Step 6: Run and watch them pass**

Run: `cargo test -p horsie --lib http::runtime_connect`
Expected: PASS, 3 tests.

- [ ] **Step 7: Full gate, then commit**

```bash
git add -A && git commit -m "feat: runtimes dial the server, per account"
```

---

### Task 6: `InProcessRuntimeVendor<P>`

The second implementation of the trait — the reason it exists.

**Files:**
- Create: `crates/server/src/runtime_vendor/in_process.rs`
- Modify: `crates/server/src/runtime_vendor/mod.rs`

**Interfaces:**
- Consumes: `RuntimeVendor` (Task 1), `ConnectedRuntimeRegistry` on `UserServices` (Task 5), `RuntimeProvider` from `horsie-runtime-vendor`
- Produces: `InProcessRuntimeVendor<P: RuntimeProvider>`

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn create_waits_for_the_runtime_to_dial_back() {
    let registry = Arc::new(ConnectedRuntimeRegistry::new());
    let vendor = InProcessRuntimeVendor::new(DialingProvider::new(registry.clone()), registry, ..);
    vendor.create("s1", &spec()).await.unwrap();
    assert!(vendor.is_live("s1").await);
}

#[tokio::test]
async fn create_fails_when_the_runtime_never_dials() {
    let registry = Arc::new(ConnectedRuntimeRegistry::new());
    let vendor = InProcessRuntimeVendor::new(SilentProvider, registry, ..);
    assert!(matches!(vendor.create("s1", &spec()).await, Err(RuntimeVendorError::Provision(_))));
}

#[tokio::test]
async fn a_relay_resolves_the_transport_per_call() {
    // A reconnect replaces the registry entry. A vendor that cached the
    // transport would keep writing into the dead socket for the rest of the
    // turn — #187, one layer down.
    let registry = Arc::new(ConnectedRuntimeRegistry::new());
    let vendor = InProcessRuntimeVendor::new(DialingProvider::new(registry.clone()), registry.clone(), ..);
    vendor.create("s1", &spec()).await.unwrap();

    let second = Arc::new(RecordingTransport::default());
    registry.register_transport("s1".into(), second.clone()).await;
    vendor.relay("s1", ping()).await.unwrap();
    assert_eq!(second.calls(), 1);
}
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test -p horsie --lib runtime_vendor::in_process`
Expected: FAIL — type does not exist.

- [ ] **Step 3: Implement it**

Holds the provider, the account's `ConnectedRuntimeRegistry`, the workspace root, and `handles: Mutex<HashMap<String, Arc<dyn RuntimeHandle>>>` — **handles only, never transports**. `create` registers the readiness waiter before calling the provider, then awaits it with the provision window. `relay` resolves the transport from the registry on every call. `hibernate` and `delete` call through to the handle.

- [ ] **Step 4: Run and watch them pass**

Run: `cargo test -p horsie --lib runtime_vendor::in_process`
Expected: PASS, 3 tests.

- [ ] **Step 5: Full gate, then commit**

```bash
git add -A && git commit -m "feat: InProcessRuntimeVendor over any RuntimeProvider"
```

---

### Task 7: The `runtime_vendors` table and just-in-time publication

**Files:**
- Create: `crates/server/migrations/<next>_runtime_vendors.sql` (both dialects, per the existing migration layout)
- Modify: `crates/server/src/config/store.rs`
- Modify: `crates/models/fluorite/settings.fl` — the settings view gains configured vendors

**Interfaces:**
- Consumes: `InProcessRuntimeVendor` (Task 6)
- Produces: `runtime_vendors` rows → published `RuntimeVendorMap` entries

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn a_saved_vendor_is_selectable_without_a_restart() {
    let h = config_harness().await;
    h.save_runtime_vendor("fly-iad", VendorKind::Fly, "tok", "iad").await.unwrap();
    assert!(h.services.runtime_vendors.read().unwrap().contains_key("fly-iad"));
}

#[tokio::test]
async fn deleting_a_vendor_removes_it_from_the_map() {
    let h = config_harness().await;
    h.save_runtime_vendor("fly-iad", VendorKind::Fly, "tok", "iad").await.unwrap();
    h.delete_runtime_vendor("fly-iad").await.unwrap();
    assert!(!h.services.runtime_vendors.read().unwrap().contains_key("fly-iad"));
}

#[tokio::test]
async fn a_localhost_callback_url_is_refused_at_save_time() {
    // The string ends up in the sandbox's argv. Failing here beats failing at
    // the first session, when the error surfaces as "never dialed back".
    let h = config_harness().await;
    let err = h.save_runtime_vendor_with_callback("fly", "http://localhost:3789").await.unwrap_err();
    assert!(err.contains("reachable"));
}
```

- [ ] **Step 2: Run, implement, run**

Migration adds `runtime_vendors(user_id, name, kind, credential, settings_json, callback_url)` with a `(user_id, name)` primary key. `DbConfigStore::open` reads the rows and publishes an `InProcessRuntimeVendor` per row; `update` rebuilds affected entries in a `begin_write` transaction.

- [ ] **Step 3: Full gate, then commit**

```bash
git add -A && git commit -m "feat: runtime vendors configured per account"
```

---

### Task 8: Settings UI

**Files:**
- Modify: `clients/web/src/pages/settings/*` — a Runtime Vendors section
- Modify: `clients/web/tests/` — one Playwright spec

- [ ] **Step 1: Add the section**

A list of configured vendors with add/edit/delete, mirroring the existing Providers section. Fields: name, kind, API token (write-only), region, image, callback URL.

- [ ] **Step 2: Verify**

Run: `cd clients/web && bun run generate-types && bun run build && TMPDIR=/tmp bun run test:e2e`
Expected: PASS. `TMPDIR=/tmp` is required — Playwright's global setup dies under the default macOS `$TMPDIR` on the unix-socket path length limit.

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "feat: configure runtime vendors from settings"
```

---

## Follow-up plan

Steps 5–8 of the spec — `FlyRuntimeProvider` with volumes, the runtime reconnect loop, the velos port, and the orphan sweep — get their own plan once this one has landed and the seam has run.
