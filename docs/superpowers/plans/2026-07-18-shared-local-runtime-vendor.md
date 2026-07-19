# Shared Local Runtime Vendor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `LocalProcessVendor` (server-spawned process, managed/host-dir workspace, `git_checkout` provisioning) with a shared local runtime vendor: a user-launched `horsie-runtime` daemon that dials back over WS, fixed to whatever directory it was started in, shareable by any number of concurrent sessions.

**Architecture:** Every connected daemon registers itself as its own named `RuntimeVendor` entry in the existing `ServerDeps.vendors` map (keyed by a caller-chosen label), reusing the velos vendor's reverse-dial listener/`ConnectedRuntimeRegistry` plumbing unchanged except for two additive seams in the `executor` crate: a collision guard (reject a duplicate live label instead of silently evicting it) and an on-registration hook (so the server-crate vendor module learns about a newly (re)connected label to mirror into `ServerDeps.vendors`). `create`/`attach` never spawn anything — they look up the already-live transport and return a client wrapping it; `stop`/`delete` are no-ops since no single session owns the daemon.

**Tech Stack:** Rust workspace (`executor`, `server`, `cli`, `runtime`, `models` crates), tokio, `tokio-tungstenite`, fluorite codegen (`models/fluorite/*.fl` → generated Rust via `models/build.rs`, no manual step).

## Global Constraints

- Every step that changes code shows the exact code — no "add error handling" placeholders.
- Run `cargo test -p <crate>` (or the broader commands each task specifies) before moving to the next task; do not proceed past a failing test.
- Follow the repo's existing `#[cfg(test)] mod tests` + `#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::wildcard_enum_match_arm)]` convention for every new test module (matches `server/src/vendor/velos.rs`, `executor/src/connected_registry.rs`).
- Never commit with `--no-verify`; if a pre-commit hook fails, fix the underlying issue and make a new commit.
- Out of scope for this plan (call out explicitly in the final PR description, do not silently drop): a "list known local instances" HTTP endpoint for a session-creation picker, and a dedicated ergonomic CLI subcommand wrapping `horsie-runtime`'s dial-out flags (the existing `horsie-runtime --endpoint ws://<host>:<port> --runtime-id <label> --workspace main=<dir>` invocation is already fully sufficient and requires no code change — no `--sandbox-caps` flag means no sandboxing, exactly matching this vendor's unsandboxed-by-design requirement).

---

## Task 1: Executor-crate plumbing — collision-safe registration, connect hook, `workdir` on `Ready`

**Files:**
- Modify: `executor/src/connected_registry.rs`
- Modify: `executor/src/executor.rs`
- Modify: `executor/src/lib.rs`
- Modify: `models/fluorite/runtime.fl`
- Modify: `runtime/src/main.rs`
- Modify: `server/src/vendor/velos.rs` (test fixture only — `RuntimeReady` gains a field)

**Interfaces:**
- Produces: `ConnectedRuntimeRegistry::try_register_transport(&self, runtime_id: String, transport: Arc<dyn RuntimeTransport>) -> bool` (registers only if `runtime_id` isn't already live; `false` on collision, existing transport untouched).
- Produces: `pub type ConnectHook = Arc<dyn Fn(String, String) + Send + Sync>;` (called `(runtime_id, workdir)` after a successful registration).
- Produces: `pub fn serve_runtime_connections_with_hook(listener: RuntimeListenerServer, registry: Arc<ConnectedRuntimeRegistry>, cancel: CancellationToken, on_registered: Option<ConnectHook>)`. `serve_runtime_connections` becomes a thin wrapper calling this with `None` — its existing call sites (`velos.rs`, `executor.rs`'s own CLI-mode `Executor`) are unaffected.
- Produces: `horsie_models::runtime::RuntimeReady { runtime_id: String, workdir: String }` (new `workdir` field; both existing constructors updated in this task).
- Consumes (Task 2 depends on these): `ConnectHook`, `serve_runtime_connections_with_hook`, `ConnectedRuntimeRegistry::try_register_transport`, `RuntimeReady.workdir`.

- [ ] **Step 1: Add `try_register_transport` to `ConnectedRuntimeRegistry`, with a failing-then-passing test**

Add this method to `impl ConnectedRuntimeRegistry` in `executor/src/connected_registry.rs`, right after the existing `register_transport`:

```rust
    /// Register a runtime's tool transport only if `runtime_id` isn't already
    /// live. Returns `false` (leaving the existing transport untouched) on a
    /// collision — used by vendors whose announced id is a caller-chosen
    /// label that could collide (unlike the unique per-attempt ids other
    /// vendors mint).
    pub async fn try_register_transport(
        &self,
        runtime_id: String,
        transport: Arc<dyn RuntimeTransport>,
    ) -> bool {
        let mut inner = self.inner.lock().await;
        if inner.transports.contains_key(&runtime_id) {
            return false;
        }
        inner.transports.insert(runtime_id.clone(), transport);
        if let Some(tx) = inner.pending.remove(&runtime_id) {
            let _ = tx.send(Ok(()));
        }
        true
    }
```

Add this test to the existing `#[cfg(test)] mod tests` block at the bottom of the same file (after `register_resolves_pending_waiter_and_stores_transport`):

```rust
    #[tokio::test]
    async fn try_register_transport_rejects_a_live_collision() {
        let reg = ConnectedRuntimeRegistry::new();
        let first: Arc<dyn RuntimeTransport> = Arc::new(MockTransport::ok("first"));
        assert!(
            reg.try_register_transport("rt-1".into(), first.clone())
                .await
        );
        let second: Arc<dyn RuntimeTransport> = Arc::new(MockTransport::ok("second"));
        assert!(
            !reg.try_register_transport("rt-1".into(), second).await,
            "a live collision must be rejected"
        );
        let still_registered = reg.runtime_transport("rt-1").await.expect("still registered");
        assert!(
            Arc::ptr_eq(&first, &still_registered),
            "collision must not disturb the original transport"
        );
    }
```

This only assumes `MockTransport::ok(...)` constructs a `RuntimeTransport` (already used by the existing test above it in this file) and distinguishes the surviving transport by `Arc` identity rather than by invoking it, so it needs no further assumptions about `MockTransport`'s API.

- [ ] **Step 2: Run the new test**

Run: `cargo test -p horsie-executor try_register_transport_rejects_a_live_collision -- --nocapture`
Expected: PASS (the method is straightforward; this mainly guards against a future accidental change to `try_register_transport`'s collision semantics).

- [ ] **Step 3: Commit**

```bash
git add executor/src/connected_registry.rs
git commit -m "executor: add collision-safe try_register_transport"
```

- [ ] **Step 4: Add `workdir` to `RuntimeReady` in the fluorite schema**

Edit `models/fluorite/runtime.fl`, changing:

```
/// First message the runtime sends after connecting.
struct RuntimeReady { runtime_id: String }
```

to:

```
/// First message the runtime sends after connecting. `workdir` is the
/// runtime's primary workspace path, reported for display (e.g. a shared
/// local vendor's connected-instance listing) — every invocation always has
/// at least one workspace, so this is never empty.
struct RuntimeReady { runtime_id: String, workdir: String }
```

- [ ] **Step 5: Update both existing `RuntimeReady` constructors to populate `workdir`**

In `runtime/src/main.rs`, the `run()` function currently moves `cli.workspaces` into `WorkspaceRegistry::new(cli.workspaces)` before `run_loop` is called, so capture the first workspace's path *before* that move. Change:

```rust
    let registry = Arc::new(
        horsie_runtime::workspace::WorkspaceRegistry::new(cli.workspaces)
            .with_plugins(plugins_dir, cli.hook_path),
    );

    match endpoint {
        Endpoint::Ws(url) => match connect_async(&url).await {
            Ok((ws, _)) => run_loop(ws, registry, cli.runtime_id, steps).await,
            Err(e) => {
                eprintln!("failed to connect to {url}: {e}");
                std::process::exit(1);
            }
        },
        Endpoint::Unix(path) => match tokio::net::UnixStream::connect(&path).await {
            Ok(stream) => match client_async("ws://localhost/", stream).await {
                Ok((ws, _)) => run_loop(ws, registry, cli.runtime_id, steps).await,
                Err(e) => {
                    eprintln!("ws handshake failed on unix socket: {e}");
                    std::process::exit(1);
                }
            },
            Err(e) => {
                eprintln!("failed to connect to unix socket {}: {e}", path.display());
                std::process::exit(1);
            }
        },
    }
```

to:

```rust
    let workdir = cli
        .workspaces
        .first()
        .map(|w| w.path.display().to_string())
        .unwrap_or_default();
    let registry = Arc::new(
        horsie_runtime::workspace::WorkspaceRegistry::new(cli.workspaces)
            .with_plugins(plugins_dir, cli.hook_path),
    );

    match endpoint {
        Endpoint::Ws(url) => match connect_async(&url).await {
            Ok((ws, _)) => run_loop(ws, registry, cli.runtime_id, steps, workdir).await,
            Err(e) => {
                eprintln!("failed to connect to {url}: {e}");
                std::process::exit(1);
            }
        },
        Endpoint::Unix(path) => match tokio::net::UnixStream::connect(&path).await {
            Ok(stream) => match client_async("ws://localhost/", stream).await {
                Ok((ws, _)) => run_loop(ws, registry, cli.runtime_id, steps, workdir).await,
                Err(e) => {
                    eprintln!("ws handshake failed on unix socket: {e}");
                    std::process::exit(1);
                }
            },
            Err(e) => {
                eprintln!("failed to connect to unix socket {}: {e}", path.display());
                std::process::exit(1);
            }
        },
    }
```

Then update `run_loop`'s signature and its `RuntimeReady` construction. Change:

```rust
async fn run_loop<S>(
    ws: WebSocketStream<S>,
    registry: Arc<horsie_runtime::workspace::WorkspaceRegistry>,
    runtime_id: String,
    steps: Vec<horsie_models::executor::ProvisionStep>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
```

to:

```rust
async fn run_loop<S>(
    ws: WebSocketStream<S>,
    registry: Arc<horsie_runtime::workspace::WorkspaceRegistry>,
    runtime_id: String,
    steps: Vec<horsie_models::executor::ProvisionStep>,
    workdir: String,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
```

And change:

```rust
    let ready = match serde_json::to_string(&RuntimeOutboundMessage::Ready(RuntimeReady {
        runtime_id: runtime_id.clone(),
    })) {
```

to:

```rust
    let ready = match serde_json::to_string(&RuntimeOutboundMessage::Ready(RuntimeReady {
        runtime_id: runtime_id.clone(),
        workdir,
    })) {
```

In `server/src/vendor/velos.rs`, inside the test module's `fake_runtime` function, change:

```rust
        let ready =
            serde_json::to_string(&RuntimeOutboundMessage::Ready(RuntimeReady { runtime_id }))
                .unwrap();
```

to:

```rust
        let ready = serde_json::to_string(&RuntimeOutboundMessage::Ready(RuntimeReady {
            runtime_id,
            workdir: "/workspace".to_string(),
        }))
        .unwrap();
```

- [ ] **Step 6: Build to confirm the schema regenerates and both call sites compile**

Run: `cargo build -p horsie-models -p horsie-runtime -p horsie-server`
Expected: builds cleanly (fluorite codegen runs automatically via `models/build.rs` on `cargo build`; no manual generation step).

- [ ] **Step 7: Run the runtime crate's existing tests plus the velos vendor tests to confirm nothing broke**

Run: `cargo test -p horsie-runtime && cargo test -p horsie-server --lib vendor::velos`
Expected: PASS (all previously-passing tests, unaffected by the additive field).

- [ ] **Step 8: Commit**

```bash
git add models/fluorite/runtime.fl runtime/src/main.rs server/src/vendor/velos.rs
git commit -m "runtime: report primary workspace path on Ready"
```

- [ ] **Step 9: Add the collision guard and on-registration hook to `handle_runtime_connection`, plus `serve_runtime_connections_with_hook`**

In `executor/src/executor.rs`, add a type alias near the top (after the existing `type WsSink = ...` block):

```rust
/// Fires `(runtime_id, workdir)` after a runtime successfully registers (not
/// on a rejected collision). Lets a vendor that registers runtimes outside
/// any `create`/`attach` call (e.g. a user-launched daemon dialing in on its
/// own) learn about a newly (re)connected id without polling.
pub type ConnectHook = Arc<dyn Fn(String, String) + Send + Sync>;
```

Change the `serve_runtime_connections` function:

```rust
pub fn serve_runtime_connections(
    listener: RuntimeListenerServer,
    registry: Arc<ConnectedRuntimeRegistry>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                result = listener.accept() => match result {
                    Ok(AcceptedConn::Tcp(ws)) => {
                        tokio::spawn(handle_runtime_connection(ws, registry.clone()));
                    }
                    Ok(AcceptedConn::Unix(ws)) => {
                        tokio::spawn(handle_runtime_connection(ws, registry.clone()));
                    }
                    Err(_) => break,
                }
            }
        }
        // Dropping `listener` here unlinks the unix socket (its Drop impl).
    });
}
```

to:

```rust
pub fn serve_runtime_connections(
    listener: RuntimeListenerServer,
    registry: Arc<ConnectedRuntimeRegistry>,
    cancel: CancellationToken,
) {
    serve_runtime_connections_with_hook(listener, registry, cancel, None)
}

/// Like [`serve_runtime_connections`], but `on_registered` (if given) fires
/// after each successful registration with `(runtime_id, workdir)`.
pub fn serve_runtime_connections_with_hook(
    listener: RuntimeListenerServer,
    registry: Arc<ConnectedRuntimeRegistry>,
    cancel: CancellationToken,
    on_registered: Option<ConnectHook>,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                result = listener.accept() => match result {
                    Ok(AcceptedConn::Tcp(ws)) => {
                        tokio::spawn(handle_runtime_connection(
                            ws,
                            registry.clone(),
                            on_registered.clone(),
                        ));
                    }
                    Ok(AcceptedConn::Unix(ws)) => {
                        tokio::spawn(handle_runtime_connection(
                            ws,
                            registry.clone(),
                            on_registered.clone(),
                        ));
                    }
                    Err(_) => break,
                }
            }
        }
        // Dropping `listener` here unlinks the unix socket (its Drop impl).
    });
}
```

Now update `handle_runtime_connection` itself. Change:

```rust
async fn handle_runtime_connection<S>(
    ws: tokio_tungstenite::WebSocketStream<S>,
    registry: Arc<ConnectedRuntimeRegistry>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (sink, mut stream) = ws.split();

    enum Handshake {
        Ready(String),
        Provisioning(String),
    }

    // First message must arrive within a bounded window so a peer that connects
    // but never announces itself can't leak this task forever.
    let first = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match stream.next().await {
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<RuntimeOutboundMessage>(&text) {
                        Ok(RuntimeOutboundMessage::Ready(ev)) => {
                            return Some(Handshake::Ready(ev.runtime_id));
                        }
                        Ok(RuntimeOutboundMessage::Provisioning(ev)) => {
                            return Some(Handshake::Provisioning(ev.runtime_id));
                        }
                        _ => {}
                    }
                }
                _ => return None,
            }
        }
    })
    .await;

    let runtime_id = match first {
        Ok(Some(Handshake::Ready(id))) => id,
        Ok(Some(Handshake::Provisioning(id))) => {
            // Provision phase: wait (much longer) for Ready or ProvisionFailed.
            let outcome = tokio::time::timeout(PROVISION_WINDOW, async {
                loop {
                    match stream.next().await {
                        Some(Ok(Message::Text(text))) => {
                            match serde_json::from_str::<RuntimeOutboundMessage>(&text) {
                                Ok(RuntimeOutboundMessage::Ready(ev)) => {
                                    return Ok(ev.runtime_id);
                                }
                                Ok(RuntimeOutboundMessage::ProvisionFailed(ev)) => {
                                    return Err(ev.message);
                                }
                                _ => {}
                            }
                        }
                        _ => return Err("runtime disconnected during provisioning".to_string()),
                    }
                }
            })
            .await;
            match outcome {
                Ok(Ok(ready_id)) => ready_id,
                Ok(Err(message)) => {
                    registry.fail_pending(&id, message).await;
                    return;
                }
                Err(_) => {
                    registry
                        .fail_pending(&id, "timed out during provisioning".to_string())
                        .await;
                    return;
                }
            }
        }
        // Timed out, stream closed, or garbage before an announce — drop the link.
        Ok(None) | Err(_) => return,
    };

    let (transport, closed) = SocketRuntimeTransport::from_split(sink, stream);
    registry
        .register_transport(runtime_id.clone(), Arc::new(transport))
        .await;
    // Deregister when the link drops so health checks observe the loss and a stale
    // transport never lingers (explicit destroy also removes it; double-remove is safe).
    let _ = closed.await;
    registry.remove(&runtime_id).await;
}
```

to:

```rust
async fn handle_runtime_connection<S>(
    ws: tokio_tungstenite::WebSocketStream<S>,
    registry: Arc<ConnectedRuntimeRegistry>,
    on_registered: Option<ConnectHook>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (sink, mut stream) = ws.split();

    enum Handshake {
        Ready(String, String),
        Provisioning(String),
    }

    // First message must arrive within a bounded window so a peer that connects
    // but never announces itself can't leak this task forever.
    let first = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match stream.next().await {
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<RuntimeOutboundMessage>(&text) {
                        Ok(RuntimeOutboundMessage::Ready(ev)) => {
                            return Some(Handshake::Ready(ev.runtime_id, ev.workdir));
                        }
                        Ok(RuntimeOutboundMessage::Provisioning(ev)) => {
                            return Some(Handshake::Provisioning(ev.runtime_id));
                        }
                        _ => {}
                    }
                }
                _ => return None,
            }
        }
    })
    .await;

    let (runtime_id, workdir) = match first {
        Ok(Some(Handshake::Ready(id, workdir))) => (id, workdir),
        Ok(Some(Handshake::Provisioning(id))) => {
            // Provision phase: wait (much longer) for Ready or ProvisionFailed.
            let outcome = tokio::time::timeout(PROVISION_WINDOW, async {
                loop {
                    match stream.next().await {
                        Some(Ok(Message::Text(text))) => {
                            match serde_json::from_str::<RuntimeOutboundMessage>(&text) {
                                Ok(RuntimeOutboundMessage::Ready(ev)) => {
                                    return Ok((ev.runtime_id, ev.workdir));
                                }
                                Ok(RuntimeOutboundMessage::ProvisionFailed(ev)) => {
                                    return Err(ev.message);
                                }
                                _ => {}
                            }
                        }
                        _ => return Err("runtime disconnected during provisioning".to_string()),
                    }
                }
            })
            .await;
            match outcome {
                Ok(Ok(ready)) => ready,
                Ok(Err(message)) => {
                    registry.fail_pending(&id, message).await;
                    return;
                }
                Err(_) => {
                    registry
                        .fail_pending(&id, "timed out during provisioning".to_string())
                        .await;
                    return;
                }
            }
        }
        // Timed out, stream closed, or garbage before an announce — drop the link.
        Ok(None) | Err(_) => return,
    };

    // Check BEFORE building the transport: `SocketRuntimeTransport::from_split`
    // unconditionally spawns a reader task that owns `stream` until the
    // socket itself closes, so rejecting *after* building it would leak that
    // task (dropping the transport handle alone doesn't stop it). A cheap
    // pre-check here means the common case (a duplicate label dialing in
    // well after the first is registered) drops `sink`/`stream` directly —
    // no task ever spawned, socket closes immediately.
    if registry.runtime_transport(&runtime_id).await.is_some() {
        return;
    }
    let (transport, closed) = SocketRuntimeTransport::from_split(sink, stream);
    if !registry
        .try_register_transport(runtime_id.clone(), Arc::new(transport))
        .await
    {
        // The narrow remaining race (two connections announcing the same id
        // within the same instant, both passing the check above before
        // either registers): `try_register_transport` is still the atomic
        // source of truth, so the loser is never reachable via
        // `runtime_transport()` — correctness holds. Its reader task isn't
        // proactively closed here, but it's inert (nothing will ever poll
        // it) and exits on its own once its peer disconnects.
        return;
    }
    if let Some(hook) = &on_registered {
        hook(runtime_id.clone(), workdir);
    }
    // Deregister when the link drops so health checks observe the loss and a stale
    // transport never lingers (explicit destroy also removes it; double-remove is safe).
    let _ = closed.await;
    registry.remove(&runtime_id).await;
}
```

- [ ] **Step 10: Export the new items from the crate root**

In `executor/src/lib.rs`, change:

```rust
pub use executor::{Executor, serve_runtime_connections};
```

to:

```rust
pub use executor::{ConnectHook, Executor, serve_runtime_connections, serve_runtime_connections_with_hook};
```

- [ ] **Step 11: Write a new executor-crate test proving the collision guard and hook work end to end**

Add a new `#[cfg(test)] mod tests` block at the bottom of `executor/src/executor.rs` (this file currently has no test module):

```rust
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use crate::runtime_listener::RuntimeEndpoint;
    use futures_util::SinkExt;
    use horsie_models::runtime::RuntimeReady;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration as StdDuration;
    use tokio_tungstenite::connect_async;

    async fn announce(addr: std::net::SocketAddr, runtime_id: &str, workdir: &str) -> WsSinkPair {
        let (ws, _) = connect_async(format!("ws://{addr}")).await.expect("connect");
        let (mut sink, stream) = ws.split();
        let ready = serde_json::to_string(&RuntimeOutboundMessage::Ready(RuntimeReady {
            runtime_id: runtime_id.to_string(),
            workdir: workdir.to_string(),
        }))
        .unwrap();
        sink.send(Message::Text(ready.into())).await.unwrap();
        (sink, stream)
    }

    type WsSinkPair = (
        futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            Message,
        >,
        futures_util::stream::SplitStream<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
        >,
    );

    async fn wait_registered(registry: &ConnectedRuntimeRegistry, id: &str) {
        for _ in 0..50 {
            if registry.runtime_transport(id).await.is_some() {
                return;
            }
            tokio::time::sleep(StdDuration::from_millis(20)).await;
        }
        panic!("'{id}' never registered within 1s");
    }

    #[tokio::test]
    async fn duplicate_runtime_id_is_rejected_without_disturbing_the_live_one() {
        let listener = RuntimeListenerServer::bind(RuntimeEndpoint::Tcp("127.0.0.1:0".parse().unwrap()))
            .await
            .unwrap();
        let addr = listener.tcp_addr().unwrap();
        let registry = Arc::new(ConnectedRuntimeRegistry::new());
        let cancel = CancellationToken::new();
        serve_runtime_connections(listener, registry.clone(), cancel.clone());

        let (_sink1, _stream1) = announce(addr, "dup-id", "/one").await;
        wait_registered(&registry, "dup-id").await;

        // A second connection announcing the SAME id must be rejected: its
        // socket closes, and the first transport stays registered.
        let (mut sink2, mut stream2) = announce(addr, "dup-id", "/two").await;
        let closed = tokio::time::timeout(StdDuration::from_secs(2), stream2.next()).await;
        assert!(
            matches!(closed, Ok(None) | Ok(Some(Err(_)))),
            "expected the duplicate connection to be closed, got {closed:?}"
        );
        let _ = sink2.close().await;
        assert!(
            registry.runtime_transport("dup-id").await.is_some(),
            "the original transport must still be registered"
        );
        cancel.cancel();
    }

    #[tokio::test]
    async fn on_registered_hook_fires_with_id_and_workdir_once_per_registration() {
        let listener = RuntimeListenerServer::bind(RuntimeEndpoint::Tcp("127.0.0.1:0".parse().unwrap()))
            .await
            .unwrap();
        let addr = listener.tcp_addr().unwrap();
        let registry = Arc::new(ConnectedRuntimeRegistry::new());
        let cancel = CancellationToken::new();
        let seen: Arc<StdMutex<Vec<(String, String)>>> = Arc::new(StdMutex::new(Vec::new()));
        let hook_seen = seen.clone();
        let hook: ConnectHook = Arc::new(move |id: String, workdir: String| {
            hook_seen.lock().unwrap().push((id, workdir));
        });
        serve_runtime_connections_with_hook(listener, registry.clone(), cancel.clone(), Some(hook));

        let (_sink, _stream) = announce(addr, "rt-1", "/proj").await;
        wait_registered(&registry, "rt-1").await;

        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &[("rt-1".to_string(), "/proj".to_string())]
        );
        cancel.cancel();
    }
}
```

- [ ] **Step 12: Run the new executor tests**

Run: `cargo test -p horsie-executor`
Expected: PASS, including the two new tests from Step 11 and the one from Step 1.

- [ ] **Step 13: Run the full existing test suite for crates touched so far to confirm no regressions**

Run: `cargo test -p horsie-executor -p horsie-runtime -p horsie-models -p horsie-server --lib vendor::velos`
Expected: PASS.

- [ ] **Step 14: Commit**

```bash
git add executor/src/executor.rs executor/src/lib.rs
git commit -m "executor: collision-guarded registration and on-connect hook"
```

---

## Task 2: Shared local runtime vendor (`LocalDaemonVendor` + `LocalDaemonRegistry`)

**Files:**
- Modify (full rewrite of contents): `server/src/vendor/local.rs`
- Modify: `server/src/vendor/mod.rs`
- Modify: `server/src/vendor/velos.rs` (doc comment only, line 4)

**Interfaces:**
- Consumes: `horsie_executor::{ConnectHook, ConnectedRuntimeRegistry, RuntimeEndpoint, RuntimeListenerServer, serve_runtime_connections_with_hook}` (Task 1), `crate::sessions::spec::SharedVendors`, `crate::vendor::{RuntimeSpec, RuntimeVendor, VendorError, VendorRuntime, VendorRuntimeHandle, WorkspaceSource}`.
- Produces: `pub struct LocalDaemonVendor` (implements `RuntimeVendor`, `name() == "local"`, exposes `pub fn workdir(&self) -> String`), `pub struct LocalDaemonRegistry` with `pub async fn bind(listen: SocketAddr, vendors: SharedVendors) -> Result<Self, VendorError>` and `pub fn listen_addr(&self) -> SocketAddr` — both consumed by Task 3.

- [ ] **Step 1: Replace the entire contents of `server/src/vendor/local.rs`**

Delete the current contents (the whole `LocalProcessVendor` implementation and its tests) and replace with:

```rust
//! Runtime vendor backed by a user-launched daemon dialing back over a
//! shared TCP listener, fixed to whatever directory it was started in.
//!
//! Unlike every other vendor, a connected daemon isn't created or owned by
//! any session: it registers itself under a caller-chosen label the moment
//! it dials in (see [`LocalDaemonRegistry::bind`]), and any number of
//! sessions may subsequently `create`/`attach` against that same label
//! concurrently, sharing the one live connection. That's safe — the wire
//! protocol already correlates concurrent calls by `call_id`, not by
//! connection order, the same mechanism a single session's parallel tool
//! calls already exercise. `stop`/`delete` are no-ops: the daemon isn't
//! owned by any one session, so halting or deleting a session must never
//! disturb others sharing the label. No provisioning (no `git_checkout`)
//! and no sandboxing — the directory and the machine are already the
//! user's own.

use crate::sessions::spec::SharedVendors;
use crate::vendor::{
    RuntimeSpec, RuntimeVendor, VendorError, VendorRuntime, VendorRuntimeHandle, WorkspaceSource,
};
use async_trait::async_trait;
use horsie_executor::{
    ConnectHook, ConnectedRuntimeRegistry, RuntimeEndpoint, RuntimeListenerServer,
    serve_runtime_connections_with_hook,
};
use horsie_runtime_client::RuntimeClient;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use tokio_util::sync::{CancellationToken, DropGuard};

/// One connected daemon's vendor identity. Never spawns anything: `create`/
/// `attach` look up whatever's currently registered for `label` in the
/// shared [`ConnectedRuntimeRegistry`] and hand back a client wrapping it.
pub struct LocalDaemonVendor {
    label: String,
    connected: Arc<ConnectedRuntimeRegistry>,
    workdir: RwLock<String>,
}

impl LocalDaemonVendor {
    /// The directory the connected daemon reported at its last (re)connect.
    pub fn workdir(&self) -> String {
        self.workdir
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn set_workdir(&self, workdir: String) {
        *self.workdir.write().unwrap_or_else(|e| e.into_inner()) = workdir;
    }

    /// Reject inputs this vendor can't honor: it never provisions (no
    /// `git_checkout`) and never resolves a caller-supplied host path (the
    /// daemon's own directory is implicit and fixed). The common case — no
    /// `workdirs`/`repos` in the request — produces one `Managed` workspace
    /// with no provision steps, which this vendor silently ignores instead
    /// of rejecting (that's exactly "just use the daemon's own dir").
    fn reject_unsupported_inputs(spec: &RuntimeSpec) -> Result<(), String> {
        if !spec.provision.is_empty() {
            return Err(
                "shared local runtime vendor does not support repo provisioning".to_string(),
            );
        }
        if spec
            .workspaces
            .iter()
            .any(|w| matches!(w.source, WorkspaceSource::HostDir(_)))
        {
            return Err(
                "shared local runtime vendor ignores workdirs; sessions use the connected \
                 daemon's own directory"
                    .to_string(),
            );
        }
        Ok(())
    }

    async fn resolve(
        &self,
        spec: &RuntimeSpec,
        attach: bool,
    ) -> Result<VendorRuntime, VendorError> {
        let wrap = |e: String| {
            if attach {
                VendorError::Attach(e)
            } else {
                VendorError::Provision(e)
            }
        };
        Self::reject_unsupported_inputs(spec).map_err(wrap)?;
        let transport = self
            .connected
            .runtime_transport(&self.label)
            .await
            .ok_or_else(|| {
                wrap(format!(
                    "local runtime '{}' is not currently connected",
                    self.label
                ))
            })?;
        Ok(VendorRuntime {
            runtime_client: RuntimeClient::from_arc(transport),
            handle: Arc::new(NoopHandle),
        })
    }
}

#[async_trait]
impl RuntimeVendor for LocalDaemonVendor {
    fn name(&self) -> &'static str {
        "local"
    }

    async fn create(
        &self,
        _runtime_id: &str,
        spec: &RuntimeSpec,
    ) -> Result<VendorRuntime, VendorError> {
        self.resolve(spec, false).await
    }

    async fn attach(
        &self,
        _runtime_id: &str,
        spec: &RuntimeSpec,
    ) -> Result<VendorRuntime, VendorError> {
        self.resolve(spec, true).await
    }

    async fn delete(&self, _runtime_id: &str) {
        // No-op: the vendor never created the daemon or its directory, so it
        // has nothing to reclaim, and other sessions may still be using it.
    }
}

/// Lifecycle handle for one session's use of a shared daemon. `stop` is a
/// no-op — halting one session must never disturb others sharing the label.
struct NoopHandle;

#[async_trait]
impl VendorRuntimeHandle for NoopHandle {
    async fn stop(&self) {}
}

/// Binds the shared reverse-dial listener every "local" daemon connects to,
/// and mirrors each newly (or re-)connected label into `ServerDeps.vendors`
/// so sessions can select it by name exactly like any other vendor.
pub struct LocalDaemonRegistry {
    connected: Arc<ConnectedRuntimeRegistry>,
    local_vendors: Arc<RwLock<HashMap<String, Arc<LocalDaemonVendor>>>>,
    listen_addr: SocketAddr,
    _serve_guard: DropGuard,
}

impl LocalDaemonRegistry {
    /// Bind the listener and start accepting daemon connections. `vendors`
    /// is the same map session lookups read (`ServerDeps.vendors`) — every
    /// connected label is inserted into it as it announces itself.
    pub async fn bind(listen: SocketAddr, vendors: SharedVendors) -> Result<Self, VendorError> {
        let listener = RuntimeListenerServer::bind(RuntimeEndpoint::Tcp(listen))
            .await
            .map_err(|e| VendorError::Provision(format!("local daemon listener: {e}")))?;
        let listen_addr = listener.tcp_addr().ok_or_else(|| {
            VendorError::Provision("local daemon vendor requires a TCP listener".into())
        })?;
        let connected = Arc::new(ConnectedRuntimeRegistry::new());
        let local_vendors: Arc<RwLock<HashMap<String, Arc<LocalDaemonVendor>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let cancel = CancellationToken::new();

        let hook_connected = connected.clone();
        let hook_local_vendors = local_vendors.clone();
        let hook_vendors = vendors;
        let hook: ConnectHook = Arc::new(move |label: String, workdir: String| {
            let vendor = {
                let mut locals = hook_local_vendors
                    .write()
                    .unwrap_or_else(|e| e.into_inner());
                locals
                    .entry(label.clone())
                    .or_insert_with(|| {
                        Arc::new(LocalDaemonVendor {
                            label: label.clone(),
                            connected: hook_connected.clone(),
                            workdir: RwLock::new(String::new()),
                        })
                    })
                    .clone()
            };
            vendor.set_workdir(workdir);
            let mut all = hook_vendors.write().unwrap_or_else(|e| e.into_inner());
            all.entry(label)
                .or_insert_with(|| vendor.clone() as Arc<dyn RuntimeVendor>);
        });

        serve_runtime_connections_with_hook(
            listener,
            connected.clone(),
            cancel.clone(),
            Some(hook),
        );

        Ok(Self {
            connected,
            local_vendors,
            listen_addr,
            _serve_guard: cancel.drop_guard(),
        })
    }

    /// The bound address, e.g. for logging or (in tests) dialing a fake daemon in.
    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    /// The label's vendor object, if a daemon has ever announced it (whether
    /// currently connected or not).
    #[cfg(test)]
    fn vendor(&self, label: &str) -> Option<Arc<LocalDaemonVendor>> {
        self.local_vendors
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(label)
            .cloned()
    }

    #[cfg(test)]
    async fn is_connected(&self, label: &str) -> bool {
        self.connected.runtime_transport(label).await.is_some()
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use crate::vendor::WorkspaceSpec;
    use futures_util::{SinkExt, StreamExt};
    use horsie_models::runtime::{
        BashInput, RuntimeInboundMessage, RuntimeOutboundMessage, RuntimeReady, ToolCall,
        ToolCallResponse, ToolOutput, ToolResult,
    };
    use std::time::Duration;
    use tokio::task::JoinHandle;
    use tokio_tungstenite::{connect_async, tungstenite::Message};

    fn empty_vendors() -> SharedVendors {
        Arc::new(RwLock::new(HashMap::new()))
    }

    fn test_spec() -> RuntimeSpec {
        RuntimeSpec {
            workspaces: vec![WorkspaceSpec {
                name: "main".into(),
                source: WorkspaceSource::Managed,
            }],
            provision: vec![],
            env: vec![],
            capabilities_file: std::env::temp_dir().join("caps.json"),
            plugins_dir: None,
            hook_path: vec![],
        }
    }

    /// A fake `horsie-runtime --endpoint ws://... --runtime-id <label>`
    /// daemon: dials in, announces Ready under `label`, then answers every
    /// tool call with a fixed stdout so tests can tell which daemon actually
    /// served a call.
    fn spawn_fake_daemon(
        addr: SocketAddr,
        label: String,
        workdir: String,
        reply: String,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let (ws, _) = match connect_async(format!("ws://{addr}")).await {
                Ok(x) => x,
                Err(_) => return,
            };
            let (mut sink, mut stream) = ws.split();
            let ready = serde_json::to_string(&RuntimeOutboundMessage::Ready(RuntimeReady {
                runtime_id: label,
                workdir,
            }))
            .unwrap();
            if sink.send(Message::Text(ready.into())).await.is_err() {
                return;
            }
            while let Some(Ok(msg)) = stream.next().await {
                if let Message::Text(text) = msg
                    && let Ok(RuntimeInboundMessage::ToolCall(req)) =
                        serde_json::from_str::<RuntimeInboundMessage>(&text)
                {
                    let resp = RuntimeOutboundMessage::ToolCallResponse(ToolCallResponse {
                        call_id: req.call_id,
                        result: ToolResult::Ok(ToolOutput {
                            stdout: reply.clone(),
                            stderr: String::new(),
                            exit_code: 0,
                        }),
                    });
                    if let Ok(json) = serde_json::to_string(&resp) {
                        let _ = sink.send(Message::Text(json.into())).await;
                    }
                }
            }
        })
    }

    async fn bind_registry() -> LocalDaemonRegistry {
        LocalDaemonRegistry::bind("127.0.0.1:0".parse().unwrap(), empty_vendors())
            .await
            .expect("bind")
    }

    async fn wait_connected(registry: &LocalDaemonRegistry, label: &str) {
        for _ in 0..50 {
            if registry.is_connected(label).await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("'{label}' never connected within 1s");
    }

    async fn wait_disconnected(registry: &LocalDaemonRegistry, label: &str) {
        for _ in 0..50 {
            if !registry.is_connected(label).await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("'{label}' never disconnected within 1s");
    }

    fn bash(command: &str) -> ToolCall {
        ToolCall::Bash(BashInput {
            command: command.into(),
            timeout_secs: None,
            workspace: None,
        })
    }

    #[tokio::test]
    async fn connect_registers_label_as_a_vendor() {
        let registry = bind_registry().await;
        let daemon = spawn_fake_daemon(
            registry.listen_addr(),
            "my-laptop".into(),
            "/home/u/proj".into(),
            "ok".into(),
        );
        wait_connected(&registry, "my-laptop").await;
        let vendor = registry.vendor("my-laptop").expect("vendor registered");
        assert_eq!(vendor.workdir(), "/home/u/proj");
        assert_eq!(vendor.name(), "local");
        daemon.abort();
    }

    #[tokio::test]
    async fn create_and_attach_from_different_sessions_share_one_connection() {
        let registry = bind_registry().await;
        let daemon = spawn_fake_daemon(
            registry.listen_addr(),
            "shared".into(),
            "/home/u/proj".into(),
            "shared-ok".into(),
        );
        wait_connected(&registry, "shared").await;
        let vendor = registry.vendor("shared").expect("vendor registered");

        let rt_a = vendor
            .create("session-a", &test_spec())
            .await
            .expect("create a");
        let rt_b = vendor
            .attach("session-b", &test_spec())
            .await
            .expect("attach b");

        let (out_a, out_b) = tokio::join!(
            rt_a.runtime_client.invoke(bash("a")),
            rt_b.runtime_client.invoke(bash("b")),
        );
        assert_eq!(out_a.unwrap().stdout, "shared-ok");
        assert_eq!(out_b.unwrap().stdout, "shared-ok");

        // Stopping/deleting one session must not disturb the other.
        rt_a.handle.stop().await;
        vendor.delete("session-a").await;
        let out_b_again = rt_b
            .runtime_client
            .invoke(bash("still there"))
            .await
            .expect("session b unaffected by session a's stop/delete");
        assert_eq!(out_b_again.stdout, "shared-ok");
        daemon.abort();
    }

    #[tokio::test]
    async fn duplicate_label_is_rejected_and_original_keeps_serving() {
        let registry = bind_registry().await;
        let daemon1 = spawn_fake_daemon(
            registry.listen_addr(),
            "dup".into(),
            "/one".into(),
            "one".into(),
        );
        wait_connected(&registry, "dup").await;
        let daemon2 = spawn_fake_daemon(
            registry.listen_addr(),
            "dup".into(),
            "/two".into(),
            "two".into(),
        );
        tokio::time::sleep(Duration::from_millis(100)).await;

        let vendor = registry.vendor("dup").expect("vendor registered");
        let rt = vendor
            .create("session-x", &test_spec())
            .await
            .expect("create");
        let out = rt
            .runtime_client
            .invoke(bash("x"))
            .await
            .expect("tool call");
        assert_eq!(
            out.stdout, "one",
            "the original daemon must still be the one serving"
        );
        daemon1.abort();
        daemon2.abort();
    }

    #[tokio::test]
    async fn reconnect_under_same_label_resumes_service() {
        let registry = bind_registry().await;
        let daemon1 = spawn_fake_daemon(
            registry.listen_addr(),
            "resumable".into(),
            "/proj".into(),
            "first".into(),
        );
        wait_connected(&registry, "resumable").await;
        let vendor_before = registry.vendor("resumable").expect("vendor registered");

        daemon1.abort();
        wait_disconnected(&registry, "resumable").await;
        assert!(
            vendor_before
                .attach("session-y", &test_spec())
                .await
                .is_err(),
            "attach must fail while disconnected"
        );

        let daemon2 = spawn_fake_daemon(
            registry.listen_addr(),
            "resumable".into(),
            "/proj".into(),
            "second".into(),
        );
        wait_connected(&registry, "resumable").await;
        let vendor_after = registry.vendor("resumable").expect("vendor still registered");
        assert!(
            Arc::ptr_eq(&vendor_before, &vendor_after),
            "vendor object identity must survive a reconnect"
        );
        let rt = vendor_after
            .attach("session-y", &test_spec())
            .await
            .expect("attach after reconnect");
        let out = rt
            .runtime_client
            .invoke(bash("y"))
            .await
            .expect("tool call after reconnect");
        assert_eq!(out.stdout, "second");
        daemon2.abort();
    }

    #[tokio::test]
    async fn rejects_provision_steps_and_host_dir_workspaces() {
        let registry = bind_registry().await;
        let daemon = spawn_fake_daemon(
            registry.listen_addr(),
            "strict".into(),
            "/proj".into(),
            "ok".into(),
        );
        wait_connected(&registry, "strict").await;
        let vendor = registry.vendor("strict").expect("vendor registered");

        let mut with_provision = test_spec();
        with_provision.provision = vec![horsie_models::executor::ProvisionStep {
            name: "clone".into(),
            uses: "git_checkout".into(),
            with: vec![],
        }];
        match vendor.create("session-p", &with_provision).await {
            Err(VendorError::Provision(msg)) => assert!(msg.contains("provisioning"), "{msg}"),
            other => panic!("expected provisioning to be rejected, got {other:?}"),
        }

        let mut with_host_dir = test_spec();
        with_host_dir.workspaces = vec![WorkspaceSpec {
            name: "byo".into(),
            source: WorkspaceSource::HostDir("/home/u/api".into()),
        }];
        match vendor.create("session-h", &with_host_dir).await {
            Err(VendorError::Provision(msg)) => assert!(msg.contains("workdirs"), "{msg}"),
            other => panic!("expected host-dir workspace to be rejected, got {other:?}"),
        }
        daemon.abort();
    }
}
```

- [ ] **Step 2: Update `server/src/vendor/mod.rs` exports**

Change:

```rust
pub use local::LocalProcessVendor;
```

to:

```rust
pub use local::{LocalDaemonRegistry, LocalDaemonVendor};
```

Also update the stale doc comment on `WorkspaceSource::HostDir` (it currently claims local-vendor-only support, which is no longer true — no vendor accepts it now). Change:

```rust
    /// User-supplied host directory (local vendor only).
    HostDir(PathBuf),
```

to:

```rust
    /// User-supplied host directory. No vendor kind currently accepts this
    /// (the shared local vendor ignores the daemon's fixed directory
    /// instead; velos rejects it outright) — kept for a future vendor kind
    /// that can honor it.
    HostDir(PathBuf),
```

- [ ] **Step 3: Fix the stale doc comment in `server/src/vendor/velos.rs`**

Change line 4's reference to the removed type:

```rust
//! Unlike [`crate::vendor::LocalProcessVendor`], which spawns a child process
//! and lets it dial a per-session unix socket, this vendor binds **one shared
```

to:

```rust
//! Unlike [`crate::vendor::LocalDaemonVendor`], which looks up an
//! already-connected daemon rather than spawning anything, this vendor binds
//! **one shared
```

(Adjust surrounding prose only as needed to keep the sentence grammatical — the rest of the module doc comment from `TCP listener...` onward is unchanged.)

- [ ] **Step 4: Confirm the crate's only remaining compile errors are outside `vendor/`**

The `server` crate does not compile yet at this point in the plan — `server/src/config/store.rs` and `server/src/http/mod.rs` still reference `LocalProcessVendor` and the old `StoreDeps` fields (fixed in Task 3), so the new tests in `vendor/local.rs` cannot run yet either.

Run: `cargo build -p horsie-server 2>&1 | grep "error\[" -A2`
Expected: every reported error references `LocalProcessVendor`/`runtime_bin`/`workspace_root`/`public_http_base`, and every one is located in `server/src/config/store.rs` or `server/src/http/mod.rs` — none inside `server/src/vendor/local.rs` or `server/src/vendor/mod.rs`. If an error appears anywhere under `server/src/vendor/`, fix it now before moving on (it means Step 1 or Step 2 has a mistake); the new vendor test suite itself is verified once Task 3 finishes wiring (Task 3 Step 5 runs `cargo test -p horsie-server`, which will include and pass these 5 tests).

- [ ] **Step 5: Commit**

```bash
git add server/src/vendor/local.rs server/src/vendor/mod.rs server/src/vendor/velos.rs
git commit -m "server: replace LocalProcessVendor with shared local daemon vendor"
```

---

## Task 3: Wire the new vendor into config store, CLI config, and `horsie serve`

**Files:**
- Modify: `server/src/config/store.rs`
- Modify: `server/src/http/mod.rs` (test helper only)
- Modify: `cli/src/config.rs`
- Modify: `cli/src/serve.rs`

**Interfaces:**
- Consumes: `crate::vendor::LocalDaemonRegistry` (Task 2), `crate::sessions::spec::SharedVendors`.
- Produces: `StoreDeps { info: ServerInfo, local_runtime_listen: Option<String> }` (fields `runtime_bin`/`workspace_root`/`public_http_base` removed — no remaining consumers after this task). `HorsieConfig.local_runtime_listen: Option<String>` (new deployment config field, parsed by `horsie serve`).

- [ ] **Step 1: Update `StoreDeps` and remove the old local-vendor build path in `server/src/config/store.rs`**

Change the import at the top:

```rust
use crate::vendor::{
    LocalProcessVendor, RuntimeVendor, VelosMutableSettings, VelosVendor, VelosVendorSettings,
};
```

to:

```rust
use crate::vendor::{
    LocalDaemonRegistry, RuntimeVendor, VelosMutableSettings, VelosVendor, VelosVendorSettings,
};
```

Change the `use std::path::{Path, PathBuf};` import — `PathBuf` becomes unused by this task's end (every use was tied to the removed local-vendor fields/tests) and `Path` was only used by the also-removed `local_runtime_available`, so delete the whole line:

```rust
use std::path::{Path, PathBuf};
```

(no replacement — nothing else in this file uses `std::path::Path`/`PathBuf` unqualified; `server/src/config/store.rs`'s test helper at what is currently line 1030 already spells out `std::path::Path` in full).

Change the `StoreDeps` struct:

```rust
/// Deployment inputs the host supplies when opening the store.
pub struct StoreDeps {
    /// `horsie-runtime` binary the built-in `local` vendor spawns.
    pub runtime_bin: PathBuf,
    /// Root under which the built-in `local` vendor allocates managed
    /// workspaces (`<workspace_root>/<runtime_id>/<name>`).
    pub workspace_root: PathBuf,
    /// Read-only deployment paths, surfaced in the settings view.
    pub info: ServerInfo,
    /// Server HTTP base a co-located `local`-vendor runtime fetches plugin
    /// artifacts from (loopback, e.g. `http://127.0.0.1:3789`). `None` disables
    /// local-vendor plugin provisioning.
    pub public_http_base: Option<String>,
}
```

to:

```rust
/// Deployment inputs the host supplies when opening the store.
pub struct StoreDeps {
    /// Read-only deployment paths, surfaced in the settings view.
    pub info: ServerInfo,
    /// Address the shared local-runtime-vendor listener binds. User-launched
    /// `horsie-runtime --endpoint ws://...` daemons dial back here. `None`
    /// disables the shared local vendor entirely (no listener bound, no
    /// `"local"` vendor kind ever registered).
    pub local_runtime_listen: Option<String>,
}
```

Change `DbConfigStore`'s struct to add a field that keeps the listener alive for the store's lifetime:

```rust
pub struct DbConfigStore {
    pool: SqlitePool,
    registry: SharedProviderRegistry,
    default_vendor: RwLock<String>,
    /// Live runtime vendors, kept in sync with the DB by `update()`'s
    /// reconciliation so most vendor edits apply without a restart.
    vendors: SharedVendors,
    /// Concrete handles for vendor kinds that support live reconfigure
    /// (currently only `velos`), keyed by name — lets `update()` call
    /// `.reconfigure()` on the right concrete type without downcasting the
    /// generic `vendors` map.
    velos_instances: RwLock<HashMap<String, Arc<VelosVendor>>>,
    /// Last build/reconfigure failure per vendor name, surfaced on
    /// `VendorView.error`. Cleared when that vendor next builds or
    /// reconfigures successfully.
    vendor_errors: RwLock<HashMap<String, String>>,
    /// Set once an *active* vendor's listener-affecting fields (`listen`/
    /// `advertise_host`/`server_url`) change — that one case still needs a
    /// process restart; never reset within a process's lifetime.
    restart_required: AtomicBool,
    info: ServerInfo,
}
```

to (adding one field at the end):

```rust
pub struct DbConfigStore {
    pool: SqlitePool,
    registry: SharedProviderRegistry,
    default_vendor: RwLock<String>,
    /// Live runtime vendors, kept in sync with the DB by `update()`'s
    /// reconciliation so most vendor edits apply without a restart.
    vendors: SharedVendors,
    /// Concrete handles for vendor kinds that support live reconfigure
    /// (currently only `velos`), keyed by name — lets `update()` call
    /// `.reconfigure()` on the right concrete type without downcasting the
    /// generic `vendors` map.
    velos_instances: RwLock<HashMap<String, Arc<VelosVendor>>>,
    /// Last build/reconfigure failure per vendor name, surfaced on
    /// `VendorView.error`. Cleared when that vendor next builds or
    /// reconfigures successfully.
    vendor_errors: RwLock<HashMap<String, String>>,
    /// Set once an *active* vendor's listener-affecting fields (`listen`/
    /// `advertise_host`/`server_url`) change — that one case still needs a
    /// process restart; never reset within a process's lifetime.
    restart_required: AtomicBool,
    info: ServerInfo,
    /// Held only to keep the shared local-runtime listener bound for the
    /// store's lifetime (unlike `velos_instances`, nothing reads this yet —
    /// no DB persistence, no live reconfigure, no listing endpoint).
    _local_daemon_registry: Option<LocalDaemonRegistry>,
}
```

Now change `build_vendors` (drop the local-vendor-building branch entirely):

```rust
/// Build the vendor set: `local` if its runtime binary is actually runnable,
/// plus one per configured row. A vendor that fails to build (or, for
/// `local`, whose binary can't be found) is logged and left out (reported
/// inactive), never fatal — matches `reconcile_vendors`'s per-update
/// behavior.
async fn build_vendors(
    rows: &[VendorRow],
    runtime_bin: PathBuf,
    workspace_root: PathBuf,
    public_http_base: Option<String>,
) -> (
    HashMap<String, Arc<dyn RuntimeVendor>>,
    HashMap<String, Arc<VelosVendor>>,
) {
    let mut vendors: HashMap<String, Arc<dyn RuntimeVendor>> = HashMap::new();
    let mut velos_instances: HashMap<String, Arc<VelosVendor>> = HashMap::new();
    if local_runtime_available(&runtime_bin).await {
        vendors.insert(
            "local".into(),
            Arc::new(LocalProcessVendor::new(
                runtime_bin,
                workspace_root,
                public_http_base,
            )),
        );
    } else {
        eprintln!(
            "warning: vendor 'local' disabled — runtime binary '{}' not found or not runnable",
            runtime_bin.display()
        );
    }
    for r in rows {
        match build_one_vendor(r).await {
            Ok(built) => {
                println!("vendor '{}' ({}) enabled", r.name, r.kind);
                let BuiltVendor::Velos(v) = &built;
                velos_instances.insert(r.name.clone(), v.clone());
                vendors.insert(r.name.clone(), built.as_dyn());
            }
            Err(e) => eprintln!("warning: vendor '{}' failed to start ({e})", r.name),
        }
    }
    (vendors, velos_instances)
}

/// Whether `bin` resolves to a runnable executable (`bin --version` exits
/// zero). Cheap, side-effect-free, and doesn't require any of the runtime's
/// real arguments (clap handles `--version` before validating them).
async fn local_runtime_available(bin: &Path) -> bool {
    let bin = bin.to_path_buf();
    tokio::task::spawn_blocking(move || {
        std::process::Command::new(&bin)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false)
}
```

to:

```rust
/// Build the vendor set from configured rows. A vendor that fails to build
/// is logged and left out (reported inactive), never fatal — matches
/// `reconcile_vendors`'s per-update behavior. The shared local-runtime
/// vendor isn't built here: it's not a DB row, and its listener is bound
/// separately in `open()` (see [`LocalDaemonRegistry`]).
async fn build_vendors(
    rows: &[VendorRow],
) -> (
    HashMap<String, Arc<dyn RuntimeVendor>>,
    HashMap<String, Arc<VelosVendor>>,
) {
    let mut vendors: HashMap<String, Arc<dyn RuntimeVendor>> = HashMap::new();
    let mut velos_instances: HashMap<String, Arc<VelosVendor>> = HashMap::new();
    for r in rows {
        match build_one_vendor(r).await {
            Ok(built) => {
                println!("vendor '{}' ({}) enabled", r.name, r.kind);
                let BuiltVendor::Velos(v) = &built;
                velos_instances.insert(r.name.clone(), v.clone());
                vendors.insert(r.name.clone(), built.as_dyn());
            }
            Err(e) => eprintln!("warning: vendor '{}' failed to start ({e})", r.name),
        }
    }
    (vendors, velos_instances)
}
```

Now change `DbConfigStore::open()`:

```rust
    pub async fn open(db_url: &str, deps: StoreDeps) -> Result<OpenedConfig, String> {
        let pool = open_pool(db_url).await?;

        let provs = read_providers(&pool).await.map_err(|e| e.to_string())?;
        let mods = read_models(&pool).await.map_err(|e| e.to_string())?;
        let registry: SharedProviderRegistry =
            Arc::new(RwLock::new(build_registry(&provs, &mods)?));

        let vendor_rows = read_vendors(&pool).await.map_err(|e| e.to_string())?;
        let (vendors, velos_instances) = build_vendors(
            &vendor_rows,
            deps.runtime_bin,
            deps.workspace_root,
            deps.public_http_base,
        )
        .await;

        let default_vendor = read_setting(&pool, "default_vendor")
            .await
            .map_err(|e| e.to_string())?
            .unwrap_or_else(|| "local".into());
        let default_vendor = if vendors.contains_key(&default_vendor) {
            default_vendor
        } else {
            // `local` isn't guaranteed to be loaded (its runtime binary may be
            // missing), so fall back to whatever vendor IS available rather
            // than hardcoding a name that might not exist either.
            let fallback = vendors
                .keys()
                .min()
                .cloned()
                .unwrap_or_else(|| "local".into());
            eprintln!(
                "warning: default vendor '{default_vendor}' is not loaded; using '{fallback}'"
            );
            fallback
        };

        let vendors: SharedVendors = Arc::new(RwLock::new(vendors));
        let store = Arc::new(Self {
            pool: pool.clone(),
            registry: registry.clone(),
            default_vendor: RwLock::new(default_vendor),
            vendors: vendors.clone(),
            velos_instances: RwLock::new(velos_instances),
            vendor_errors: RwLock::new(HashMap::new()),
            restart_required: AtomicBool::new(false),
            info: deps.info,
        });
        Ok(OpenedConfig {
            store,
            registry,
            vendors,
            pool,
        })
    }
```

to:

```rust
    pub async fn open(db_url: &str, deps: StoreDeps) -> Result<OpenedConfig, String> {
        let pool = open_pool(db_url).await?;

        let provs = read_providers(&pool).await.map_err(|e| e.to_string())?;
        let mods = read_models(&pool).await.map_err(|e| e.to_string())?;
        let registry: SharedProviderRegistry =
            Arc::new(RwLock::new(build_registry(&provs, &mods)?));

        let vendor_rows = read_vendors(&pool).await.map_err(|e| e.to_string())?;
        let (vendors, velos_instances) = build_vendors(&vendor_rows).await;

        let default_vendor = read_setting(&pool, "default_vendor")
            .await
            .map_err(|e| e.to_string())?
            .unwrap_or_else(|| "local".into());
        let default_vendor = if vendors.contains_key(&default_vendor) {
            default_vendor
        } else {
            // A connected shared-local-vendor label isn't known at open()
            // time either (daemons dial in after startup), so fall back to
            // whatever vendor IS already loaded rather than hardcoding a
            // name that might not exist yet.
            let fallback = vendors
                .keys()
                .min()
                .cloned()
                .unwrap_or_else(|| "local".into());
            eprintln!(
                "warning: default vendor '{default_vendor}' is not loaded; using '{fallback}'"
            );
            fallback
        };

        let vendors: SharedVendors = Arc::new(RwLock::new(vendors));
        let local_daemon_registry = match deps.local_runtime_listen.as_deref() {
            Some(addr_str) => match addr_str.parse::<SocketAddr>() {
                Ok(addr) => match LocalDaemonRegistry::bind(addr, vendors.clone()).await {
                    Ok(registry) => Some(registry),
                    Err(e) => {
                        eprintln!("warning: shared local runtime vendor disabled: {e}");
                        None
                    }
                },
                Err(e) => {
                    eprintln!(
                        "warning: shared local runtime vendor disabled — invalid \
                         local_runtime_listen '{addr_str}': {e}"
                    );
                    None
                }
            },
            None => None,
        };
        let store = Arc::new(Self {
            pool: pool.clone(),
            registry: registry.clone(),
            default_vendor: RwLock::new(default_vendor),
            vendors: vendors.clone(),
            velos_instances: RwLock::new(velos_instances),
            vendor_errors: RwLock::new(HashMap::new()),
            restart_required: AtomicBool::new(false),
            info: deps.info,
            _local_daemon_registry: local_daemon_registry,
        });
        Ok(OpenedConfig {
            store,
            registry,
            vendors,
            pool,
        })
    }
```

- [ ] **Step 2: Update the two test-only `StoreDeps` construction sites inside `server/src/config/store.rs`**

Change (in the test module near the bottom of the file):

```rust
    async fn open(dir: &std::path::Path) -> OpenedConfig {
        DbConfigStore::open(
            &format!("sqlite://{}/t.db", dir.display()),
            StoreDeps {
                runtime_bin: PathBuf::from("horsie-runtime"),
                workspace_root: dir.join("workspaces"),
                info: info(),
                public_http_base: None,
            },
        )
        .await
        .unwrap()
    }
```

to:

```rust
    async fn open(dir: &std::path::Path) -> OpenedConfig {
        let _ = dir; // kept for signature symmetry with other test helpers in this crate
        DbConfigStore::open(
            &format!("sqlite://{}/t.db", dir.display()),
            StoreDeps {
                info: info(),
                local_runtime_listen: None,
            },
        )
        .await
        .unwrap()
    }
```

Then remove the four now-obsolete tests further down in the same test module (`local_runtime_available_is_false_for_a_missing_binary`, `local_runtime_available_is_true_for_a_runnable_binary`, `build_vendors_excludes_local_when_its_binary_is_missing`, `build_vendors_includes_local_when_its_binary_resolves`) — delete these four `#[tokio::test] async fn ... { ... }` blocks in their entirety (they test code removed in Step 1).

- [ ] **Step 3: Update the `StoreDeps` construction in `server/src/http/mod.rs`'s test helper**

Change:

```rust
        let opened = crate::config::DbConfigStore::open(
            &format!("sqlite://{}", db.display()),
            crate::config::StoreDeps {
                runtime_bin: std::path::PathBuf::from("horsie-runtime"),
                workspace_root: tmp.path().join("workspaces"),
                info: test_info(),
                public_http_base: None,
            },
        )
        .await
        .unwrap();
```

to:

```rust
        let opened = crate::config::DbConfigStore::open(
            &format!("sqlite://{}", db.display()),
            crate::config::StoreDeps {
                info: test_info(),
                local_runtime_listen: None,
            },
        )
        .await
        .unwrap();
```

- [ ] **Step 4: Build the server crate and fix any remaining fallout**

Run: `cargo build -p horsie-server --tests`
Expected: builds cleanly. If it doesn't, the error will point at any remaining reference to the removed fields/types — fix those references the same way as the analogous site above (there should be none left after Steps 1–3, but this build is the actual verification, not an assumption).

- [ ] **Step 5: Run the full server crate test suite**

Run: `cargo test -p horsie-server`
Expected: PASS (all previously-passing tests, plus Task 2's new vendor tests, minus the four deleted obsolete tests).

- [ ] **Step 6: Add the deployment config field in `cli/src/config.rs`**

Add this field to `HorsieConfig`, after `default_vendor` and before `database`:

```rust
    /// Address the shared local-runtime-vendor listener binds (session
    /// server only) — user-launched `horsie-runtime --endpoint ws://...`
    /// daemons dial back here so any number of sessions can share one
    /// already-running, already-open directory. Absent → the shared local
    /// vendor is disabled (no listener bound, no `"local"` vendor kind ever
    /// registered).
    #[serde(default)]
    pub local_runtime_listen: Option<String>,
```

Add these two tests to the `#[cfg(test)] mod tests` block at the bottom of the file, near `parses_default_vendor`/`velos_and_default_vendor_absent_by_default`:

```rust
    #[test]
    fn parses_local_runtime_listen() {
        let cfg: HorsieConfig =
            serde_json::from_str(r#"{ "local_runtime_listen": "0.0.0.0:7080" }"#).unwrap();
        assert_eq!(cfg.local_runtime_listen.as_deref(), Some("0.0.0.0:7080"));
    }

    #[test]
    fn local_runtime_listen_absent_by_default() {
        let cfg: HorsieConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.local_runtime_listen.is_none());
        assert!(HorsieConfig::default().local_runtime_listen.is_none());
    }
```

- [ ] **Step 7: Run the CLI config tests**

Run: `cargo test -p horsie-cli config::tests`
Expected: PASS, including the two new tests.

- [ ] **Step 8: Commit**

```bash
git add cli/src/config.rs
git commit -m "cli: add local_runtime_listen deployment config"
```

- [ ] **Step 9: Wire the new config into `cli/src/serve.rs`, dropping the dead local-vendor plumbing**

Remove the now-unused import and variable. Change:

```rust
use crate::capabilities;
use crate::config::HorsieConfig;
use crate::daemon::default_runtime_bin;
use crate::error::CliError;
```

to:

```rust
use crate::capabilities;
use crate::config::HorsieConfig;
use crate::error::CliError;
```

Change:

```rust
    let journal: Arc<dyn Journal> = Arc::new(FileJournal::new(data_dir.clone()));
    let runtime_bin = cfg.runtime.bin.clone().unwrap_or_else(default_runtime_bin);
```

to:

```rust
    let journal: Arc<dyn Journal> = Arc::new(FileJournal::new(data_dir.clone()));
```

Remove the local-vendor-only plugin capability grant (the old local vendor was the only thing that ever read from `<workspace_root>/.plugins`; no vendor does now). Change:

```rust
    let (pd, hp) = (plugins_dir.clone(), hook_path.clone());
    // The local vendor materializes selected bundles under
    // `<workspace_root>/.plugins`; grant the sandboxed runtime read/write there
    // so it can fetch, unpack, and scan them (harmless for velos, which is
    // unsandboxed and ignores the capability file).
    let local_plugins_root = state_dir
        .join("workspaces")
        .join(".plugins")
        .to_string_lossy()
        .into_owned();
    let caps_finalize: Arc<dyn Fn(CapabilitySpec) -> CapabilitySpec + Send + Sync> =
        Arc::new(move |caps| {
            let mut spec = capabilities::with_plugin_grants(
                capabilities::resolve_user_paths(caps),
                pd.as_deref(),
                &hp,
            );
            spec.grants.push(horsie_models::capabilities::Grant::Dir(
                horsie_models::capabilities::DirGrant {
                    path: local_plugins_root.clone(),
                    access: horsie_models::capabilities::Access::ReadWrite,
                },
            ));
            capabilities::with_default_seatbelt_rules(spec)
        });
```

to:

```rust
    let (pd, hp) = (plugins_dir.clone(), hook_path.clone());
    let caps_finalize: Arc<dyn Fn(CapabilitySpec) -> CapabilitySpec + Send + Sync> =
        Arc::new(move |caps| {
            let spec = capabilities::with_plugin_grants(
                capabilities::resolve_user_paths(caps),
                pd.as_deref(),
                &hp,
            );
            capabilities::with_default_seatbelt_rules(spec)
        });
```

Remove the now-unused `public_http_base` computation. Change:

```rust
    let db_url = resolve_db_url(&cfg, &data_dir);
    // Loopback base a co-located local-vendor runtime fetches plugin artifacts
    // from (same host as the server). Velos derives its own base from the
    // vendor config's advertise_host + http_port.
    let public_http_base = addr
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .map(|port| format!("http://127.0.0.1:{port}"));
    let info = ServerInfo {
```

to:

```rust
    let db_url = resolve_db_url(&cfg, &data_dir);
    let info = ServerInfo {
```

Change the `StoreDeps` construction:

```rust
    let opened = DbConfigStore::open(
        &db_url,
        StoreDeps {
            runtime_bin,
            workspace_root: state_dir.join("workspaces"),
            info,
            public_http_base: public_http_base.clone(),
        },
    )
    .await
    .map_err(CliError::Config)?;
```

to:

```rust
    let opened = DbConfigStore::open(
        &db_url,
        StoreDeps {
            info,
            local_runtime_listen: cfg.local_runtime_listen.clone(),
        },
    )
    .await
    .map_err(CliError::Config)?;
```

- [ ] **Step 10: Build the CLI crate**

Run: `cargo build -p horsie-cli`
Expected: builds cleanly with no unused-import/unused-variable warnings (this workspace denies warnings via `[lints] workspace = true` in most crates — check `cli/Cargo.toml`/the workspace root `Cargo.toml` `[workspace.lints]` if the build unexpectedly fails on a warning rather than an error, and fix the same way: remove the dead code rather than `#[allow]`).

- [ ] **Step 11: Run the full CLI crate test suite**

Run: `cargo test -p horsie-cli`
Expected: PASS.

- [ ] **Step 12: Commit**

```bash
git add cli/src/serve.rs
git commit -m "cli: wire the shared local runtime vendor into horsie serve"
```

---

## Task 4: Full-workspace verification and PR

**Files:** none (verification only, plus whatever small fixes fallout requires — apply them to the same files touched in Tasks 1–3, following this plan's existing patterns).

**Interfaces:** none — this task only runs checks and fixes anything they surface.

- [ ] **Step 1: Run the full workspace build**

Run: `cargo build --workspace --all-targets`
Expected: clean build, no errors or warnings. If something fails, it will name the crate/file — fix it consistently with the corresponding change earlier in this plan (e.g. a missed `StoreDeps` construction site, a doc comment still mentioning `LocalProcessVendor`) rather than papering over it with `#[allow]`.

- [ ] **Step 2: Run the full workspace test suite**

Run: `cargo test --workspace`
Expected: all tests pass, including every test added in Tasks 1–3 and every pre-existing test not touched by this plan.

- [ ] **Step 3: Run clippy across the workspace**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean. Fix anything it flags (e.g. an unused `Path`/`PathBuf` import missed in Task 3, or a `must_use` on `LocalDaemonRegistry::bind`'s `Result`).

- [ ] **Step 4: Grep for any remaining reference to the removed vendor to confirm full removal**

Run: `grep -rn "LocalProcessVendor" --include="*.rs" .`
Expected: no output (zero matches anywhere in the workspace).

- [ ] **Step 5: If this workspace has a `make check` target, run it as the final gate**

Run: `make check` (if present at the repo root — check `Makefile` for a `check` target combining build/test/lint/fluorite-drift checks, matching how prior features in this repo were verified before opening a PR).
Expected: green.

- [ ] **Step 6: Review the full diff against `origin/main` one more time for anything left half-migrated**

Run: `git diff origin/main --stat` then `git log --oneline origin/main..HEAD`
Expected: a coherent set of commits (executor plumbing → new vendor → wiring → any final fixes), touching exactly the files this plan named plus nothing unrelated.

- [ ] **Step 7: Open the pull request**

Push the branch and open a PR against `main` with a description covering:
- What changed: `LocalProcessVendor` removed; new shared local runtime vendor where a user-launched `horsie-runtime` daemon dials back over WS, fixed to its own directory, shareable by concurrent sessions via a caller-chosen label.
- How to use it: `horsie-runtime --endpoint ws://<server-host>:<port> --runtime-id <label> --workspace main=<dir>` (no `--sandbox-caps`), then create a session with `"vendor": "<label>"`. Enable the listener with `local_runtime_listen` in `config.json` (e.g. `"0.0.0.0:7080"`); absent → the shared local vendor is disabled entirely.
- Explicitly called-out follow-ups (not in this PR): a "list known local instances" HTTP endpoint + web UI picker; a dedicated ergonomic CLI subcommand (the raw `horsie-runtime` invocation above is fully sufficient today).
- Consequence worth noting: local, no-velos deployments lose the ability to provision GitHub-repo sessions (git_checkout) locally — that now requires velos (or a future vendor) to be configured; the design doc (`docs/superpowers/specs/2026-07-18-shared-local-runtime-vendor-design.md`) covers the tradeoff.

Run:
```bash
git push -u origin shared-local-runtime-vendor
gh pr create --title "Replace local process vendor with a shared local runtime vendor" --body "$(cat <<'EOF'
## Summary
- Removes `LocalProcessVendor` (server-spawned process, managed/host-dir workspace, git_checkout provisioning).
- Adds a shared local runtime vendor: a user-launched `horsie-runtime` daemon dials back over WS, fixed to whatever directory it was started in. Any number of sessions may share one connected daemon (by a caller-chosen label) concurrently — safe because the wire protocol already correlates concurrent tool calls by `call_id`, not connection order.
- `create`/`attach` never spawn anything (just look up the live connection); `stop`/`delete` are no-ops (no session owns the shared daemon).
- No provisioning (`repos`/`workdirs` against this vendor are rejected with a clear error) and no sandboxing (the directory/machine are already the user's own).

## Usage
Enable with `"local_runtime_listen": "0.0.0.0:7080"` in `config.json` (absent → disabled, no listener bound). Connect a daemon with:
```
horsie-runtime --endpoint ws://<server-host>:7080 --runtime-id <label> --workspace main=<dir>
```
Then create a session with `"vendor": "<label>"`.

## Explicit follow-ups (not in this PR)
- A "list known local instances" HTTP endpoint + web UI picker.
- A dedicated ergonomic CLI subcommand (the raw invocation above already works with zero new code).

## Known consequence
Local deployments without `velos` configured lose local GitHub-repo (`git_checkout`) provisioning — see `docs/superpowers/specs/2026-07-18-shared-local-runtime-vendor-design.md` for the tradeoff.

## Test plan
- [x] `cargo test --workspace`
- [x] `cargo clippy --workspace --all-targets -- -D warnings`
- [x] `grep -rn "LocalProcessVendor"` → no matches
EOF
)"
```

Report the PR URL once created.
