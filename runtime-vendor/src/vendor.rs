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
    error::CredentialError,
    provider::{RuntimeHandle, RuntimeProvider},
    reconnect::Backoff,
};
use futures_util::{SinkExt, StreamExt};
use horsie_models::capabilities::{Access, DirGrant, Grant};
use horsie_models::executor::{EnvVar, RuntimeConfig, RuntimeInfo, RuntimeState, WorkspaceConfig};
use horsie_models::runtime::RuntimeInboundMessage;
use horsie_models::runtime_vendor::{
    CreateRuntimeResponse, DeleteRuntimeResponse, GetRuntimeResponse, HibernateRuntimeResponse,
    QueryRuntimesResponse, RequestFailed, RuntimeRelayRequest, RuntimeRelayResponse, RuntimeSpec,
    RuntimeVendorCapabilities, RuntimeVendorCommand, RuntimeVendorEvent,
    RuntimeVendorInboundMessage, RuntimeVendorOutboundMessage, RuntimeVendorReady,
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
/// binds its capability file at construction. `caps_file` is `Some` when
/// sandboxing is enabled and the agent wrote its baseline spec to disk.
pub type ProviderFactory =
    Arc<dyn Fn(&str, Option<PathBuf>) -> Arc<dyn RuntimeProvider> + Send + Sync>;

/// Produces the bearer for one dial attempt.
///
/// A closure rather than a string because the answer changes over the life of
/// an agent: a CLI credential is refreshed against the server, while a machine
/// token is a constant. Both are the same shape here, and the reconnect loop
/// does not care which it has.
pub type CredentialProvider = Arc<
    dyn Fn() -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Option<String>, CredentialError>> + Send>,
        > + Send
        + Sync,
>;

/// A provider that presents no bearer at all — correct against a server running
/// with authentication disabled, and the shape tests use.
#[must_use]
pub fn no_credential() -> CredentialProvider {
    Arc::new(|| Box::pin(async { Ok(None) }))
}

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
    /// Whether to sandbox spawned runtimes with the vendor's baseline
    /// capability spec. The library default is off; `horsie connect` turns it
    /// on unless started with `--no-sandbox`.
    sandbox: bool,
    /// The host plugin library this agent serves, resolved agent-side by the
    /// CLI. Never sent by the server — it is a property of this machine.
    host_library: Option<PathBuf>,
    /// Root of the shared clones the host library's symlinks point into. The
    /// sandbox resolves through symlinks, so this must be granted alongside the
    /// library itself or the runtime cannot read any installed plugin.
    host_sources: Option<PathBuf>,
    hook_path: Vec<PathBuf>,
    /// How this agent's runtimes fetch server-managed bundles: the base URL
    /// that reaches the server from where they run, the directory they unpack
    /// into, and an optional content-hash cache. All three are the agent's
    /// knowledge, not the server's — it sends only hashes and a token.
    bundles: Option<BundleDelivery>,
    /// How long to wait between connection attempts. A field rather than a
    /// constant so tests can run the reconnect path on a millisecond scale.
    backoff: Backoff,
    /// Whether a get for a runtime that is not live may rebuild it from its
    /// persisted spec. See [`RuntimeVendor::with_respawnable_runtimes`].
    respawnable: bool,
    runtimes: Arc<Mutex<HashMap<String, LiveRuntime>>>,
    /// One lock per runtime id, held for the whole of a lifecycle command.
    ///
    /// Commands dispatch on their own tasks, so without this a `GetRuntime`
    /// arriving while its `CreateRuntime` is still provisioning would answer
    /// "gone" for a runtime that is moments from existing. The server's
    /// contract says a get waits for an in-flight create, and this is where
    /// that promise is kept.
    lifecycle_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

/// The WS upgrade request, carrying the bearer when there is one. Built in one
/// place so the fail-fast validation in `run` and the real dial cannot diverge.
fn client_request(
    server_url: &str,
    token: Option<&str>,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>, String> {
    let mut request = server_url
        .into_client_request()
        .map_err(|e| format!("invalid server URL '{server_url}': {e}"))?;
    if let Some(t) = token {
        let value = format!("Bearer {t}")
            .parse()
            .map_err(|e| format!("token is not a valid header value: {e}"))?;
        request.headers_mut().insert(
            tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
            value,
        );
    }
    Ok(request)
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
            host_sources: None,
            hook_path: Vec::new(),
            bundles: None,
            backoff: Backoff::default(),
            respawnable: false,
            runtimes: Arc::new(Mutex::new(HashMap::new())),
            lifecycle_locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Sandbox every runtime with the vendor's baseline capability spec.
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

    /// Whether a get for a runtime that is not live may rebuild it from the
    /// spec this agent persisted when it created it.
    ///
    /// Off by default, and that default is load-bearing. A vendor that
    /// provisions the workspace it hands out — cloning repos, installing
    /// bundles — would redo all of that on a respawn, handing the session a
    /// clean tree where its work used to be. For that vendor a missing runtime
    /// really is terminal, and the server turning it into an unrecoverable
    /// session is the correct, non-destructive answer.
    ///
    /// A vendor fixed to user-owned directories is the opposite case: it built
    /// nothing, so there is nothing to rebuild. Its runtime is a process over a
    /// directory that exists whether or not the process does, which makes
    /// starting another one recovery rather than provisioning.
    ///
    /// Deliberately not derived from `supports_provisioning`. "Can you build a
    /// workspace?" and "is your runtime disposable?" are different questions,
    /// and a vendor could answer them independently.
    #[must_use]
    pub fn with_respawnable_runtimes(mut self, enabled: bool) -> Self {
        self.respawnable = enabled;
        self
    }

    /// Let this agent's runtimes fetch server-managed bundles.
    #[must_use]
    pub fn with_bundles(mut self, delivery: BundleDelivery) -> Self {
        self.bundles = Some(delivery);
        self
    }

    /// Serve a host plugin library to every runtime this agent spawns.
    ///
    /// `sources` is the root the library's symlinks point into; both it and the
    /// library are added to the sandbox capability spec this agent writes.
    #[must_use]
    pub fn with_host_library(
        mut self,
        dir: PathBuf,
        sources: Option<PathBuf>,
        hook_path: Vec<PathBuf>,
    ) -> Self {
        self.host_library = Some(dir);
        self.host_sources = sources;
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
    /// `credential` supplies the bearer, and is asked again before *every*
    /// attempt rather than once at startup. An access token outlives neither a
    /// long link nor a long outage, and an established WebSocket is never
    /// re-authenticated — so a token captured at startup can be hours stale by
    /// the time a reconnect first presents it, and every retry after that
    /// presents the same corpse. `Ok(None)` is correct against a server running
    /// with authentication disabled.
    ///
    /// Dial, handshake, transport and [`CredentialError::Transient`] failures
    /// are retried indefinitely. There are exactly two `Err` returns: a
    /// `server_url` no attempt could ever dial (fail-fast, before the first
    /// attempt) and a [`CredentialError::Dead`] credential, which no retry
    /// could fix.
    pub async fn run(
        self,
        server_url: &str,
        credential: CredentialProvider,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        // Reject an undialable URL before the first attempt — a typo should be
        // an error the operator sees once rather than a retry loop that can
        // never succeed. The token is checked per attempt, below.
        client_request(server_url, None)?;

        let mut backoff = self.backoff;
        let mut failures: u32 = 0;
        let mut connections: u32 = 0;
        let agent = Arc::new(self);

        loop {
            let token = match credential().await {
                Ok(token) => token,
                // Nothing this loop can do will produce a working credential,
                // and a silent 401 every 30s is the failure mode this whole
                // change exists to end. Say so and stop.
                Err(CredentialError::Dead(why)) => {
                    return Err(format!("credential rejected: {why}"));
                }
                // Indistinguishable from the server being unreachable, and
                // treated the same: the issuer may be back before the next
                // attempt.
                Err(CredentialError::Transient(why)) => {
                    failures = failures.saturating_add(1);
                    let delay = backoff.next_delay();
                    note(&format!(
                        "vendor agent: attempt {failures} failed: {why}; reconnecting in {:.1}s",
                        delay.as_secs_f64()
                    ));
                    tokio::select! {
                        () = cancel.cancelled() => break,
                        () = tokio::time::sleep(delay) => {}
                    }
                    continue;
                }
            };
            let ended = match agent.connect(server_url, token.as_deref()).await {
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
    async fn connect(
        &self,
        server_url: &str,
        token: Option<&str>,
    ) -> Result<(Sink, Stream), String> {
        let (ws, _) = tokio_tungstenite::connect_async(client_request(server_url, token)?)
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

    /// The lock guarding lifecycle commands for one runtime id.
    async fn lifecycle_lock(&self, runtime_id: &str) -> Arc<Mutex<()>> {
        self.lifecycle_locks
            .lock()
            .await
            .entry(runtime_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn dispatch(&self, inbound: RuntimeVendorInboundMessage, sink: Sink) {
        let request_id = inbound.request_id;
        let outcome = match inbound.command {
            RuntimeVendorCommand::CreateRuntime(cmd) => {
                let lock = self.lifecycle_lock(&cmd.runtime_id).await;
                let _guard = lock.lock().await;
                let created = self.provision(&cmd.runtime_id, &cmd.spec).await;
                created.map(|()| {
                    RuntimeVendorEvent::CreateRuntime(CreateRuntimeResponse {
                        runtime_id: cmd.runtime_id,
                    })
                })
            }
            // Live → hand it back. Not live but we recorded how to rebuild it →
            // rebuild. Neither → gone, which the server turns into a terminally
            // unrecoverable session.
            //
            // Only a vendor that builds nothing gets the middle case: see
            // `with_respawnable_runtimes`. For everyone else this is exactly the
            // liveness check it has always been.
            RuntimeVendorCommand::GetRuntime(cmd) => {
                let lock = self.lifecycle_lock(&cmd.runtime_id).await;
                let _guard = lock.lock().await;
                let resolved = if self.transport_for(&cmd.runtime_id).await.is_some() {
                    Ok(())
                } else {
                    match self.persisted_spec(&cmd.runtime_id) {
                        Some(spec) => self.provision(&cmd.runtime_id, &spec).await,
                        None => Err(format!(
                            "no runtime '{}' on this vendor; it cannot be resumed",
                            cmd.runtime_id
                        )),
                    }
                };
                resolved.map(|()| {
                    RuntimeVendorEvent::GetRuntime(GetRuntimeResponse {
                        runtime_id: cmd.runtime_id,
                    })
                })
            }
            // Advisory. An agent that can rebuild the runtime takes the hint and
            // frees the process; one that cannot must decline, because for it
            // stopping the runtime and losing the session are the same act.
            RuntimeVendorCommand::HibernateRuntime(cmd) => {
                if self.respawnable {
                    let lock = self.lifecycle_lock(&cmd.runtime_id).await;
                    let _guard = lock.lock().await;
                    self.halt(&cmd.runtime_id).await;
                }
                Ok(RuntimeVendorEvent::HibernateRuntime(
                    HibernateRuntimeResponse {
                        runtime_id: cmd.runtime_id,
                    },
                ))
            }
            RuntimeVendorCommand::DeleteRuntime(cmd) => {
                let lock = self.lifecycle_lock(&cmd.runtime_id).await;
                let _guard = lock.lock().await;
                self.halt(&cmd.runtime_id).await;
                // The session is gone, so the record of how to rebuild its
                // runtime goes with it — otherwise a deleted session's spec
                // would outlive it on disk forever.
                let _ = std::fs::remove_dir_all(self.state_dir.join(&cmd.runtime_id));
                self.lifecycle_locks.lock().await.remove(&cmd.runtime_id);
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

        // Recorded before the spawn, not after: a runtime that dies during
        // provisioning is exactly the one a later get should be able to rebuild.
        if self.respawnable {
            self.write_spec_file(runtime_id, request)?;
        }

        // The vendor owns the sandbox policy: nothing about confinement
        // crosses the wire. The provider needs the spec as a file, so the
        // baseline (plus this machine's plugin-library grants) is written per
        // runtime — on revive as well, so a recovered runtime never runs
        // against a stale policy.
        let caps_file = if self.sandbox {
            Some(self.write_caps_file(runtime_id)?)
        } else {
            None
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
            // Only worth mirroring if this runtime can come back: for anyone
            // else the file would outlive nothing.
            state_file: self
                .respawnable
                .then(|| self.agents_path(runtime_id).to_string_lossy().into_owned()),
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

    /// Persist the effective capability spec for a runtime and return its path.
    ///
    /// The spec is this vendor's [`baseline`](crate::baseline) plus read
    /// grants for the host plugin library — without them a sandboxed runtime
    /// is handed a `--plugins-dir` it has no capability to read.
    fn write_caps_file(&self, runtime_id: &str) -> Result<PathBuf, String> {
        let mut spec = crate::baseline::baseline_capabilities()?;
        let sources: Vec<PathBuf> = self.host_sources.iter().cloned().collect();
        spec.grants
            .extend(horsie_support::plugin::grants::plugin_library_grants(
                self.host_library.as_deref(),
                &sources,
                &self.hook_path,
            ));
        // The runtime mirrors its per-agent cwd and env into this directory. The
        // baseline grants the working dir and a few system reads and nothing
        // else, so without this the first `set_env` inside a sandboxed runtime
        // dies on a sandbox denial rather than anything legible.
        if self.respawnable {
            spec.grants.push(Grant::Dir(DirGrant {
                path: self
                    .state_dir
                    .join(runtime_id)
                    .to_string_lossy()
                    .into_owned(),
                access: Access::ReadWrite,
            }));
        }
        let path = self.caps_path(runtime_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create runtime state dir: {e}"))?;
        }
        let bytes =
            serde_json::to_vec_pretty(&spec).map_err(|e| format!("encode capability spec: {e}"))?;
        std::fs::write(&path, bytes).map_err(|e| format!("write capability file: {e}"))?;
        Ok(path)
    }

    /// Where this agent writes a runtime's sandbox capability file. Public so
    /// the process provider can be pointed at the same path.
    #[must_use]
    pub fn caps_path(&self, runtime_id: &str) -> PathBuf {
        self.state_dir.join(runtime_id).join("capabilities.json")
    }

    /// Where this agent remembers what a runtime was made of.
    fn spec_path(&self, runtime_id: &str) -> PathBuf {
        self.state_dir.join(runtime_id).join("spec.json")
    }

    /// Where the runtime's own process mirrors its per-agent cwd and env. This
    /// agent only supplies the path; it never reads the file.
    fn agents_path(&self, runtime_id: &str) -> PathBuf {
        self.state_dir.join(runtime_id).join("agents.json")
    }

    /// Remember what this runtime was made of, so a later get can rebuild it
    /// without the server having to re-send anything.
    ///
    /// 0600 because the spec's `env` is where the server puts what it mints. A
    /// vendor fixed to user-owned directories is handed none of that today, but
    /// nothing in the protocol promises that, and this file sits on the same
    /// machine as the workspaces it would grant access to.
    fn write_spec_file(&self, runtime_id: &str, spec: &RuntimeSpec) -> Result<(), String> {
        let path = self.spec_path(runtime_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create runtime state dir: {e}"))?;
        }
        let bytes = serde_json::to_vec(spec).map_err(|e| format!("encode runtime spec: {e}"))?;
        std::fs::write(&path, bytes).map_err(|e| format!("write runtime spec: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// The spec a previous incarnation of this runtime was built from, if this
    /// agent is allowed to rebuild it at all.
    ///
    /// An unreadable or malformed file reads as absent: the runtime is then
    /// reported gone, which is the same answer this agent gave before it
    /// persisted anything, and is safe.
    fn persisted_spec(&self, runtime_id: &str) -> Option<RuntimeSpec> {
        if !self.respawnable {
            return None;
        }
        let bytes = std::fs::read(self.spec_path(runtime_id)).ok()?;
        serde_json::from_slice(&bytes).ok()
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

    /// A `wss://` server must be dialable at all: without a TLS feature on
    /// `tokio-tungstenite`, every dial to an HTTPS-fronted session server dies
    /// with "TLS support not compiled in" before a single byte of TLS, and the
    /// reconnect loop retries that forever.
    ///
    /// The listener accepts and immediately drops the connection, so the dial
    /// gets far enough to prove TLS is compiled in (it fails on the handshake,
    /// not on the URL) without needing a certificate.
    #[tokio::test]
    async fn a_wss_url_gets_as_far_as_the_tls_handshake() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = listener.accept().await;
        });

        let err = tokio_tungstenite::connect_async(format!("wss://127.0.0.1:{port}/"))
            .await
            .expect_err("a listener that hangs up cannot complete a handshake")
            .to_string();

        assert!(
            !err.contains("TLS support not compiled in"),
            "wss dial rejected before any I/O: {err}"
        );
    }

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

    /// The baseline cannot know this machine's plugin library, so the agent
    /// must inject the grants for it. Without this a sandboxed runtime is
    /// handed a `--plugins-dir` it has no capability to read — and since
    /// installed plugins are symlinks into `sources/`, the target root has to
    /// be granted too or every read still fails.
    #[test]
    fn the_written_caps_file_grants_the_host_plugin_library_and_its_sources() {
        let state = tempfile::tempdir().expect("tempdir");
        let agent = RuntimeVendor::new(
            "test-vendor".to_string(),
            false,
            Arc::new(|_id: &str, _caps: Option<PathBuf>| Arc::new(NeverProvider)),
            Arc::new(ConnectedRuntimeRegistry::new()),
            Arc::new(FixedWorkspaces::new(HashMap::new())),
            state.path().to_path_buf(),
        )
        .with_host_library(
            PathBuf::from("/data/plugins"),
            Some(PathBuf::from("/data/sources")),
            vec![PathBuf::from("/opt/node/bin")],
        );

        let path = agent.write_caps_file("rt-1").expect("write caps");
        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read caps")).expect("parse caps");
        // The grant union is fluorite-tagged: `{"type":"Dir","value":{"path":…}}`.
        let granted: Vec<&str> = written["grants"]
            .as_array()
            .expect("grants array")
            .iter()
            .filter_map(|g| {
                g.get("value")
                    .and_then(|v| v.get("path"))
                    .and_then(serde_json::Value::as_str)
            })
            .collect();

        assert!(granted.contains(&"/data/plugins"), "granted: {granted:?}");
        assert!(granted.contains(&"/data/sources"), "granted: {granted:?}");
        assert!(granted.contains(&"/opt/node/bin"), "granted: {granted:?}");
        // The baseline's own grants survive alongside them.
        assert!(granted.contains(&"/usr"), "granted: {granted:?}");
    }

    /// With no host library there is nothing to merge, and the written file is
    /// exactly the baseline.
    #[test]
    fn the_written_caps_file_is_the_baseline_without_a_host_library() {
        let state = tempfile::tempdir().expect("tempdir");
        let agent = RuntimeVendor::new(
            "test-vendor".to_string(),
            false,
            Arc::new(|_id: &str, _caps: Option<PathBuf>| Arc::new(NeverProvider)),
            Arc::new(ConnectedRuntimeRegistry::new()),
            Arc::new(FixedWorkspaces::new(HashMap::new())),
            state.path().to_path_buf(),
        );
        let path = agent.write_caps_file("rt-1").expect("write caps");
        let written: horsie_models::capabilities::CapabilitySpec =
            serde_json::from_slice(&std::fs::read(&path).expect("read caps")).expect("parse caps");
        assert_eq!(
            written,
            crate::baseline::baseline_capabilities().expect("baseline")
        );
    }

    #[tokio::test]
    async fn an_undialable_url_fails_before_any_attempt_instead_of_retrying_forever() {
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            agent().run(
                "ws://not a host/api/vendor/connect",
                no_credential(),
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
                    .run(
                        "ws://127.0.0.1:1/api/vendor/connect",
                        no_credential(),
                        cancel,
                    )
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

    #[test]
    fn the_dial_request_carries_the_bearer_when_there_is_one() {
        use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;

        let bare = client_request("ws://localhost:3789/api/vendor/connect", None).unwrap();
        assert!(bare.headers().get(AUTHORIZATION).is_none());

        let with =
            client_request("ws://localhost:3789/api/vendor/connect", Some("hsk_agt_x")).unwrap();
        assert_eq!(
            with.headers().get(AUTHORIZATION).unwrap(),
            "Bearer hsk_agt_x"
        );
    }

    #[test]
    fn an_undialable_url_or_an_unusable_token_fails_before_any_attempt() {
        assert!(client_request("ws://not a host/api/vendor/connect", None).is_err());
        // A newline in a token would smuggle a header; reject it at the door.
        assert!(
            client_request("ws://localhost:3789/api/vendor/connect", Some("bad\nvalue")).is_err()
        );
    }
}
