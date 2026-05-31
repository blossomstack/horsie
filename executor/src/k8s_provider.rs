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
}
