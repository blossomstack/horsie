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
    RuntimeSpec, RuntimeVendor, VendorCapabilities, VendorError, VendorRuntime, VendorRuntimeHandle,
};
use async_trait::async_trait;
use horsie_executor::{ConnectHook, ConnectedRuntimeRegistry};
use horsie_runtime_client::RuntimeClient;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// One connected daemon's vendor identity. Never spawns anything: `create`/
/// `attach` look up whatever's currently registered for `label` in the
/// shared [`ConnectedRuntimeRegistry`] and hand back a client wrapping it.
pub struct LocalDaemonVendor {
    label: String,
    connected: Arc<ConnectedRuntimeRegistry>,
}

impl LocalDaemonVendor {
    /// Reject inputs this vendor can't honor: it never provisions (no
    /// `git_checkout`, no bundle install) — the daemon's own directory is
    /// implicit and fixed. The common case — no `repos`/provision steps in
    /// the request — leaves the daemon's own dir untouched, which is exactly
    /// what this vendor does. This enforces the `supports_provisioning: false`
    /// it announces in [`capabilities`](RuntimeVendor::capabilities); the UI
    /// won't send provisioning here, so this is a backstop.
    fn reject_unsupported_inputs(spec: &RuntimeSpec) -> Result<(), String> {
        if !spec.provision.is_empty() {
            return Err("shared local runtime vendor does not support provisioning".to_string());
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
    fn capabilities(&self) -> VendorCapabilities {
        // Runs in the connected daemon's own fixed directory; provisions
        // nothing (no repo clone, no bundle install).
        VendorCapabilities {
            supports_provisioning: false,
        }
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

/// Registers each "local" daemon that dials the shared HTTP
/// `/api/runtime/connect` route (with `?register=local`) as a vendor: it mirrors
/// every newly (or re-)connected label into `ServerDeps.vendors` so sessions can
/// select it by name exactly like any other vendor. Owns no listener — it hands
/// the HTTP route a [`ConnectHook`] via [`hook`](Self::hook) that fires on each
/// successful registration into the shared [`ConnectedRuntimeRegistry`].
pub struct LocalDaemonRegistry {
    connected: Arc<ConnectedRuntimeRegistry>,
    local_vendors: Arc<RwLock<HashMap<String, Arc<LocalDaemonVendor>>>>,
    hook: ConnectHook,
}

impl LocalDaemonRegistry {
    /// Build the registry over the server-wide `connected` registry. `vendors`
    /// is the same map session lookups read (`ServerDeps.vendors`) — every
    /// connected label is inserted into it as it announces itself. No listener
    /// is bound; the HTTP route drives connections through [`hook`](Self::hook).
    pub fn new(connected: Arc<ConnectedRuntimeRegistry>, vendors: SharedVendors) -> Self {
        let local_vendors: Arc<RwLock<HashMap<String, Arc<LocalDaemonVendor>>>> =
            Arc::new(RwLock::new(HashMap::new()));

        let hook_connected = connected.clone();
        let hook_local_vendors = local_vendors.clone();
        let hook_vendors = vendors;
        let hook: ConnectHook = Arc::new(move |label: String| {
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
                        })
                    })
                    .clone()
            };
            let mut all = hook_vendors.write().unwrap_or_else(|e| e.into_inner());
            all.entry(label)
                .or_insert_with(|| vendor.clone() as Arc<dyn RuntimeVendor>);
        });

        Self {
            connected,
            local_vendors,
            hook,
        }
    }

    /// The connect hook the HTTP `/api/runtime/connect?register=local` handler
    /// passes to `handle_runtime_connection`; it fires once per successful
    /// registration with the `label`.
    pub fn hook(&self) -> ConnectHook {
        self.hook.clone()
    }

    /// The label's vendor object, if a daemon has ever announced it (whether
    /// currently connected or not). Used by tests today; a future
    /// connected-instance listing endpoint would read this the same way.
    pub fn vendor(&self, label: &str) -> Option<Arc<LocalDaemonVendor>> {
        self.local_vendors
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(label)
            .cloned()
    }

    /// Whether `label` currently has a live connection.
    pub async fn is_connected(&self, label: &str) -> bool {
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
    use horsie_executor::{
        RuntimeEndpoint, RuntimeListenerServer, serve_runtime_connections_with_hook,
    };
    use horsie_models::runtime::{
        BashInput, RuntimeInboundMessage, RuntimeOutboundMessage, RuntimeReady, ToolCall,
        ToolCallResponse, ToolOutput, ToolResult,
    };
    use std::net::SocketAddr;
    use std::time::Duration;
    use tokio::task::JoinHandle;
    use tokio_tungstenite::{connect_async, tungstenite::Message};
    use tokio_util::sync::CancellationToken;

    fn empty_vendors() -> SharedVendors {
        Arc::new(RwLock::new(HashMap::new()))
    }

    /// Test wrapper that binds a real listener onto the registry's shared
    /// `connected` map with the registry's hook — standing in for the HTTP
    /// `/api/runtime/connect?register=local` route — and exposes the same
    /// accessors the tests used before the listener moved into HTTP.
    struct TestHarness {
        registry: LocalDaemonRegistry,
        connected: Arc<ConnectedRuntimeRegistry>,
        addr: SocketAddr,
    }

    impl TestHarness {
        fn listen_addr(&self) -> SocketAddr {
            self.addr
        }
        fn vendor(&self, label: &str) -> Option<Arc<LocalDaemonVendor>> {
            self.registry.vendor(label)
        }
        async fn is_connected(&self, label: &str) -> bool {
            self.connected.runtime_transport(label).await.is_some()
        }
    }

    fn test_spec() -> RuntimeSpec {
        RuntimeSpec {
            workspaces: vec![WorkspaceSpec {
                name: "main".into(),
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
    fn spawn_fake_daemon(addr: SocketAddr, label: String, reply: String) -> JoinHandle<()> {
        tokio::spawn(async move {
            let (ws, _) = match connect_async(format!("ws://{addr}")).await {
                Ok(x) => x,
                Err(_) => return,
            };
            let (mut sink, mut stream) = ws.split();
            let ready = serde_json::to_string(&RuntimeOutboundMessage::Ready(RuntimeReady {
                runtime_id: label,
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

    async fn bind_registry() -> TestHarness {
        bind_registry_with(empty_vendors()).await
    }

    async fn bind_registry_with(vendors: SharedVendors) -> TestHarness {
        let connected = Arc::new(ConnectedRuntimeRegistry::new());
        let registry = LocalDaemonRegistry::new(connected.clone(), vendors);
        let listener =
            RuntimeListenerServer::bind(RuntimeEndpoint::Tcp("127.0.0.1:0".parse().unwrap()))
                .await
                .expect("bind listener");
        let addr = listener.tcp_addr().expect("tcp addr");
        serve_runtime_connections_with_hook(
            listener,
            connected.clone(),
            CancellationToken::new(),
            Some(registry.hook()),
        );
        TestHarness {
            registry,
            connected,
            addr,
        }
    }

    async fn wait_connected(registry: &TestHarness, label: &str) {
        for _ in 0..50 {
            if registry.is_connected(label).await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("'{label}' never connected within 1s");
    }

    async fn wait_disconnected(registry: &TestHarness, label: &str) {
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
        let daemon = spawn_fake_daemon(registry.listen_addr(), "my-laptop".into(), "ok".into());
        wait_connected(&registry, "my-laptop").await;
        registry.vendor("my-laptop").expect("vendor registered");
        daemon.abort();
    }

    #[tokio::test]
    async fn create_and_attach_from_different_sessions_share_one_connection() {
        let registry = bind_registry().await;
        let daemon = spawn_fake_daemon(registry.listen_addr(), "shared".into(), "shared-ok".into());
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
        let daemon1 = spawn_fake_daemon(registry.listen_addr(), "dup".into(), "one".into());
        wait_connected(&registry, "dup").await;
        let daemon2 = spawn_fake_daemon(registry.listen_addr(), "dup".into(), "two".into());
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
        let daemon1 = spawn_fake_daemon(registry.listen_addr(), "resumable".into(), "first".into());
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

        let daemon2 =
            spawn_fake_daemon(registry.listen_addr(), "resumable".into(), "second".into());
        wait_connected(&registry, "resumable").await;
        let vendor_after = registry
            .vendor("resumable")
            .expect("vendor still registered");
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
    async fn announces_no_provisioning_and_rejects_provision_steps() {
        let registry = bind_registry().await;
        let daemon = spawn_fake_daemon(registry.listen_addr(), "strict".into(), "ok".into());
        wait_connected(&registry, "strict").await;
        let vendor = registry.vendor("strict").expect("vendor registered");

        assert!(
            !vendor.capabilities().supports_provisioning,
            "the shared local vendor announces it cannot provision"
        );

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
        daemon.abort();
    }
}
