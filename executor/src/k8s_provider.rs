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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
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

    fn is_dns1123_label(s: &str) -> bool {
        !s.is_empty()
            && s.len() <= 63
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
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

    fn sample_pod() -> Pod {
        build_pod_spec(
            "img:tag",
            "october",
            "october-runtime-rt-1",
            "rt-1",
            "/work",
            "ws://cb:9000",
        )
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
                "--endpoint",
                "ws://cb:9000",
                "--runtime-id",
                "rt-1",
                "--working-dir",
                "/work",
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
        assert_eq!(
            labels.get("app.kubernetes.io/managed-by").map(String::as_str),
            Some("october-executor")
        );
        assert_eq!(
            labels.get("october.dev/runtime-id").map(String::as_str),
            Some("rt-1")
        );
    }

    #[test]
    fn pod_disables_service_account_automount() {
        let pod = sample_pod();
        assert_eq!(
            pod.spec.unwrap().automount_service_account_token,
            Some(false)
        );
    }

    #[test]
    fn pod_sets_name_namespace_image() {
        let pod = sample_pod();
        assert_eq!(pod.metadata.name.as_deref(), Some("october-runtime-rt-1"));
        assert_eq!(pod.metadata.namespace.as_deref(), Some("october"));
        assert_eq!(
            pod.spec.unwrap().containers[0].image.as_deref(),
            Some("img:tag")
        );
    }

    #[test]
    fn classify_create_conflict_maps_409_only() {
        assert!(matches!(
            classify_create_conflict(409, "p"),
            Some(RuntimeError::AlreadyExists(_))
        ));
        assert!(classify_create_conflict(404, "p").is_none());
        assert!(classify_create_conflict(500, "p").is_none());
    }

    #[test]
    fn is_not_found_only_404() {
        assert!(is_not_found(404));
        assert!(!is_not_found(409));
        assert!(!is_not_found(500));
    }

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
        RuntimeConfig {
            working_dir: "/work".to_string(),
        }
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
        let provider = provider_with(registry.clone(), fake.clone())
            .with_connect_timeout(Duration::from_millis(50));

        // Never register a transport → create() must time out AND clean up the pod.
        // (`matches!` rather than `unwrap_err()`: dyn RuntimeHandle isn't Debug.)
        let res = provider.create("rt-1", &cfg()).await;
        assert!(matches!(res, Err(RuntimeError::Provider(_))));
        assert_eq!(fake.deleted_names(), vec!["october-runtime-rt-1"]);
    }

    #[tokio::test]
    async fn create_propagates_conflict() {
        let registry = Arc::new(ConnectedRuntimeRegistry::new());
        let fake = Arc::new(FakePodApi::default());
        fake.fail_next_create(RuntimeError::AlreadyExists("october-runtime-rt-1".into()));
        let provider = provider_with(registry.clone(), fake.clone());

        let res = provider.create("rt-1", &cfg()).await;
        assert!(matches!(res, Err(RuntimeError::AlreadyExists(_))));
        // A create that never placed a pod must not issue a delete.
        assert!(fake.deleted_names().is_empty());
    }
}
