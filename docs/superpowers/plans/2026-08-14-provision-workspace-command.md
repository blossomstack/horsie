# ProvisionWorkspace as a Command — Implementation Plan (PR 1 of 3)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the runtime's `Provisioning` → `ProvisionFailed` → `Ready` boot handshake with a `ProvisionWorkspace` request the server issues after acquisition, so `Ready` means only "the process is up, confined and listening".

**Architecture:** Provision steps stop riding the runtime's environment (`HORSIE_PROVISION`) and stop being drained by the first connection. They travel in a correlated request on the existing relay, handled by the runtime's message loop like any other command, and become idempotent so every acquisition can send them without the server tracking what already happened.

**Tech Stack:** Rust, fluorite codegen (`crates/models/fluorite/*.fl` → `make types`), tokio, tokio-tungstenite.

**Spec:** `docs/superpowers/specs/2026-08-14-runtime-protocol-lifecycle-commands-design.md`

## Global Constraints

- **No deadline parameter on the wire.** `runtime_reconciler.rs` is the timeout mechanism; operations bound themselves runtime-side. Copied verbatim from the spec: "No design here may introduce a deadline parameter on the wire."
- **`.fl` edits require `make types`.** A codegen bump racing a PR reddens main; regenerate before building.
- **Production code denies `unwrap_used`, `expect_used`, `panic`, `wildcard_enum_match_arm`.** Test modules opt out with `#![cfg_attr(test, allow(...))]`.
- **CI runs clippy with `-D warnings`.** A local clippy without it exits 0 and reddens the PR.
- **Every command goes through `RuntimeClient` with `track()`/`untrack()`**, so the reconciler sees it.
- **Idempotence is required, not optional.** `ProvisionWorkspace` is sent on every acquisition.

---

## File Structure

**Protocol (`crates/models/`)**
- `fluorite/runtime.fl` — add `ProvisionWorkspaceRequest`/`Response`, `ProvisionResult`; remove `RuntimeProvisioning`, `RuntimeProvisionFailed` and their outbound arms.
- `fluorite/runtime_vendor.fl` — remove `RuntimeSpec.provision` and the `use executor.ProvisionStep`.
- `fluorite/executor.fl` — remove `RuntimeConfig.provision` (nothing reads it once the env var is gone).
- `src/lib.rs` — remove `ENV_PROVISION`.

**Runtime (`crates/runtime/`)**
- `src/steps.rs` — drop `steps_from_env`; `run_steps` stays and becomes the command's body.
- `src/main.rs` — drop the boot-phase provisioning block and the steps drain; add a `ProvisionWorkspace` arm to the message loop.
- `tests/provision_steps.rs` — rewritten to drive the command over a socket rather than through `create`.

**Host (`crates/runtime-host/`)**
- `src/transport.rs` — `provision_workspace()` default method; exhaustive matches updated.
- `src/client.rs` — `provision_workspace()` with `track`/`untrack`.
- `src/listener.rs` — the `Handshake::Provisioning` window goes.
- `src/process_provider.rs`, `src/vendor.rs` — stop injecting/plumbing provision steps.
- `src/testkit.rs` — `MockTransport` arm.

**Server (`crates/server/`)**
- `src/runtime_vendor/mod.rs` — `RuntimeSpec.provision` field and its `to_wire`.
- `src/runtime_manager.rs` — stop building `provision` into the spec; drop `Awaited::ProvisionFailed`; send `ProvisionWorkspace` in `get()`.
- `src/runtime_vendor/{fly,velos}.rs` — stop injecting `ENV_PROVISION`.
- `src/runtime_vendor/fake.rs` — answer the new inbound variant.

---

### Task 1: Protocol types

**Files:**
- Modify: `crates/models/fluorite/runtime.fl`
- Modify: `crates/models/fluorite/runtime_vendor.fl:18,51-52`
- Modify: `crates/models/fluorite/executor.fl:44-46`
- Modify: `crates/models/src/lib.rs:245-248`

**Interfaces:**
- Produces: `horsie_models::runtime::{ProvisionWorkspaceRequest, ProvisionWorkspaceResponse, ProvisionOk, ProvisionError, ProvisionResult}`; `RuntimeInboundMessage::ProvisionWorkspace`; `RuntimeOutboundMessage::ProvisionResult`.
- Removes: `RuntimeProvisioning`, `RuntimeProvisionFailed`, `RuntimeOutboundMessage::{Provisioning,ProvisionFailed}`, `RuntimeSpec.provision`, `RuntimeConfig.provision`, `horsie_models::ENV_PROVISION`.

- [ ] **Step 1: Add the request/response types to `runtime.fl`**

Above the `RuntimeInboundMessage` union, after `CancelCallRequest`:

```
/// Bring this runtime's workspaces to the state `steps` describes.
///
/// A request rather than a boot phase. As a phase it had no caller: nothing
/// could time it, retry it, or run it a second time, and a failure could only
/// be reported by the runtime exiting. It is also why `Ready` used to assert
/// three separate things at once.
///
/// Idempotent, which is what lets the server send it on every acquisition
/// instead of remembering whether it already did. A `git_checkout` over a
/// directory that already holds that checkout does nothing. The server cannot
/// know whether a hibernated runtime kept its volume; the runtime always can.
struct ProvisionWorkspaceRequest { call_id: String, steps: Vec<ProvisionStep> }

/// Which steps ran. Reported per step rather than as one boolean so a failure
/// names the step that failed and the ones before it are known to have applied.
struct ProvisionOk { applied: Vec<String> }
struct ProvisionError { reason: String }

#[type_tag = "status"]
union ProvisionResult { Ok(ProvisionOk), Err(ProvisionError) }

struct ProvisionWorkspaceResponse { call_id: String, result: ProvisionResult }
```

Add `use executor.ProvisionStep;` to the top of `runtime.fl` beside the existing `use hooks.HookRecord;`.

- [ ] **Step 2: Wire the union arms**

In `union RuntimeInboundMessage` add `ProvisionWorkspace(ProvisionWorkspaceRequest),`.
In `union RuntimeOutboundMessage` add `ProvisionResult(ProvisionWorkspaceResponse),` and **delete** the `Provisioning(RuntimeProvisioning),` and `ProvisionFailed(RuntimeProvisionFailed),` arms plus the two structs and their doc comments.

- [ ] **Step 3: Strip the now-dead carriers**

`runtime_vendor.fl`: delete `use executor.ProvisionStep;` and the `provision: Vec<ProvisionStep>,` field (with its doc) from `RuntimeSpec`. In `RuntimeRelayResponse`'s doc, replace the sentence naming `Ready`/`Provisioning`/`ProvisionFailed` with one naming only `Ready`.

`executor.fl`: delete `RuntimeConfig.provision` and its doc.

`crates/models/src/lib.rs`: delete `pub const ENV_PROVISION`.

- [ ] **Step 4: Regenerate and check the types compile**

```bash
make types && cargo check -p horsie-models
```
Expected: PASS. Everything downstream will not compile yet — that is the rest of the plan.

- [ ] **Step 5: Commit**

```bash
git add crates/models && git commit -m "feat(protocol): provision workspaces by request, not by boot phase"
```

---

### Task 2: The runtime executes the command

**Files:**
- Modify: `crates/runtime/src/steps.rs:19-31` (delete `steps_from_env` and its tests)
- Modify: `crates/runtime/src/main.rs:299-310, 417-439, 528-588, 604-853`
- Test: `crates/runtime/tests/provision_steps.rs` (rewrite)

**Interfaces:**
- Consumes: `ProvisionWorkspaceRequest`, `ProvisionResult` from Task 1.
- Produces: the runtime answers `RuntimeInboundMessage::ProvisionWorkspace` with `RuntimeOutboundMessage::ProvisionResult`, registered in its in-flight map so `Ping` reports it and `CancelCall` aborts it.
- Keeps: `pub async fn run_steps(registry: &WorkspaceRegistry, steps: &[ProvisionStep]) -> Result<(), String>` unchanged.

- [ ] **Step 1: Write the failing test**

Replace `crates/runtime/tests/provision_steps.rs` wholesale. It no longer drives `ProcessRuntimeProvider`; it speaks the protocol over a socket, like `crates/runtime/tests/ping.rs` does. Copy that file's harness (`send`, `next_outbound`, the `Ready`-first assertion) and add:

```rust
#[tokio::test]
async fn provision_workspace_clones_and_reports_each_step() {
    let fixture = TempDir::new().unwrap();
    let url = fixture_repo(fixture.path());
    let ws = TempDir::new().unwrap();
    let mut rt = spawn_runtime(ws.path()).await;

    rt.expect_ready().await;
    rt.send(RuntimeInboundMessage::ProvisionWorkspace(
        ProvisionWorkspaceRequest {
            call_id: "p1".into(),
            steps: vec![checkout_step(&url, "repo")],
        },
    ))
    .await;

    match rt.next_outbound().await {
        RuntimeOutboundMessage::ProvisionResult(r) => {
            assert_eq!(r.call_id, "p1");
            match r.result {
                ProvisionResult::Ok(ok) => assert_eq!(ok.applied, vec!["checkout repo".to_string()]),
                ProvisionResult::Err(e) => panic!("expected success, got {}", e.reason),
            }
        }
        other => panic!("expected ProvisionResult, got {other:?}"),
    }
    assert!(ws.path().join("repo/README.md").is_file());
}

/// The point of the change: a second identical request is not an error and
/// does not clone twice. The server sends this on every acquisition.
#[tokio::test]
async fn provisioning_twice_is_not_an_error() {
    let fixture = TempDir::new().unwrap();
    let url = fixture_repo(fixture.path());
    let ws = TempDir::new().unwrap();
    let mut rt = spawn_runtime(ws.path()).await;
    rt.expect_ready().await;

    for call_id in ["p1", "p2"] {
        rt.send(RuntimeInboundMessage::ProvisionWorkspace(
            ProvisionWorkspaceRequest {
                call_id: call_id.into(),
                steps: vec![checkout_step(&url, "repo")],
            },
        ))
        .await;
        match rt.next_outbound().await {
            RuntimeOutboundMessage::ProvisionResult(r) => {
                assert!(matches!(r.result, ProvisionResult::Ok(_)), "{call_id} failed");
            }
            other => panic!("expected ProvisionResult, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_bad_clone_reports_the_git_error_instead_of_killing_the_runtime() {
    let ws = TempDir::new().unwrap();
    let mut rt = spawn_runtime(ws.path()).await;
    rt.expect_ready().await;

    rt.send(RuntimeInboundMessage::ProvisionWorkspace(
        ProvisionWorkspaceRequest {
            call_id: "p1".into(),
            steps: vec![checkout_step("file:///nonexistent-xyz", "repo")],
        },
    ))
    .await;

    match rt.next_outbound().await {
        RuntimeOutboundMessage::ProvisionResult(r) => match r.result {
            ProvisionResult::Err(e) => assert!(e.reason.contains("git clone failed"), "{}", e.reason),
            ProvisionResult::Ok(_) => panic!("a bad clone must not report success"),
        },
        other => panic!("expected ProvisionResult, got {other:?}"),
    }

    // The runtime is still alive and still answering — the old path exited 5.
    rt.send(RuntimeInboundMessage::Ping(PingRequest { call_id: "ping".into() })).await;
    assert!(matches!(rt.next_outbound().await, RuntimeOutboundMessage::Pong(_)));
}
```

Keep `git`, `fixture_repo` and `checkout_step` from the existing file verbatim.

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test -p horsie-runtime --test provision_steps
```
Expected: FAIL to compile — `ProvisionWorkspaceRequest` has no handler.

- [ ] **Step 3: Delete the boot-phase path**

In `crates/runtime/src/main.rs`:
- Delete the `steps_from_env` block in `run()` (`:299-310`) and the `steps` local.
- Remove the `steps` parameter from `serve_until_disconnected` and `run_loop`, plus `let mut steps = steps;` and `std::mem::take(&mut steps)`, and the doc paragraphs about draining.
- Delete the `if !steps.is_empty() { … }` block in `run_loop` (`:544-573`) so `Ready` is sent unconditionally and immediately.
- Drop `RuntimeProvisionFailed, RuntimeProvisioning` from the `use` at `:15`.

In `crates/runtime/src/steps.rs`, delete `steps_from_env` and its unit tests; keep `run_steps` and the `git_checkout` helpers.

- [ ] **Step 4: Add the message-loop arm**

In `run_loop`'s `match inbound`, mirroring the `ScanWorkspace` arm — spawned, registered in `in_flight` so `Ping` reports it and `CancelCall` aborts it:

```rust
RuntimeInboundMessage::ProvisionWorkspace(req) => {
    let call_id = req.call_id.clone();
    let map_id = req.call_id.clone();
    let registry = registry.clone();
    let sink_clone = sink.clone();
    let in_flight_clone = in_flight.clone();

    // Registered like any other request: a clone of a large repository is
    // exactly the long work the reconciler must see as running, and exactly
    // the work a user hitting Stop must be able to abandon.
    let handle = tokio::spawn(async move {
        let applied: Vec<String> = req.steps.iter().map(|s| s.name.clone()).collect();
        let result = match horsie_runtime::steps::run_steps(&registry, &req.steps).await {
            Ok(()) => ProvisionResult::Ok(ProvisionOk { applied }),
            Err(reason) => ProvisionResult::Err(ProvisionError { reason }),
        };
        let response = serde_json::to_string(&RuntimeOutboundMessage::ProvisionResult(
            ProvisionWorkspaceResponse { call_id: call_id.clone(), result },
        ));
        if let Ok(json) = response {
            let _ = sink_clone.lock().await.send(Message::Text(json.into())).await;
        }
        in_flight_clone.lock().await.remove(&call_id);
    });

    in_flight.lock().await.insert(map_id, handle.abort_handle());
}
```

- [ ] **Step 5: Run the tests to verify they pass**

```bash
cargo test -p horsie-runtime --test provision_steps
```
Expected: PASS, all three.

- [ ] **Step 6: Commit**

```bash
git add crates/runtime && git commit -m "feat(runtime): execute provision steps on request"
```

---

### Task 3: Host transport and client

**Files:**
- Modify: `crates/runtime-host/src/transport.rs:72-82,104-115,154-164,183-193,206-216,233-243,281-291,297-307`
- Modify: `crates/runtime-host/src/client.rs`
- Modify: `crates/runtime-host/src/listener.rs:36,197,206-215,224-253`
- Modify: `crates/runtime-host/src/process_provider.rs:181-198`
- Modify: `crates/runtime-host/src/vendor.rs:847`
- Modify: `crates/runtime-host/src/testkit.rs:356-456`

**Interfaces:**
- Produces: `RuntimeTransport::provision_workspace(&self, call_id: &str, steps: Vec<ProvisionStep>) -> Result<ProvisionResult, TransportError>` and `RuntimeClient::provision_workspace(&self, steps: Vec<ProvisionStep>) -> Result<(), RuntimeCallError>`.

- [ ] **Step 1: Write the failing test**

In `crates/runtime-host/src/client.rs`'s test module:

```rust
/// Provisioning is tracked like a tool call, so the reconciler sees a long
/// clone as running rather than cancelling it as an orphan, and Stop reaches it.
#[tokio::test]
async fn provision_workspace_is_tracked_while_it_runs() {
    let gate = crate::testkit::BlockHandle::new();
    let transport = crate::testkit::MockTransport::gated(gate.clone());
    let in_flight = Arc::new(InFlight::new());
    let client = RuntimeClient::new(transport, "a1").with_in_flight(in_flight.clone());

    let task = {
        let client = client.clone();
        tokio::spawn(async move { client.provision_workspace(vec![]).await })
    };
    gate.wait_until_blocked().await;
    assert_eq!(in_flight.len(), 1, "a running provision must be visible to the reconciler");

    gate.release();
    let _ = task.await;
    assert_eq!(in_flight.len(), 0, "and untracked once it answers");
}
```

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo test -p horsie-runtime-host --lib provision_workspace_is_tracked
```
Expected: FAIL — no such method.

- [ ] **Step 3: Add the transport method**

In `crates/runtime-host/src/transport.rs`, beside `scan_workspace`:

```rust
/// Bring the workspaces to the state `steps` describes.
///
/// Bounded by the steps themselves, never by a deadline here: a clone of a
/// large repository and a checkout that is already present ride this same
/// call, which is the shape the reconciler exists to handle.
async fn provision_workspace(
    &self,
    call_id: &str,
    steps: Vec<ProvisionStep>,
) -> Result<ProvisionResult, TransportError> {
    let reply = self
        .relay(RuntimeInboundMessage::ProvisionWorkspace(
            ProvisionWorkspaceRequest { call_id: call_id.to_string(), steps },
        ))
        .await?;
    match reply {
        RuntimeOutboundMessage::ProvisionResult(resp) => Ok(resp.result),
        RuntimeOutboundMessage::Ready(_)
        | RuntimeOutboundMessage::ToolCallResponse(_)
        | RuntimeOutboundMessage::ScanResult(_)
        | RuntimeOutboundMessage::HookRecords(_)
        | RuntimeOutboundMessage::McpTools(_)
        | RuntimeOutboundMessage::McpResult(_)
        | RuntimeOutboundMessage::Pong(_) => Err(wrong_reply("a workspace provision")),
    }
}
```

Then update every other exhaustive match in the file: delete the `Provisioning(_)` and `ProvisionFailed(_)` alternatives and add `ProvisionResult(_)`. In `inbound_call_id` add `RuntimeInboundMessage::ProvisionWorkspace(req) => &req.call_id`. In `outbound_call_id` add `ProvisionResult(resp) => Some(&resp.call_id)` and reduce the `None` arm to `Ready(_)` alone.

- [ ] **Step 4: Add the client method**

In `crates/runtime-host/src/client.rs`, shaped exactly like `invoke`:

```rust
/// Bring the workspaces to the state `steps` describes.
///
/// Tracked like a tool call — see [`Self::invoke`]. A clone that takes minutes
/// must be visible to the reconciler as running, or it is cancelled as an
/// orphan; and a user hitting Stop must be able to abandon it.
pub async fn provision_workspace(&self, steps: Vec<ProvisionStep>) -> Result<(), RuntimeCallError> {
    let call_id = Uuid::new_v4().to_string();
    self.track(&call_id);
    let outcome = self.inner.provision_workspace(&call_id, steps).await;
    self.untrack(&call_id);
    match outcome.map_err(RuntimeCallError::Transport)? {
        ProvisionResult::Ok(_) => Ok(()),
        ProvisionResult::Err(ProvisionError { reason }) => Err(RuntimeCallError::ToolFailed(reason)),
    }
}
```

- [ ] **Step 5: Delete the handshake window**

In `crates/runtime-host/src/listener.rs`: remove the `Handshake::Provisioning` variant, its first-frame arm, the long provision-window branch, the `ProvisionFailed` arm and the `"timed out during provisioning"` path. The handshake now accepts `Ready` or fails.

In `crates/runtime-host/src/process_provider.rs`: delete the `ENV_PROVISION` injection block and the stale comment at `:196-197`.
In `crates/runtime-host/src/vendor.rs:847`: drop `provision:` from the `RuntimeConfig` literal.
In `crates/runtime-host/src/testkit.rs`: add a `ProvisionWorkspace` arm to `MockTransport::relay` answering `ProvisionResult::Ok(ProvisionOk { applied: vec![] })`, gated by the same `BlockHandle` `scan_workspace` uses.

- [ ] **Step 6: Run the tests**

```bash
cargo test -p horsie-runtime-host --lib
```
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/runtime-host && git commit -m "feat(host): relay and track workspace provisioning"
```

---

### Task 4: The server sends it

**Files:**
- Modify: `crates/server/src/runtime_vendor/mod.rs:61-81`
- Modify: `crates/server/src/runtime_manager.rs:87,235-258,474,544-552,571,1359-1391`
- Modify: `crates/server/src/runtime_vendor/fly.rs:265-273,830-845`
- Modify: `crates/server/src/runtime_vendor/velos.rs:140-147,721-732`
- Modify: `crates/server/src/runtime_vendor/fake.rs:743-890`

**Interfaces:**
- Consumes: `RuntimeClient::provision_workspace` from Task 3.
- Produces: `RuntimeManager::get` provisions before returning its client.

- [ ] **Step 1: Write the failing test**

In `crates/server/src/runtime_manager.rs`'s test module:

```rust
/// Every acquisition provisions. The server does not remember whether it
/// already did — a hibernated runtime may or may not have kept its workspace,
/// and only the runtime knows which.
#[tokio::test]
async fn every_acquisition_provisions_the_workspace() {
    let (manager, fake) = manager_with_fake_vendor().await;
    let spec = session_spec_with_checkout();

    manager.create("s1", "i1", "fake", &spec).await.unwrap();
    for _ in 0..2 {
        manager.get("s1", "i1", "fake", &spec, None).await.unwrap();
    }

    assert_eq!(
        fake.provision_requests().len(),
        2,
        "each acquisition sends its own ProvisionWorkspace"
    );
}
```

Add a `provision_requests()` recorder to `fake.rs` alongside `last_create_request()`.

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo test -p horsie-server --lib every_acquisition_provisions
```
Expected: FAIL — nothing sends the request.

- [ ] **Step 3: Strip `provision` from the spec**

`runtime_vendor/mod.rs`: delete the `provision` field from `RuntimeSpec` and its line in `to_wire()`.
`runtime_manager.rs`: delete the `provision:` mapping in `runtime_spec` (`:243-258`).
`fly.rs` / `velos.rs`: delete the `ENV_PROVISION` injection blocks and the two `provision_steps_ride_the_environment` tests.
Fix every `RuntimeSpec { … provision: vec![] … }` literal listed in the survey.

- [ ] **Step 4: Drop `ProvisionFailed` from the acquisition**

`runtime_manager.rs`: remove `Awaited::ProvisionFailed`, its `use`, its `select!` arm and its `Err` mapping. Delete the test `a_runtime_that_fails_its_provision_steps_fails_the_acquisition` — provisioning is no longer part of the handshake, and Step 1's test covers the new path.

- [ ] **Step 5: Send it from `get()`**

After the client is resolved and before it is returned, when the session has steps:

```rust
// Every acquisition, not just the first. The steps are idempotent, and this
// is the only party that cannot know whether a hibernated runtime kept its
// workspace — so it asks rather than assuming.
if !spec.provision.is_empty() {
    let steps = Self::wire_steps(&spec.provision);
    client
        .provision_workspace(steps)
        .await
        .map_err(|e| RuntimeError::Provision(e.to_string()))?;
}
```

`wire_steps` is the `ProvisionStepSpec` → `horsie_models::executor::ProvisionStep` mapping lifted out of the old `runtime_spec` body.

- [ ] **Step 6: Answer it in the fake vendor**

Add a `ProvisionWorkspace` arm to `fake.rs`'s exhaustive `RuntimeInboundMessage` match: record the request, answer `ProvisionResult::Ok(ProvisionOk { applied })`.

- [ ] **Step 7: Run the tests**

```bash
cargo test -p horsie-server --lib runtime_manager
```
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/server && git commit -m "feat(server): provision the workspace on every acquisition"
```

---

### Task 5: Sweep the workspace green

**Files:** whatever still refers to the removed symbols — `crates/cli/`, `crates/tests/`, `crates/runtime-host/tests/vendor_conformance.rs`.

- [ ] **Step 1: Find what is left**

```bash
cargo check --workspace --all-targets 2>&1 | grep -E '^error' | head -50
```

- [ ] **Step 2: Fix each**

The expected set, from the survey: five wire `RuntimeSpec` literals in `crates/cli/tests/connect_e2e.rs`, the `spec()` helper in `crates/runtime-host/tests/vendor_conformance.rs:119-125`, and `crates/server/src/environments/service.rs:336`'s reserved-name list (drop `"HORSIE_PROVISION"`, keep the other two).

- [ ] **Step 3: Format and lint**

```bash
cargo fmt --all && cargo clippy --all-targets --all-features -- -D warnings
```
Expected: clean. `-D warnings` is not optional — CI adds it and a local run without it exits 0 on a PR that reddens.

- [ ] **Step 4: Commit and open the PR**

```bash
git add -A && git commit -m "chore: drop the last references to boot-phase provisioning"
git push -u origin feat/runtime-lifecycle-commands
gh pr create --title "feat(protocol)!: provision workspaces by request" --body-file <(...)
```

---

## Self-Review

**Spec coverage.** This plan covers the spec's decision 4 (`ProvisionWorkspace` as a command), the `runtime.fl`/`runtime_vendor.fl` removals attributable to it, and step 0 of the ordering section. Decisions 1–3 and 5, the store-and-symlink layout, `agent_id` on scan/discovery, `CancelledResponse`, and the orphan-cancellation fix belong to PRs 2 and 3 and are deliberately out of scope here.

**Deferred to PR 2:** `CancelledResponse`. `ProvisionWorkspace` is tracked from Task 3, so a Stop during a clone hits the tool-shaped cancel reply and surfaces as a wrong-message error. That is a pre-existing defect this PR makes reachable on one more command; PR 2 fixes it for all seven at once. Called out here so it is a known deferral rather than an oversight.
