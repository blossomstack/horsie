# Kubernetes RuntimeProvider Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `KubernetesRuntimeProvider` that implements the existing `RuntimeProvider` trait by launching the `october-runtime` container as a Kubernetes Pod that dials back to the executor over WebSocket.

**Architecture:** Mirror `ProcessRuntimeProvider` exactly. The only behavioral change is "spawn a child process" → "create a bare Pod (`restartPolicy: Never`)". All `kube` interaction hides behind a narrow two-method `PodApi` trait so the provider/handle logic is 100% unit-tested with a fake and a pure pod-spec builder — no cluster required. Code lives in the existing `executor` crate behind a non-default `kubernetes` cargo feature.

**Tech Stack:** Rust (edition 2024, pinned toolchain 1.96.0), `kube` 3.x + `k8s-openapi` 0.27 (rustls-tls), `async-trait`, `tokio`. CI runs `--all-features` for clippy/test/cargo-deny.

---

## Design decisions (human design-review checkpoint)

| Question | Decision | Why |
|---|---|---|
| Pod vs Job vs Deployment | **Bare Pod, `restartPolicy: Never`** | The executor owns the restart lifecycle (`run_health_check`/`do_restart` re-call `provider.create()` with the same id, capped at `max_restarts`). A controller would fight that and orphan replicas. |
| Test seam | `trait PodApi { create(&Pod); delete(&str) }` | Two methods suffice (reverse-connection model: `create` never connects; `stop`/timeout only `delete`). Real `KubePodApi` wraps `kube::Api<Pod>`; `FakePodApi` in tests. Narrow interface / deep impl. |
| Dial-back address | Explicit `callback_url: String` constructor arg | The listener binds `0.0.0.0`/`127.0.0.1`, unroutable from a Pod. The cluster-reachable Service DNS / NodePort is operator-supplied. Not a `RuntimeConfig`/fluorite change. |
| Cleanup on failure | `create()` calls `pod_api.delete()` on **every** error/timeout path | A Pod has no `Drop`/`kill_on_drop`. Without this, every failed `create()` (ImagePullBackOff, bad URL) leaks a Pod, and the executor retries → orphan storm. **#1 correctness invariant.** |
| Conflict / not-found | `create` maps HTTP 409 → `AlreadyExists`; `delete` treats 404 as `Ok` (idempotent) | Lets the restart-by-recreate loop never wedge on a leftover pod. |
| Pod name | `sanitize_pod_name(id)` → DNS-1123 label `october-runtime-<id>`, lowercased, `[a-z0-9-]`, no leading/trailing `-`, ≤63 chars | API server rejects invalid names; ids aren't guaranteed clean UUIDs. Stored on the handle so `stop()` targets the exact object. |
| Sandbox / `--sandbox-caps` | **Deferred** (no `with_sandbox` in v1) | `nono` is host-process confinement (and the fork is macOS-specific); inside a Linux Pod the container is the isolation boundary. A Pod container also starts from a clean env, so the process-mode env-scrub concern is structurally absent. |
| Dispatch | Provider holds `Arc<dyn PodApi>` (not generic) | `RuntimeProvider` is consumed as `Box<dyn ...>`; matches `ProcessRuntimeProvider`'s `Arc` fields. |
| Placement | Existing `executor` crate, `kubernetes` cargo feature | No new crate (avoids the org cargo-deny new-crate rule). Default build unaffected; CI `--all-features` compiles/tests/license-checks it. |

**In scope:** the provider, handle, `PodApi` + `KubePodApi`, pure builders, unit tests, deps/feature, exports.

**Explicit follow-ups (NOT this PR):** executor TCP-listener wiring (`bind 0.0.0.0`) + Service topology; resource limits / `securityContext` / `ownerReferences` builder methods; label-based orphan sweep + ownerReference GC; k8s-native capability translation (replacing in-container nono); `wss` + per-pod bearer-token Ready-handshake auth + NetworkPolicy; `#[ignore]`d real-cluster e2e test.

---

## File structure

- **Create** `executor/src/k8s_provider.rs` — the entire feature: `PodApi` trait, `KubePodApi`, `KubernetesRuntimeProvider`, `KubernetesRuntimeHandle`, pure `sanitize_pod_name`/`build_pod_spec`/`classify_*` helpers, and the in-file `#[cfg(test)] mod tests`.
- **Modify** `executor/Cargo.toml` — optional `kube`/`k8s-openapi` deps + `kubernetes` feature.
- **Modify** `executor/src/lib.rs` — feature-gated `mod` + re-exports.
- **Modify** `Cargo.lock` — regenerated, committed (CI is `--locked`).

---

### Task 1: Add deps + feature flag, de-risk the build FIRST

**Files:**
- Modify: `executor/Cargo.toml`

- [ ] **Step 1: Add optional deps + feature to `executor/Cargo.toml`**

Under `[dependencies]` add:

```toml
# Kubernetes runtime provider (optional; enabled by the `kubernetes` feature).
# rustls-tls reuses the existing rustls/ring stack — do NOT enable openssl-tls.
kube        = { version = "3", default-features = false, features = ["client", "rustls-tls"], optional = true }
k8s-openapi = { version = "0.27", default-features = false, features = ["v1_31"], optional = true }
```

Add a new section after `[dependencies]`:

```toml
[features]
# Compile the KubernetesRuntimeProvider. Off by default so non-k8s builds stay lean;
# CI runs --all-features so it is always compiled, tested, and license-checked.
kubernetes = ["dep:kube", "dep:k8s-openapi"]
```

- [ ] **Step 2: Verify it resolves and builds on the pinned toolchain**

Run: `cargo +1.96.0 build -p executor --all-features`
Expected: PASS (downloads kube/k8s-openapi). If `kube = "3"` fails to resolve or bumps MSRV above 1.96.0, fall back to `kube = "0.99"` + `k8s-openapi = "0.24"` (same `v1_31`, `rustls-tls`, `default-features=false`) and re-run.

- [ ] **Step 3: Confirm no openssl and license-clean supply chain**

Run: `cargo tree -p executor --all-features -i openssl-sys` → Expected: no match (executor's tree is rustls-only). Note: `openssl-sys` already exists in `Cargo.lock` on `main` (pre-existing via other crates' `native-tls`); the check is that kube adds none to the executor tree, not that the lockfile is globally openssl-free.
Run: `cargo deny check licenses bans sources --all-features 2>&1 | tail -20`
Expected: no license errors. If a new SPDX surfaces from a kube-only transitive, add it to `deny.toml`'s `[licenses] allow` list in this PR and re-run.

- [ ] **Step 4: Confirm the default build is unaffected**

Run: `cargo build -p executor`
Expected: PASS, no kube compiled.

- [ ] **Step 5: Commit (lockfile + manifest)**

```bash
git add executor/Cargo.toml Cargo.lock deny.toml
git commit -m "build: add optional kube deps behind kubernetes feature"
```

---

### Task 2: Empty feature-gated module compiles

**Files:**
- Create: `executor/src/k8s_provider.rs`
- Modify: `executor/src/lib.rs`

- [ ] **Step 1: Create the module file with header + imports**

`executor/src/k8s_provider.rs`:

```rust
//! Kubernetes runtime provider: launches the `october-runtime` container as a bare
//! Pod (`restartPolicy: Never`) that dials back to the executor's WebSocket listener,
//! mirroring [`ProcessRuntimeProvider`](crate::ProcessRuntimeProvider). All `kube`
//! interaction hides behind the narrow [`PodApi`] seam so the provider/handle logic
//! is unit-testable without a cluster.

use crate::{
    connected_registry::ConnectedRuntimeRegistry,
    error::RuntimeError,
    provider::{HealthStatus, RuntimeHandle, RuntimeProvider},
};
use async_trait::async_trait;
use k8s_openapi::api::core::v1::{Container, Pod, PodSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use models::executor::RuntimeConfig;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
```

- [ ] **Step 2: Gate the module in `executor/src/lib.rs`**

Add after line 6 (`mod process_provider;`):

```rust
#[cfg(feature = "kubernetes")]
mod k8s_provider;
```

(Re-exports are added in Task 8 once the public types exist.)

- [ ] **Step 3: Verify both build profiles compile**

Run: `cargo build -p executor && cargo build -p executor --all-features`
Expected: PASS (the `--all-features` build will warn about unused imports — fine until the next task fills them in; do not commit yet).

---

### Task 3: `sanitize_pod_name` (pure, TDD)

**Files:**
- Modify: `executor/src/k8s_provider.rs`

- [ ] **Step 1: Write failing tests**

Append to `executor/src/k8s_provider.rs`:

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

    fn is_dns1123_label(s: &str) -> bool {
        !s.is_empty()
            && s.len() <= 63
            && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !s.starts_with('-')
            && !s.ends_with('-')
    }

    #[test]
    fn sanitize_keeps_clean_uuid() {
        let id = "3f1a2b3c-0000-4d5e-8f90-aabbccddeeff";
        assert_eq!(sanitize_pod_name(id), format!("october-runtime-{id}"));
        assert!(is_dns1123_label(&sanitize_pod_name(id)));
    }

    #[test]
    fn sanitize_lowercases_and_replaces_invalid() {
        assert_eq!(sanitize_pod_name("ABC_123"), "october-runtime-abc-123");
    }

    #[test]
    fn sanitize_truncates_to_63_and_stays_valid() {
        let id = "a".repeat(200);
        let name = sanitize_pod_name(&id);
        assert!(name.len() <= 63);
        assert!(is_dns1123_label(&name));
    }

    #[test]
    fn sanitize_trims_dashes_and_handles_empty() {
        assert_eq!(sanitize_pod_name("--x--"), "october-runtime-x");
        assert_eq!(sanitize_pod_name("***"), "october-runtime-rt");
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p executor --all-features sanitize_ 2>&1 | tail -20`
Expected: FAIL — `cannot find function sanitize_pod_name`.

- [ ] **Step 3: Implement `sanitize_pod_name`**

Insert into `executor/src/k8s_provider.rs` (above the test module):

```rust
/// k8s object names must be DNS-1123 labels (`[a-z0-9-]`, ≤63 chars, no leading/
/// trailing `-`). Build `october-runtime-<sanitized-id>`: lowercase, map any other
/// char to `-`, truncate, trim `-`, and fall back to `rt` if nothing survives.
fn sanitize_pod_name(runtime_id: &str) -> String {
    const PREFIX: &str = "october-runtime-";
    let max_id_len = 63 - PREFIX.len();
    let mapped: String = runtime_id
        .chars()
        .map(|c| {
            let lc = c.to_ascii_lowercase();
            if lc.is_ascii_alphanumeric() || lc == '-' {
                lc
            } else {
                '-'
            }
        })
        .take(max_id_len)
        .collect();
    let trimmed = mapped.trim_matches('-');
    let id_part = if trimmed.is_empty() { "rt" } else { trimmed };
    format!("{PREFIX}{id_part}")
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p executor --all-features sanitize_ 2>&1 | tail -20`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add executor/src/k8s_provider.rs executor/src/lib.rs
git commit -m "feat: add DNS-1123 pod name sanitizer for k8s provider"
```

---

### Task 4: `build_pod_spec` (pure, TDD)

**Files:**
- Modify: `executor/src/k8s_provider.rs`

- [ ] **Step 1: Write failing tests** (append inside `mod tests`)

```rust
    fn sample_pod() -> Pod {
        build_pod_spec("img:tag", "october", "october-runtime-rt-1", "rt-1", "/work", "ws://cb:9000")
    }

    #[test]
    fn pod_uses_restart_policy_never() {
        let pod = sample_pod();
        assert_eq!(pod.spec.unwrap().restart_policy.as_deref(), Some("Never"));
    }

    #[test]
    fn pod_args_match_process_argv_exactly() {
        let pod = sample_pod();
        let c = &pod.spec.unwrap().containers[0];
        assert_eq!(
            c.args.clone().unwrap(),
            vec![
                "--endpoint", "ws://cb:9000",
                "--runtime-id", "rt-1",
                "--working-dir", "/work",
            ]
        );
    }

    #[test]
    fn pod_has_no_sandbox_caps_arg() {
        let pod = sample_pod();
        let c = &pod.spec.unwrap().containers[0];
        assert!(!c.args.clone().unwrap().iter().any(|a| a == "--sandbox-caps"));
    }

    #[test]
    fn pod_carries_management_labels() {
        let pod = sample_pod();
        let labels = pod.metadata.labels.unwrap();
        assert_eq!(labels.get("app.kubernetes.io/managed-by").map(String::as_str), Some("october-executor"));
        assert_eq!(labels.get("october.dev/runtime-id").map(String::as_str), Some("rt-1"));
    }

    #[test]
    fn pod_disables_service_account_automount() {
        let pod = sample_pod();
        assert_eq!(pod.spec.unwrap().automount_service_account_token, Some(false));
    }

    #[test]
    fn pod_sets_name_namespace_image() {
        let pod = sample_pod();
        assert_eq!(pod.metadata.name.as_deref(), Some("october-runtime-rt-1"));
        assert_eq!(pod.metadata.namespace.as_deref(), Some("october"));
        assert_eq!(pod.spec.unwrap().containers[0].image.as_deref(), Some("img:tag"));
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p executor --all-features pod_ 2>&1 | tail -20`
Expected: FAIL — `cannot find function build_pod_spec`.

- [ ] **Step 3: Implement `build_pod_spec`**

Insert above the test module:

```rust
/// Build the Pod manifest for one runtime. Pure (no I/O): the heavily-tested core.
/// `restartPolicy: Never` so the executor — not k8s — owns restarts; container `args`
/// mirror the process-mode child argv exactly.
fn build_pod_spec(
    image: &str,
    namespace: &str,
    pod_name: &str,
    runtime_id: &str,
    working_dir: &str,
    callback_url: &str,
) -> Pod {
    let mut labels = BTreeMap::new();
    labels.insert(
        "app.kubernetes.io/managed-by".to_string(),
        "october-executor".to_string(),
    );
    labels.insert("october.dev/runtime-id".to_string(), runtime_id.to_string());

    Pod {
        metadata: ObjectMeta {
            name: Some(pod_name.to_string()),
            namespace: Some(namespace.to_string()),
            labels: Some(labels),
            ..Default::default()
        },
        spec: Some(PodSpec {
            restart_policy: Some("Never".to_string()),
            automount_service_account_token: Some(false),
            containers: vec![Container {
                name: "runtime".to_string(),
                image: Some(image.to_string()),
                image_pull_policy: Some("IfNotPresent".to_string()),
                args: Some(vec![
                    "--endpoint".to_string(),
                    callback_url.to_string(),
                    "--runtime-id".to_string(),
                    runtime_id.to_string(),
                    "--working-dir".to_string(),
                    working_dir.to_string(),
                ]),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    }
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p executor --all-features pod_ 2>&1 | tail -20`
Expected: PASS (6 tests).

- [ ] **Step 5: Commit**

```bash
git add executor/src/k8s_provider.rs
git commit -m "feat: add pure k8s pod spec builder"
```

---

### Task 5: `PodApi` trait + error classifiers + `FakePodApi`

**Files:**
- Modify: `executor/src/k8s_provider.rs`

- [ ] **Step 1: Write failing classifier tests** (append inside `mod tests`)

```rust
    #[test]
    fn classify_create_conflict_maps_409_only() {
        assert!(matches!(classify_create_conflict(409, "p"), Some(RuntimeError::AlreadyExists(_))));
        assert!(classify_create_conflict(404, "p").is_none());
        assert!(classify_create_conflict(500, "p").is_none());
    }

    #[test]
    fn is_not_found_only_404() {
        assert!(is_not_found(404));
        assert!(!is_not_found(409));
        assert!(!is_not_found(500));
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p executor --all-features classify_ is_not_found 2>&1 | tail -20`
Expected: FAIL — `cannot find function classify_create_conflict`.

- [ ] **Step 3: Implement the trait + classifiers**

Insert above the test module:

```rust
/// Narrow seam over the Kubernetes pod API so the provider/handle are unit-testable
/// without a cluster. Returns [`RuntimeError`] directly — kube error classification
/// stays inside [`KubePodApi`] where `kube::Error` is in scope.
#[async_trait]
pub trait PodApi: Send + Sync {
    async fn create(&self, pod: &Pod) -> Result<(), RuntimeError>;
    /// Idempotent: deleting an already-absent pod is `Ok`.
    async fn delete(&self, name: &str) -> Result<(), RuntimeError>;
}

/// HTTP 409 (Conflict) on create means the named pod already exists.
fn classify_create_conflict(code: u16, name: &str) -> Option<RuntimeError> {
    if code == 409 {
        Some(RuntimeError::AlreadyExists(name.to_string()))
    } else {
        None
    }
}

/// HTTP 404 on delete means the pod is already gone — treat as success.
fn is_not_found(code: u16) -> bool {
    code == 404
}
```

- [ ] **Step 4: Add `FakePodApi` to the test module** (append inside `mod tests`)

```rust
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakePodApi {
        created: Mutex<Vec<Pod>>,
        deleted: Mutex<Vec<String>>,
        create_err: Mutex<Option<RuntimeError>>,
    }

    impl FakePodApi {
        fn fail_next_create(&self, e: RuntimeError) {
            *self.create_err.lock().unwrap() = Some(e);
        }
        fn created_names(&self) -> Vec<String> {
            self.created
                .lock()
                .unwrap()
                .iter()
                .filter_map(|p| p.metadata.name.clone())
                .collect()
        }
        fn deleted_names(&self) -> Vec<String> {
            self.deleted.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl PodApi for FakePodApi {
        async fn create(&self, pod: &Pod) -> Result<(), RuntimeError> {
            if let Some(e) = self.create_err.lock().unwrap().take() {
                return Err(e);
            }
            self.created.lock().unwrap().push(pod.clone());
            Ok(())
        }
        async fn delete(&self, name: &str) -> Result<(), RuntimeError> {
            self.deleted.lock().unwrap().push(name.to_string());
            Ok(())
        }
    }
```

- [ ] **Step 5: Run to verify pass**

Run: `cargo test -p executor --all-features classify_ is_not_found 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add executor/src/k8s_provider.rs
git commit -m "feat: add PodApi seam and kube error classifiers"
```

---

### Task 6: `KubernetesRuntimeHandle` (TDD)

**Files:**
- Modify: `executor/src/k8s_provider.rs`

- [ ] **Step 1: Write failing tests** (append inside `mod tests`)

```rust
    use crate::connected_registry::ConnectedRuntimeRegistry;
    use runtime_client::MockTransport;

    fn handle_with(
        registry: Arc<ConnectedRuntimeRegistry>,
        pod_api: Arc<dyn PodApi>,
    ) -> KubernetesRuntimeHandle {
        KubernetesRuntimeHandle {
            pod_name: "october-runtime-rt-1".to_string(),
            runtime_id: "rt-1".to_string(),
            pod_api,
            connected_registry: registry,
        }
    }

    #[tokio::test]
    async fn stop_deletes_pod_and_deregisters() {
        let registry = Arc::new(ConnectedRuntimeRegistry::new());
        registry
            .register_transport("rt-1".into(), Arc::new(MockTransport::ok("")))
            .await;
        let fake = Arc::new(FakePodApi::default());
        let handle = handle_with(registry.clone(), fake.clone());

        handle.stop().await.unwrap();

        assert_eq!(fake.deleted_names(), vec!["october-runtime-rt-1"]);
        assert!(registry.runtime_transport("rt-1").await.is_none());
    }

    #[tokio::test]
    async fn health_check_reflects_transport_presence() {
        let registry = Arc::new(ConnectedRuntimeRegistry::new());
        let fake = Arc::new(FakePodApi::default());
        let handle = handle_with(registry.clone(), fake);

        assert!(matches!(
            handle.health_check().await.unwrap(),
            HealthStatus::Unhealthy { .. }
        ));
        registry
            .register_transport("rt-1".into(), Arc::new(MockTransport::ok("")))
            .await;
        assert_eq!(handle.health_check().await.unwrap(), HealthStatus::Healthy);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p executor --all-features stop_deletes health_check_reflects 2>&1 | tail -20`
Expected: FAIL — `cannot find type KubernetesRuntimeHandle`.

- [ ] **Step 3: Implement the handle**

Insert above the test module:

```rust
/// Lifecycle handle for one runtime Pod. `stop` deletes the pod and deregisters the
/// transport; `health_check` is byte-identical to `ProcessRuntimeHandle` — it reports
/// `Healthy` while the runtime's dial-back transport is registered.
pub struct KubernetesRuntimeHandle {
    pod_name: String,
    runtime_id: String,
    pod_api: Arc<dyn PodApi>,
    connected_registry: Arc<ConnectedRuntimeRegistry>,
}

#[async_trait]
impl RuntimeHandle for KubernetesRuntimeHandle {
    async fn stop(&self) -> Result<(), RuntimeError> {
        // Always deregister, even if the delete call errors, so a failed API call
        // can't leave a stale transport that health checks would read as Healthy.
        let deleted = self.pod_api.delete(&self.pod_name).await;
        self.connected_registry.remove(&self.runtime_id).await;
        deleted
    }

    async fn health_check(&self) -> Result<HealthStatus, RuntimeError> {
        let connected = self
            .connected_registry
            .runtime_transport(&self.runtime_id)
            .await
            .is_some();
        if connected {
            Ok(HealthStatus::Healthy)
        } else {
            Ok(HealthStatus::Unhealthy {
                reason: "runtime disconnected".to_string(),
            })
        }
    }
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p executor --all-features stop_deletes health_check_reflects 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add executor/src/k8s_provider.rs
git commit -m "feat: add KubernetesRuntimeHandle"
```

---

### Task 7: `KubernetesRuntimeProvider::create` (TDD)

**Files:**
- Modify: `executor/src/k8s_provider.rs`

- [ ] **Step 1: Write failing tests** (append inside `mod tests`)

```rust
    fn provider_with(
        registry: Arc<ConnectedRuntimeRegistry>,
        pod_api: Arc<dyn PodApi>,
    ) -> KubernetesRuntimeProvider {
        KubernetesRuntimeProvider::new(
            "img:tag".to_string(),
            "october".to_string(),
            "ws://cb:9000".to_string(),
            registry,
            pod_api,
        )
    }

    fn cfg() -> RuntimeConfig {
        RuntimeConfig { working_dir: "/work".to_string() }
    }

    #[tokio::test]
    async fn create_succeeds_when_runtime_dials_back() {
        let registry = Arc::new(ConnectedRuntimeRegistry::new());
        let fake = Arc::new(FakePodApi::default());
        let provider = provider_with(registry.clone(), fake.clone());

        let task = tokio::spawn(async move { provider.create("rt-1", &cfg()).await });
        // Let create() install the readiness waiter and create the pod, then simulate
        // the runtime container dialing back by registering its transport.
        tokio::time::sleep(Duration::from_millis(50)).await;
        registry
            .register_transport("rt-1".into(), Arc::new(MockTransport::ok("")))
            .await;

        let handle = task.await.unwrap().unwrap();
        assert_eq!(handle.health_check().await.unwrap(), HealthStatus::Healthy);
        assert_eq!(fake.created_names(), vec!["october-runtime-rt-1"]);
    }

    #[tokio::test]
    async fn create_deletes_pod_on_timeout() {
        let registry = Arc::new(ConnectedRuntimeRegistry::new());
        let fake = Arc::new(FakePodApi::default());
        let provider =
            provider_with(registry.clone(), fake.clone()).with_connect_timeout(Duration::from_millis(50));

        // Never register a transport → create() must time out AND clean up the pod.
        let err = provider.create("rt-1", &cfg()).await.unwrap_err();
        assert!(matches!(err, RuntimeError::Provider(_)));
        assert_eq!(fake.deleted_names(), vec!["october-runtime-rt-1"]);
    }

    #[tokio::test]
    async fn create_propagates_conflict() {
        let registry = Arc::new(ConnectedRuntimeRegistry::new());
        let fake = Arc::new(FakePodApi::default());
        fake.fail_next_create(RuntimeError::AlreadyExists("october-runtime-rt-1".into()));
        let provider = provider_with(registry.clone(), fake.clone());

        let err = provider.create("rt-1", &cfg()).await.unwrap_err();
        assert!(matches!(err, RuntimeError::AlreadyExists(_)));
        // A create that never placed a pod must not issue a delete.
        assert!(fake.deleted_names().is_empty());
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p executor --all-features create_succeeds create_deletes create_propagates 2>&1 | tail -20`
Expected: FAIL — `cannot find type KubernetesRuntimeProvider`.

- [ ] **Step 3: Implement the provider**

Insert above the test module:

```rust
/// [`RuntimeProvider`] that launches each runtime as a Kubernetes Pod. Mirrors
/// [`ProcessRuntimeProvider`](crate::ProcessRuntimeProvider): register a readiness
/// waiter, create the pod, await the dial-back, return a handle. Because a Pod has no
/// `Drop`, `create` explicitly deletes the pod on every failure path.
pub struct KubernetesRuntimeProvider {
    image: String,
    namespace: String,
    /// `ws://` URL the pod dials back to — must be reachable from inside the cluster
    /// (Service DNS / NodePort), not the listener's bind address.
    callback_url: String,
    connected_registry: Arc<ConnectedRuntimeRegistry>,
    connect_timeout: Duration,
    pod_api: Arc<dyn PodApi>,
}

impl KubernetesRuntimeProvider {
    pub fn new(
        image: String,
        namespace: String,
        callback_url: String,
        connected_registry: Arc<ConnectedRuntimeRegistry>,
        pod_api: Arc<dyn PodApi>,
    ) -> Self {
        Self {
            image,
            namespace,
            callback_url,
            connected_registry,
            connect_timeout: Duration::from_secs(30),
            pod_api,
        }
    }

    pub fn with_connect_timeout(mut self, d: Duration) -> Self {
        self.connect_timeout = d;
        self
    }
}

#[async_trait]
impl RuntimeProvider for KubernetesRuntimeProvider {
    async fn create(
        &self,
        id: &str,
        config: &RuntimeConfig,
    ) -> Result<Arc<dyn RuntimeHandle>, RuntimeError> {
        // Register the readiness waiter BEFORE creating the pod, so the dial-back
        // signal cannot be lost (parity with ProcessRuntimeProvider).
        let ready_rx = self.connected_registry.notify_when_ready(id).await;

        let pod_name = sanitize_pod_name(id);
        let pod = build_pod_spec(
            &self.image,
            &self.namespace,
            &pod_name,
            id,
            &config.working_dir,
            &self.callback_url,
        );
        // A create failure (e.g. 409) means no pod was placed by us → propagate
        // without deleting.
        self.pod_api.create(&pod).await?;

        match tokio::time::timeout(self.connect_timeout, ready_rx).await {
            Ok(Ok(())) => Ok(Arc::new(KubernetesRuntimeHandle {
                pod_name,
                runtime_id: id.to_string(),
                pod_api: Arc::clone(&self.pod_api),
                connected_registry: Arc::clone(&self.connected_registry),
            })),
            Ok(Err(_)) => {
                // Readiness channel dropped before the runtime connected — clean up.
                let _ = self.pod_api.delete(&pod_name).await;
                Err(RuntimeError::Provider(
                    "connection channel dropped".to_string(),
                ))
            }
            Err(_) => {
                // No Drop on a Pod: explicitly delete so a never-connecting runtime
                // does not leak a pod the executor's retry loop would multiply.
                let _ = self.pod_api.delete(&pod_name).await;
                Err(RuntimeError::Provider(
                    "runtime connection timed out".to_string(),
                ))
            }
        }
    }
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p executor --all-features create_succeeds create_deletes create_propagates 2>&1 | tail -20`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add executor/src/k8s_provider.rs
git commit -m "feat: add KubernetesRuntimeProvider with create-path cleanup"
```

---

### Task 8: Real `KubePodApi` + public exports

**Files:**
- Modify: `executor/src/k8s_provider.rs`
- Modify: `executor/src/lib.rs`

- [ ] **Step 1: Implement `KubePodApi`** (insert above the test module)

```rust
use kube::api::{Api, DeleteParams, PostParams};
use kube::Client;

/// Production [`PodApi`] backed by `kube::Api<Pod>`. Built from the ambient cluster
/// config (in-cluster ServiceAccount or local kubeconfig). The error mapping lives
/// here, the one place `kube::Error` is in scope.
pub struct KubePodApi {
    api: Api<Pod>,
}

impl KubePodApi {
    /// Connect using the ambient kube config and scope to `namespace`.
    pub async fn namespaced(namespace: &str) -> Result<Self, RuntimeError> {
        let client = Client::try_default()
            .await
            .map_err(|e| RuntimeError::Provider(e.to_string()))?;
        Ok(Self {
            api: Api::namespaced(client, namespace),
        })
    }

    /// Build from an existing `kube::Api<Pod>` (e.g. a custom client/config).
    pub fn from_api(api: Api<Pod>) -> Self {
        Self { api }
    }
}

#[async_trait]
impl PodApi for KubePodApi {
    async fn create(&self, pod: &Pod) -> Result<(), RuntimeError> {
        match self.api.create(&PostParams::default(), pod).await {
            Ok(_) => Ok(()),
            Err(e) => {
                // `if let` (not `match`) on the non-exhaustive kube::Error keeps the
                // production lints happy without a wildcard arm.
                if let kube::Error::Api(resp) = &e {
                    let name = pod.metadata.name.clone().unwrap_or_default();
                    if let Some(mapped) = classify_create_conflict(resp.code, &name) {
                        return Err(mapped);
                    }
                }
                Err(RuntimeError::Provider(e.to_string()))
            }
        }
    }

    async fn delete(&self, name: &str) -> Result<(), RuntimeError> {
        match self.api.delete(name, &DeleteParams::default()).await {
            Ok(_) => Ok(()),
            Err(e) => {
                if let kube::Error::Api(resp) = &e {
                    if is_not_found(resp.code) {
                        return Ok(());
                    }
                }
                Err(RuntimeError::Provider(e.to_string()))
            }
        }
    }
}
```

Move the `use kube...` lines to the top import block if `cargo fmt` prefers; keep them compiling.

- [ ] **Step 2: Add feature-gated re-exports to `executor/src/lib.rs`**

After line 17 (`pub use process_provider::...`):

```rust
#[cfg(feature = "kubernetes")]
pub use k8s_provider::{KubePodApi, KubernetesRuntimeHandle, KubernetesRuntimeProvider, PodApi};
```

- [ ] **Step 3: Verify everything compiles and tests pass**

Run: `cargo test -p executor --all-features 2>&1 | tail -30`
Expected: PASS (all k8s tests + existing executor tests).

- [ ] **Step 4: Commit**

```bash
git add executor/src/k8s_provider.rs executor/src/lib.rs
git commit -m "feat: add kube-backed KubePodApi and export k8s provider"
```

---

### Task 9: Full pre-PR gate

**Files:** none (verification only)

- [ ] **Step 1: Format**

Run: `cargo fmt --all`
Then: `cargo fmt --all -- --check` → Expected: clean (no diff).

- [ ] **Step 2: Clippy (the CI command, verbatim)**

Run: `cargo +1.96.0 clippy --locked --all-targets --all-features -- -D warnings 2>&1 | tail -30`
Expected: no warnings. Fix any `unwrap_used`/`expect_used`/`panic`/`wildcard_enum_match_arm` in non-test code.

- [ ] **Step 3: Tests (the CI command, verbatim)**

Run: `cargo test --locked --workspace --all-features 2>&1 | tail -30`
Expected: PASS.

- [ ] **Step 4: Supply chain (the CI command, verbatim)**

Run: `cargo deny check advisories bans licenses sources 2>&1 | tail -30`
Expected: no errors (advisories/licenses/bans/sources all OK).

- [ ] **Step 5: Commit any fmt/lock fixes**

```bash
git add -A
git commit -m "chore: fmt + lockfile for k8s provider" || echo "nothing to commit"
```

---

## Self-review checklist (done before execution)

- **Spec coverage:** trait impl (Task 7), handle (Task 6), seam + kube backend (Tasks 5, 8), pure builders (Tasks 3, 4), deps/feature (Task 1), exports (Task 8), tests throughout, CI gate (Task 9). ✓
- **#1 invariant (anti-orphan):** `create_deletes_pod_on_timeout` asserts the delete on timeout (Task 7). ✓
- **Type consistency:** `sanitize_pod_name`, `build_pod_spec`, `classify_create_conflict`, `is_not_found`, `PodApi::{create,delete}`, `KubernetesRuntimeProvider::{new,with_connect_timeout,create}`, `KubernetesRuntimeHandle` fields — names used identically across tasks. ✓
- **Lint safety:** production code has no `unwrap`/`expect`/`panic`; kube error handling uses `if let` not a wildcard `match` arm; test module carries the standard `#[allow(...)]`. ✓
- **No placeholders:** every code step shows real code; every run step shows the command + expected result. ✓
