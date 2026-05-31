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
}
