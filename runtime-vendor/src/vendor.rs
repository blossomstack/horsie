//! The reusable half of a runtime vendor agent.
//!
//! An agent owns runtime lifecycle for one vendor: it dials the server's
//! `/api/vendor/connect`, announces itself, and thereafter serves
//! [`RuntimeVendorCommand`]s by driving a [`RuntimeProvider`] and relaying the
//! runtime protocol, verbatim, to the runtimes that dialed its own listener.
//!
//! Only lifecycle is decoded here. Anything addressed to a runtime is forwarded
//! untouched in both directions, so a new runtime capability needs no change in
//! this file.
//!
//! Everything vendor-specific lives behind two seams — the `RuntimeProvider`
//! (spawn a process, schedule a container, …) and the [`WorkspaceResolver`]
//! (turn a requested workspace *name* into a path this vendor owns). A new
//! vendor implements those two and reuses this loop verbatim.

use crate::{
    connected_registry::ConnectedRuntimeRegistry,
    provider::{RuntimeHandle, RuntimeProvider},
    reconnect::Backoff,
};
use futures_util::{SinkExt, StreamExt};
use horsie_models::executor::{EnvVar, RuntimeConfig, RuntimeInfo, RuntimeState, WorkspaceConfig};
use horsie_models::runtime::RuntimeInboundMessage;
use horsie_models::runtime_vendor::{
    AttachRuntimeResponse, CreateRuntimeResponse, DeleteRuntimeResponse, QueryRuntimesResponse,
    RequestFailed, RuntimeRelayRequest, RuntimeRelayResponse, RuntimeSpec,
    RuntimeVendorCapabilities, RuntimeVendorCommand, RuntimeVendorEvent,
    RuntimeVendorInboundMessage, RuntimeVendorOutboundMessage, RuntimeVendorReady,
    StopRuntimeResponse,
};
use horsie_runtime_client::RuntimeTransport;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_util::sync::CancellationToken;

/// Turns a requested workspace name into a path this vendor owns.
///
/// The server sends names, never paths: it cannot know an agent's filesystem.
/// An agent that cannot honor a name returns `None`, which fails the command
/// explicitly rather than silently substituting a directory.
pub trait WorkspaceResolver: Send + Sync + 'static {
    fn resolve(&self, name: &str) -> Option<PathBuf>;
}

/// Builds a [`RuntimeProvider`] for one runtime.
///
/// A factory rather than a single shared provider because the sandbox policy is
/// per-runtime and [`ProcessRuntimeProvider`](crate::ProcessRuntimeProvider)
/// binds its capability file at construction. `caps_file` is `Some` when the
/// agent wrote the server-sent spec to disk and sandboxing is enabled.
pub type ProviderFactory =
    Arc<dyn Fn(&str, Option<PathBuf>) -> Arc<dyn RuntimeProvider> + Send + Sync>;

/// A resolver over a fixed name→path table, as `horsie connect --workspace`
/// builds from its arguments.
pub struct FixedWorkspaces {
    paths: HashMap<String, PathBuf>,
}

impl FixedWorkspaces {
    #[must_use]
    pub fn new(paths: HashMap<String, PathBuf>) -> Self {
        Self { paths }
    }

    #[must_use]
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.paths.keys().cloned().collect();
        names.sort();
        names
    }
}

impl WorkspaceResolver for FixedWorkspaces {
    fn resolve(&self, name: &str) -> Option<PathBuf> {
        self.paths.get(name).cloned()
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

type Sink = Arc<Mutex<futures_util::stream::SplitSink<Socket, Message>>>;

type Stream = futures_util::stream::SplitStream<Socket>;

/// How one link to the server ended — the only thing the reconnect loop
/// branches on.
enum LinkEnd {
    /// The agent was asked to shut down. Its runtimes go with it.
    Cancelled,
    /// The socket died. Says nothing about the runtimes, which keep running.
    Disconnected,
}

/// Where and how an agent's runtimes materialize server-managed bundles.
pub struct BundleDelivery {
    /// Base URL reaching the server *from where the runtimes run* — loopback
    /// for a local agent, an advertise address for a remote one.
    pub base_url: String,
    /// Path the runtime unpacks bundles into and scans.
    pub dir: String,
    /// Optional content-hash cache, so repeat sessions skip re-fetching.
    pub cache_dir: Option<String>,
}

/// One live runtime this agent owns.
///
/// The transport rather than a `RuntimeClient`: this agent never interprets what
/// it forwards, so the typed client would only be a layer to unwrap.
struct LiveRuntime {
    handle: Arc<dyn RuntimeHandle>,
    transport: Arc<dyn RuntimeTransport>,
}

pub struct RuntimeVendor {
    vendor_name: String,
    supports_provisioning: bool,
    provider: ProviderFactory,
    connected: Arc<ConnectedRuntimeRegistry>,
    workspaces: Arc<dyn WorkspaceResolver>,
    /// Where per-runtime scratch (the sandbox capability file) is written.
    state_dir: PathBuf,
    /// Whether to honor the server's sandbox spec. Off by default so the local
    /// vendor keeps behaving as it does today, where the machine is already the
    /// user's own; `horsie connect --sandbox` turns it on.
    sandbox: bool,
    /// The host plugin library this agent serves, resolved agent-side by the
    /// CLI. Never sent by the server — it is a property of this machine.
    host_library: Option<PathBuf>,
    hook_path: Vec<PathBuf>,
    /// How this agent's runtimes fetch server-managed bundles: the base URL
    /// that reaches the server from where they run, the directory they unpack
    /// into, and an optional content-hash cache. All three are the agent's
    /// knowledge, not the server's — it sends only hashes and a token.
    bundles: Option<BundleDelivery>,
    /// How long to wait between connection attempts. A field rather than a
    /// constant so tests can run the reconnect path on a millisecond scale.
    backoff: Backoff,
    runtimes: Arc<Mutex<HashMap<String, LiveRuntime>>>,
}

impl RuntimeVendor {
    #[must_use]
    pub fn new(
        vendor_name: String,
        supports_provisioning: bool,
        provider: ProviderFactory,
        connected: Arc<ConnectedRuntimeRegistry>,
        workspaces: Arc<dyn WorkspaceResolver>,
        state_dir: PathBuf,
    ) -> Self {
        Self {
            vendor_name,
            supports_provisioning,
            provider,
            connected,
            workspaces,
            state_dir,
            sandbox: false,
            host_library: None,
            hook_path: Vec::new(),
            bundles: None,
            backoff: Backoff::default(),
            runtimes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Honor the server's sandbox capability spec for every runtime.
    #[must_use]
    pub fn with_sandbox(mut self, enabled: bool) -> Self {
        self.sandbox = enabled;
        self
    }

    /// Reconnect on a schedule other than [`Backoff::default`]. Tests use a
    /// millisecond-scale one; nothing in production has a reason to.
    #[must_use]
    pub fn with_backoff(mut self, backoff: Backoff) -> Self {
        self.backoff = backoff;
        self
    }

    /// Let this agent's runtimes fetch server-managed bundles.
    #[must_use]
    pub fn with_bundles(mut self, delivery: BundleDelivery) -> Self {
        self.bundles = Some(delivery);
        self
    }

    /// Serve a host plugin library to every runtime this agent spawns.
    #[must_use]
    pub fn with_host_library(mut self, dir: PathBuf, hook_path: Vec<PathBuf>) -> Self {
        self.host_library = Some(dir);
        self.hook_path = hook_path;
        self
    }

    /// Serve this vendor until `cancel` fires, reconnecting whenever the link
    /// to the server dies.
    ///
    /// The link is the *only* thing a disconnect destroys. The runtime
    /// listener, the [`ConnectedRuntimeRegistry`] it feeds, and every runtime
    /// this agent spawned are owned outside the link and deliberately outlive
    /// it: a dead socket to the server says nothing about the sandboxes running
    /// here, and rebinding the listener would both risk "address in use" and
    /// strand the runtimes currently dialed into it. Runtimes die on
    /// cancellation, not on disconnection.
    ///
    /// The server, for its part, does not ask what survived when the same
    /// vendor name re-registers — it re-creates or re-attaches lazily on the
    /// next turn. Reconciling the two views with `QueryRuntimes` is issue #92
    /// item 4 and deliberately not done here.
    ///
    /// The only `Err` is fail-fast: a `server_url` no attempt could ever dial.
    /// Dial, handshake and transport failures are retried indefinitely.
    pub async fn run(self, server_url: &str, cancel: CancellationToken) -> Result<(), String> {
        // Reject an undialable URL before the first attempt, so a typo is an
        // error the operator sees once rather than a retry loop that can never
        // succeed.
        server_url
            .into_client_request()
            .map_err(|e| format!("invalid server URL '{server_url}': {e}"))?;

        let mut backoff = self.backoff;
        let mut failures: u32 = 0;
        let mut connections: u32 = 0;
        let agent = Arc::new(self);

        loop {
            let ended = match agent.connect(server_url).await {
                Ok((sink, stream)) => {
                    connections = connections.saturating_add(1);
                    failures = 0;
                    // A fresh incident from here on, however long the last
                    // streak of failures waited.
                    backoff.reset();
                    if connections > 1 {
                        note(&format!(
                            "vendor agent: reconnected to {server_url} as \"{}\"",
                            agent.vendor_name
                        ));
                    }
                    Ok(agent.serve(sink, stream, &cancel).await)
                }
                Err(e) => {
                    failures = failures.saturating_add(1);
                    Err(e)
                }
            };

            let reason = match ended {
                Ok(LinkEnd::Cancelled) => break,
                Ok(LinkEnd::Disconnected) => format!("lost the link to {server_url}"),
                Err(e) => format!("attempt {failures} failed: {e}"),
            };
            let delay = backoff.next_delay();
            note(&format!(
                "vendor agent: {reason}; reconnecting in {:.1}s",
                delay.as_secs_f64()
            ));
            // Cancellation must not have to wait out a 30s delay to be heard.
            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep(delay) => {}
            }
        }

        // Kill every runtime we spawned. `tokio::process::Child` does not kill
        // on drop, so without this an agent shutdown (Ctrl-C, SIGTERM) would
        // orphan one `horsie-runtime` per live session. A mere server hangup
        // does not reach here — that is the point of the loop above.
        agent.halt_all().await;
        Ok(())
    }

    /// Dial the server and announce this vendor. Both halves are one step: a
    /// socket that never got its `Ready` across is no more usable than one that
    /// never opened, and the reconnect loop treats them identically.
    async fn connect(&self, server_url: &str) -> Result<(Sink, Stream), String> {
        let (ws, _) = tokio_tungstenite::connect_async(server_url)
            .await
            .map_err(|e| format!("connect {server_url}: {e}"))?;
        let (sink_inner, stream) = ws.split();
        let sink: Sink = Arc::new(Mutex::new(sink_inner));
        send(
            &sink,
            RuntimeVendorOutboundMessage {
                request_id: "boot".to_string(),
                event: RuntimeVendorEvent::Ready(RuntimeVendorReady {
                    vendor_name: self.vendor_name.clone(),
                    capabilities: RuntimeVendorCapabilities {
                        supports_provisioning: self.supports_provisioning,
                    },
                }),
            },
        )
        .await?;
        Ok((sink, stream))
    }

    /// Serve commands over one live link until it dies or `cancel` fires.
    async fn serve(
        self: &Arc<Self>,
        sink: Sink,
        mut stream: Stream,
        cancel: &CancellationToken,
    ) -> LinkEnd {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return LinkEnd::Cancelled,
                next = stream.next() => {
                    let Some(next) = next else { return LinkEnd::Disconnected };
                    let text = match next {
                        Ok(Message::Text(text)) => text,
                        Ok(Message::Binary(_))
                        | Ok(Message::Ping(_))
                        | Ok(Message::Pong(_))
                        | Ok(Message::Frame(_)) => continue,
                        Ok(Message::Close(_)) | Err(_) => return LinkEnd::Disconnected,
                    };
                    let Ok(inbound) = serde_json::from_str::<RuntimeVendorInboundMessage>(&text) else {
                        note("vendor agent: undecodable command, ignoring");
                        continue;
                    };
                    // Each command runs on its own task: a bash tool call can
                    // legitimately run for minutes, and blocking the read loop
                    // on it would stall every other session on this agent.
                    let agent = self.clone();
                    let sink = sink.clone();
                    tokio::spawn(async move {
                        agent.dispatch(inbound, sink).await;
                    });
                }
            }
        }
    }

    async fn dispatch(&self, inbound: RuntimeVendorInboundMessage, sink: Sink) {
        let request_id = inbound.request_id;
        let outcome = match inbound.command {
            RuntimeVendorCommand::CreateRuntime(cmd) => {
                let created = self.provision(&cmd.runtime_id, &cmd.spec).await;
                created.map(|()| {
                    RuntimeVendorEvent::CreateRuntime(CreateRuntimeResponse {
                        runtime_id: cmd.runtime_id,
                    })
                })
            }
            RuntimeVendorCommand::AttachRuntime(cmd) => {
                let attached = self.provision(&cmd.runtime_id, &cmd.spec).await;
                attached.map(|()| {
                    RuntimeVendorEvent::AttachRuntime(AttachRuntimeResponse {
                        runtime_id: cmd.runtime_id,
                    })
                })
            }
            RuntimeVendorCommand::StopRuntime(cmd) => {
                self.halt(&cmd.runtime_id).await;
                Ok(RuntimeVendorEvent::StopRuntime(StopRuntimeResponse {
                    runtime_id: cmd.runtime_id,
                }))
            }
            RuntimeVendorCommand::DeleteRuntime(cmd) => {
                self.halt(&cmd.runtime_id).await;
                Ok(RuntimeVendorEvent::DeleteRuntime(DeleteRuntimeResponse {
                    runtime_id: cmd.runtime_id,
                }))
            }
            RuntimeVendorCommand::QueryRuntimes(_) => {
                let runtimes = self
                    .runtimes
                    .lock()
                    .await
                    .keys()
                    .map(|id| RuntimeInfo {
                        runtime_id: id.clone(),
                        state: RuntimeState::Running,
                        restart_count: 0,
                    })
                    .collect();
                Ok(RuntimeVendorEvent::QueryRuntimes(QueryRuntimesResponse {
                    runtimes,
                }))
            }
            RuntimeVendorCommand::Runtime(cmd) => match self.relay(cmd).await {
                Some(outcome) => outcome,
                // One-way by protocol: the server is not waiting.
                None => return,
            },
        };

        let event = match outcome {
            Ok(event) => event,
            Err(message) => RuntimeVendorEvent::RequestFailed(RequestFailed { message }),
        };
        let _ = send(&sink, RuntimeVendorOutboundMessage { request_id, event }).await;
    }

    /// Forward a runtime message to the runtime it names, verbatim in both
    /// directions. This agent does not decode what it carries, which is why
    /// adding a runtime capability costs nothing here.
    ///
    /// `None` means the message draws no reply — `CancelCall`, matching the
    /// runtime link itself, where a cancel is fire-and-forget.
    async fn relay(
        &self,
        request: RuntimeRelayRequest,
    ) -> Option<Result<RuntimeVendorEvent, String>> {
        let RuntimeRelayRequest {
            runtime_id,
            message,
        } = request;
        let oneway = matches!(message, RuntimeInboundMessage::CancelCall(_));
        let Some(transport) = self.transport_for(&runtime_id).await else {
            if oneway {
                return None;
            }
            return Some(Err(format!(
                "no live runtime '{runtime_id}' on this vendor"
            )));
        };
        if oneway {
            let _ = transport.send_oneway(message).await;
            return None;
        }
        Some(match transport.relay(message).await {
            Ok(message) => Ok(RuntimeVendorEvent::Runtime(RuntimeRelayResponse {
                runtime_id,
                message,
            })),
            Err(e) => Err(format!("relay to runtime '{runtime_id}': {e}")),
        })
    }

    /// Create or revive a runtime. Both paths are identical for a process-backed
    /// vendor — the workspace is on disk and survives — so `attach` re-spawns
    /// against the same resolved directory.
    async fn provision(&self, runtime_id: &str, request: &RuntimeSpec) -> Result<(), String> {
        let mut workspaces = Vec::with_capacity(request.workspaces.len());
        for name in &request.workspaces {
            let path = self.workspaces.resolve(name).ok_or_else(|| {
                format!("this vendor has no workspace named '{name}'; it serves only its own configured directories")
            })?;
            workspaces.push(WorkspaceConfig {
                name: name.clone(),
                path: path.to_string_lossy().into_owned(),
            });
        }

        // A previous incarnation may still be live (a re-create after a crash).
        self.halt(runtime_id).await;

        // The sandbox spec arrives inline; a provider needs it as a file.
        let caps_file = match (&request.sandbox_capabilities, self.sandbox) {
            (Some(spec), true) => {
                let path = self.caps_path(runtime_id);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("create runtime state dir: {e}"))?;
                }
                let bytes = serde_json::to_vec_pretty(spec)
                    .map_err(|e| format!("encode capability spec: {e}"))?;
                std::fs::write(&path, bytes).map_err(|e| format!("write capability file: {e}"))?;
                Some(path)
            }
            (None, _) | (Some(_), false) => None,
        };

        let mut env = request.env.clone();
        if let Some(b) = &self.bundles {
            env.push(EnvVar {
                name: horsie_models::ENV_PLUGINS_BASE.to_string(),
                value: b.base_url.clone(),
            });
            env.push(EnvVar {
                name: horsie_models::ENV_PLUGINS_DIR.to_string(),
                value: b.dir.clone(),
            });
            if let Some(cache) = &b.cache_dir {
                env.push(EnvVar {
                    name: horsie_models::ENV_PLUGINS_CACHE.to_string(),
                    value: cache.clone(),
                });
            }
        }

        let config = RuntimeConfig {
            workspaces,
            plugins_dir: self
                .host_library
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            hook_path: self
                .hook_path
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
            env,
            provision: request.provision.clone(),
        };
        let handle = (self.provider)(runtime_id, caps_file)
            .create(runtime_id, &config)
            .await
            .map_err(|e| e.to_string())?;
        let transport = self
            .connected
            .runtime_transport(runtime_id)
            .await
            .ok_or_else(|| format!("runtime '{runtime_id}' started but never dialed back"))?;
        self.runtimes
            .lock()
            .await
            .insert(runtime_id.to_string(), LiveRuntime { handle, transport });
        Ok(())
    }

    /// Stop every runtime this agent owns. Used on shutdown.
    async fn halt_all(&self) {
        let live: Vec<(String, Arc<dyn RuntimeHandle>)> = {
            let mut guard = self.runtimes.lock().await;
            guard.drain().map(|(id, r)| (id, r.handle)).collect()
        };
        for (id, handle) in live {
            if let Err(e) = handle.stop().await {
                note(&format!("vendor agent: stopping runtime '{id}': {e}"));
            }
        }
    }

    async fn halt(&self, runtime_id: &str) {
        if let Some(live) = self.runtimes.lock().await.remove(runtime_id) {
            let _ = live.handle.stop().await;
        }
    }

    async fn transport_for(&self, runtime_id: &str) -> Option<Arc<dyn RuntimeTransport>> {
        self.runtimes
            .lock()
            .await
            .get(runtime_id)
            .map(|r| r.transport.clone())
    }

    /// Where this agent writes a runtime's sandbox capability file. Public so
    /// the process provider can be pointed at the same path.
    #[must_use]
    pub fn caps_path(&self, runtime_id: &str) -> PathBuf {
        self.state_dir.join(runtime_id).join("capabilities.json")
    }
}

async fn send(sink: &Sink, msg: RuntimeVendorOutboundMessage) -> Result<(), String> {
    let json = serde_json::to_string(&msg).map_err(|e| format!("encode event: {e}"))?;
    sink.lock()
        .await
        .send(Message::Text(json.into()))
        .await
        .map_err(|e| format!("send event: {e}"))
}

/// This crate has no tracing dependency, and these lines are addressed to
/// whoever is watching the agent — a terminal, or a process manager's log — so
/// they go straight to stderr rather than through a subscriber that a vendor
/// binary may never have installed.
fn note(message: &str) {
    eprintln!("{message}");
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
    use crate::reconnect::Backoff;

    struct NeverProvider;

    #[async_trait::async_trait]
    impl RuntimeProvider for NeverProvider {
        async fn create(
            &self,
            _id: &str,
            _config: &RuntimeConfig,
        ) -> Result<Arc<dyn RuntimeHandle>, crate::error::RuntimeError> {
            panic!("an agent that never connects must never provision")
        }
    }

    fn agent() -> RuntimeVendor {
        RuntimeVendor::new(
            "test-vendor".to_string(),
            false,
            Arc::new(|_id: &str, _caps: Option<PathBuf>| Arc::new(NeverProvider)),
            Arc::new(ConnectedRuntimeRegistry::new()),
            Arc::new(FixedWorkspaces::new(HashMap::new())),
            PathBuf::from("/tmp/horsie-vendor-test"),
        )
        // Long enough that a retry would visibly hang the test if the URL were
        // treated as retryable.
        .with_backoff(Backoff::new(
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(60),
        ))
    }

    #[tokio::test]
    async fn an_undialable_url_fails_before_any_attempt_instead_of_retrying_forever() {
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            agent().run(
                "ws://not a host/api/vendor/connect",
                CancellationToken::new(),
            ),
        )
        .await
        .expect("a URL no attempt could succeed with must fail fast, not back off")
        .expect_err("an unparseable server URL is an operator error, not an outage");
        assert!(err.contains("invalid server URL"), "{err}");
    }

    #[tokio::test]
    async fn a_cancelled_agent_stops_without_waiting_out_the_backoff() {
        let cancel = CancellationToken::new();
        // Port 1 on loopback: the dial fails immediately, so the agent is in its
        // 60s backoff sleep by the time the cancel lands.
        let run = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                agent()
                    .run("ws://127.0.0.1:1/api/vendor/connect", cancel)
                    .await
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        cancel.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(5), run)
            .await
            .expect("the backoff sleep must give way to cancellation")
            .expect("the agent task must not panic")
            .expect("a cancelled agent exits cleanly");
    }
}
