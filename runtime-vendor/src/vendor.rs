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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

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

/// How often this agent pings the server it is connected to.
///
/// The server drops a link that goes quiet for three of these. Nothing waits
/// for the pong: the ping is this agent reporting that it is alive, not asking
/// whether the server is — a dead server is already visible as a socket error.
const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

/// How long the server has to accept or refuse a registration before the
/// attempt is written off and retried.
const REGISTRATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Why a dial did not produce a serving link.
enum ConnectError {
    /// Worth retrying: an unreachable server, a refused socket, a handshake
    /// that never completed.
    Transient(String),
    /// The server refused to publish this agent. Retrying re-runs the identical
    /// handshake and earns the identical answer.
    Refused(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transient(e) | Self::Refused(e) => write!(f, "{e}"),
        }
    }
}

/// Why an agent stopped for good.
///
/// A sum type rather than a string because the two arms want different
/// handling: `horsie connect` can tell an operator how to resolve a name
/// collision, and only a caller that knows about flags can say that.
#[derive(Debug, PartialEq, Eq)]
pub enum AgentExit {
    /// The server refused to publish this agent under the name it announced.
    NameRefused(String),
    /// Anything else no retry could fix: an undialable server URL, a credential
    /// the issuer refuses.
    Fatal(String),
}

impl std::fmt::Display for AgentExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NameRefused(e) | Self::Fatal(e) => write!(f, "{e}"),
        }
    }
}

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
    /// Root under which each runtime gets its own directory to unpack into.
    pub dir: String,
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
    /// This process's identity, minted once and presented on every dial.
    ///
    /// The server uses it for exactly one decision: whether a dial claiming a
    /// name already in use is this agent coming back, or a second agent trying
    /// to take the name. It is never how anything addresses this vendor — that
    /// is always `vendor_name`.
    instance_id: String,
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
    /// Directories prepended to PATH when a runtime runs a plugin hook (the
    /// node bin dir, typically). A property of this machine, resolved
    /// agent-side; never sent by the server.
    hook_path: Vec<PathBuf>,
    /// How this agent's runtimes fetch server-managed bundles: the base URL
    /// that reaches the server from where they run, and the root they unpack
    /// into. Both are the agent's knowledge, not the server's — it sends only
    /// hashes and a token.
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
            instance_id: Uuid::new_v4().to_string(),
            supports_provisioning,
            provider,
            connected,
            workspaces,
            state_dir,
            sandbox: false,
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

    /// Directories prepended to PATH when a runtime runs plugin hooks, and
    /// granted read access in the sandbox.
    #[must_use]
    pub fn with_hook_path(mut self, hook_path: Vec<PathBuf>) -> Self {
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
    ) -> Result<(), AgentExit> {
        // Reject an undialable URL before the first attempt — a typo should be
        // an error the operator sees once rather than a retry loop that can
        // never succeed. The token is checked per attempt, below.
        client_request(server_url, None).map_err(AgentExit::Fatal)?;

        // Nothing this agent owns is running yet, so any bundle directory
        // without a spec belongs to a runtime that cannot come back.
        self.sweep_plugin_dirs();

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
                    return Err(AgentExit::Fatal(format!("credential rejected: {why}")));
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
                // The name belongs to another live agent. Every retry re-runs
                // the identical handshake and earns the identical answer, so
                // stop and say why — silently backing off forever is what made
                // two agents sharing a name invisible in the first place.
                Err(ConnectError::Refused(reason)) => {
                    agent.halt_all().await;
                    return Err(AgentExit::NameRefused(reason));
                }
                Err(ConnectError::Transient(e)) => {
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

    /// Dial the server, announce this vendor, and wait to be told it is
    /// published. All three are one step: a socket that never got its `Ready`
    /// across, and one whose `Ready` was refused, are both unusable, and only
    /// the reason for stopping differs.
    async fn connect(
        &self,
        server_url: &str,
        token: Option<&str>,
    ) -> Result<(Sink, Stream), ConnectError> {
        let request = client_request(server_url, token).map_err(ConnectError::Transient)?;
        let (ws, _) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| ConnectError::Transient(format!("connect {server_url}: {e}")))?;
        let (sink_inner, mut stream) = ws.split();
        let sink: Sink = Arc::new(Mutex::new(sink_inner));
        send(
            &sink,
            RuntimeVendorOutboundMessage {
                request_id: "boot".to_string(),
                event: RuntimeVendorEvent::Ready(RuntimeVendorReady {
                    vendor_name: self.vendor_name.clone(),
                    instance_id: self.instance_id.clone(),
                    capabilities: RuntimeVendorCapabilities {
                        supports_provisioning: self.supports_provisioning,
                    },
                }),
            },
        )
        .await
        .map_err(ConnectError::Transient)?;
        Self::await_verdict(&mut stream).await?;
        Ok((sink, stream))
    }

    /// Read until the server accepts or refuses the registration.
    ///
    /// Nothing else can arrive first: commands are addressed to a *published*
    /// vendor, and this one is not published yet. An unexpected frame is
    /// therefore skipped rather than treated as fatal — the verdict is what this
    /// wait is for.
    async fn await_verdict(stream: &mut Stream) -> Result<(), ConnectError> {
        let verdict = tokio::time::timeout(REGISTRATION_TIMEOUT, async {
            loop {
                let Some(next) = stream.next().await else {
                    return Err(ConnectError::Transient(
                        "server closed the link before answering the handshake".to_string(),
                    ));
                };
                let text = match next {
                    Ok(Message::Text(text)) => text,
                    Ok(Message::Binary(_))
                    | Ok(Message::Ping(_))
                    | Ok(Message::Pong(_))
                    | Ok(Message::Frame(_)) => continue,
                    Ok(Message::Close(_)) => {
                        return Err(ConnectError::Transient(
                            "server closed the link before answering the handshake".to_string(),
                        ));
                    }
                    Err(e) => return Err(ConnectError::Transient(format!("link failed: {e}"))),
                };
                let Ok(inbound) = serde_json::from_str::<RuntimeVendorInboundMessage>(&text) else {
                    note("vendor agent: undecodable frame while awaiting registration, ignoring");
                    continue;
                };
                match inbound.command {
                    RuntimeVendorCommand::VendorRegistered(_) => return Ok(()),
                    RuntimeVendorCommand::VendorRejected(rejected) => {
                        return Err(ConnectError::Refused(rejected.reason));
                    }
                    RuntimeVendorCommand::CreateRuntime(_)
                    | RuntimeVendorCommand::GetRuntime(_)
                    | RuntimeVendorCommand::HibernateRuntime(_)
                    | RuntimeVendorCommand::DeleteRuntime(_)
                    | RuntimeVendorCommand::QueryRuntimes(_)
                    | RuntimeVendorCommand::Runtime(_) => {
                        note("vendor agent: command before registration was answered, ignoring");
                    }
                }
            }
        })
        .await;
        match verdict {
            Ok(result) => result,
            Err(_) => Err(ConnectError::Transient(
                "timed out waiting for the server to answer the handshake".to_string(),
            )),
        }
    }

    /// Serve commands over one live link until it dies or `cancel` fires.
    async fn serve(
        self: &Arc<Self>,
        sink: Sink,
        mut stream: Stream,
        cancel: &CancellationToken,
    ) -> LinkEnd {
        // Ticks immediately, so the server sees this agent alive as soon as it
        // starts serving rather than one interval later.
        let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return LinkEnd::Cancelled,
                _ = heartbeat.tick() => {
                    // A failed write means the socket is gone; let the read
                    // half report it rather than racing it to a verdict.
                    if let Err(e) = sink.lock().await.send(Message::Ping(Vec::new().into())).await {
                        note(&format!("vendor agent: heartbeat failed: {e}"));
                    }
                }
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
                // runtime goes with it, and so do its bundles — otherwise a
                // deleted session's state would outlive it on disk forever.
                self.forget_runtime_dirs(&cmd.runtime_id);
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
            // Both are answers to the handshake, consumed before this loop ever
            // runs. Arriving here means the server sent one twice; there is
            // nothing to do with it and nothing to reply.
            RuntimeVendorCommand::VendorRegistered(_) | RuntimeVendorCommand::VendorRejected(_) => {
                note("vendor agent: a registration verdict arrived on an established link");
                return;
            }
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

        self.prepare_plugins_dir(runtime_id);

        let mut env = request.env.clone();
        env.extend(self.bundle_env(runtime_id));

        let config = RuntimeConfig {
            workspaces,
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
    /// The spec is this vendor's [`baseline`](crate::baseline) plus the
    /// directory this runtime materializes its session's bundles into and the
    /// hook interpreter dirs. The bundles dir is read-write: the sandbox is
    /// applied before the runtime fetches, so it unpacks under confinement.
    fn write_caps_file(&self, runtime_id: &str) -> Result<PathBuf, String> {
        let mut spec = crate::baseline::baseline_capabilities()?;
        spec.grants
            .extend(horsie_support::plugin::grants::session_plugin_grants(
                self.plugins_path(runtime_id).as_deref(),
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

    /// Root under which each runtime materializes its session's bundles.
    /// `None` when this agent serves none.
    fn plugins_root(&self) -> Option<&Path> {
        self.bundles.as_ref().map(|b| Path::new(b.dir.as_str()))
    }

    /// Where `runtime_id` materializes its session's bundles. One directory per
    /// runtime: the runtime scans the whole directory it is given, so a shared
    /// one would show a session every other session's skills.
    fn plugins_path(&self, runtime_id: &str) -> Option<PathBuf> {
        self.plugins_root().map(|root| root.join(runtime_id))
    }

    /// Remove bundle directories belonging to runtimes this agent can no longer
    /// revive.
    ///
    /// Called once at startup, where no runtime process is live by definition,
    /// so anything without a persisted spec is crash debris. A vendor that
    /// persists no specs at all cannot revive anything, and correctly loses
    /// every directory here.
    ///
    /// Best-effort throughout: an unreadable root or an undeletable directory
    /// costs disk, and is not worth refusing to start over.
    fn sweep_plugin_dirs(&self) {
        let Some(root) = self.plugins_root() else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(runtime_id) = name.to_str() else {
                continue;
            };
            if self.spec_path(runtime_id).is_file() {
                continue;
            }
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }

    /// Drop everything on disk belonging to a runtime whose session is gone:
    /// the record of how to rebuild it, and the bundles it materialized.
    ///
    /// Deliberately not called from `halt`. Stopping a process is not losing a
    /// session, and a hibernated runtime must find its bundles still there when
    /// it wakes — that is what makes materialization a once-per-runtime cost.
    fn forget_runtime_dirs(&self, runtime_id: &str) {
        let _ = std::fs::remove_dir_all(self.state_dir.join(runtime_id));
        if let Some(dir) = self.plugins_path(runtime_id) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// Create the directory this runtime will materialize its bundles into.
    ///
    /// The runtime cannot do this for itself under a sandbox: the grant names
    /// that directory by path, and creating it is a write on the *parent*,
    /// which is deliberately not granted. So the agent — which owns the machine
    /// and is not confined — makes it, and the runtime only ever writes inside.
    ///
    /// Best-effort: a failure here surfaces as the runtime reporting it could
    /// not provision its bundles, which is the same class of degradation as a
    /// bundle that would not download.
    fn prepare_plugins_dir(&self, runtime_id: &str) {
        if let Some(dir) = self.plugins_path(runtime_id) {
            if let Err(e) = std::fs::create_dir_all(&dir) {
                note(&format!(
                    "vendor agent: cannot create bundle dir {}: {e}",
                    dir.display()
                ));
            }
        }
    }

    /// The bundle-delivery environment for one runtime. Extracted from
    /// `provision` so the per-runtime path is testable without spawning
    /// anything.
    fn bundle_env(&self, runtime_id: &str) -> Vec<EnvVar> {
        let Some(b) = &self.bundles else {
            return Vec::new();
        };
        let mut env = vec![EnvVar {
            name: horsie_models::ENV_PLUGINS_BASE.to_string(),
            value: b.base_url.clone(),
        }];
        if let Some(dir) = self.plugins_path(runtime_id) {
            env.push(EnvVar {
                name: horsie_models::ENV_PLUGINS_DIR.to_string(),
                value: dir.to_string_lossy().into_owned(),
            });
        }
        env
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
    /// 0600 because the spec's `env` is where the server puts what it mints,
    /// including the token a runtime fetches its bundles with — every vendor is
    /// handed that, and this file sits on the same machine as the workspaces it
    /// would grant access to.
    ///
    /// That token outlives its own validity here: a revive replays whatever was
    /// written, expiry included. Nothing on the happy path re-fetches, so it is
    /// unreachable rather than fixed — see blossomstack/horsie#242.
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
pub(crate) fn note(message: &str) {
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

    /// A shared bundles directory would let one session scan another's skills,
    /// because the runtime scans the whole directory it is pointed at.
    #[test]
    fn the_bundle_env_names_a_directory_per_runtime() {
        let agent = agent().with_bundles(BundleDelivery {
            base_url: "http://127.0.0.1:3789".to_string(),
            dir: "/state/plugins".to_string(),
        });
        let value = |vars: &[EnvVar], name: &str| {
            vars.iter()
                .find(|v| v.name == name)
                .map(|v| v.value.clone())
        };

        let one = agent.bundle_env("rt-1");
        let two = agent.bundle_env("rt-2");

        assert_eq!(
            value(&one, horsie_models::ENV_PLUGINS_DIR).as_deref(),
            Some("/state/plugins/rt-1")
        );
        assert_eq!(
            value(&two, horsie_models::ENV_PLUGINS_DIR).as_deref(),
            Some("/state/plugins/rt-2")
        );
        assert_eq!(
            value(&one, horsie_models::ENV_PLUGINS_BASE).as_deref(),
            Some("http://127.0.0.1:3789")
        );
    }

    #[test]
    fn an_agent_serving_no_bundles_adds_no_bundle_env() {
        assert!(agent().bundle_env("rt-1").is_empty());
    }

    /// A sandboxed runtime is granted its bundle dir *by path*, so creating it
    /// would be a write on the ungranted parent. The agent is not confined and
    /// makes it first; without this every sandboxed provision fails silently.
    #[test]
    fn the_agent_creates_the_bundle_dir_the_runtime_cannot() {
        let state = tempfile::tempdir().expect("tempdir");
        let agent = agent_with_bundles(state.path());
        assert!(!state.path().join("plugins/rt-1").exists());

        agent.prepare_plugins_dir("rt-1");

        assert!(state.path().join("plugins/rt-1").is_dir());
    }

    /// Build an agent rooted at `state`, serving bundles out of `state/plugins`.
    fn agent_with_bundles(state: &Path) -> RuntimeVendor {
        RuntimeVendor::new(
            "test-vendor".to_string(),
            false,
            Arc::new(|_id: &str, _caps: Option<PathBuf>| Arc::new(NeverProvider)),
            Arc::new(ConnectedRuntimeRegistry::new()),
            Arc::new(FixedWorkspaces::new(HashMap::new())),
            state.to_path_buf(),
        )
        .with_bundles(BundleDelivery {
            base_url: "http://127.0.0.1:3789".to_string(),
            dir: state.join("plugins").to_string_lossy().into_owned(),
        })
    }

    /// Boot is the one moment when "no runtime process is live" is guaranteed,
    /// so the only question left is whether the runtime still exists at all —
    /// and the spec file is the same record that decides whether it could be
    /// revived.
    #[test]
    fn boot_sweeps_bundle_dirs_with_no_surviving_spec() {
        let state = tempfile::tempdir().expect("tempdir");
        let plugins = state.path().join("plugins");
        std::fs::create_dir_all(plugins.join("kept/demo")).expect("mkdir");
        std::fs::create_dir_all(plugins.join("orphan/demo")).expect("mkdir");
        // `kept` is revivable: it has a persisted spec.
        std::fs::create_dir_all(state.path().join("kept")).expect("mkdir");
        std::fs::write(state.path().join("kept/spec.json"), b"{}").expect("write");

        agent_with_bundles(state.path()).sweep_plugin_dirs();

        assert!(
            plugins.join("kept/demo").is_dir(),
            "a revivable runtime keeps its bundles"
        );
        assert!(!plugins.join("orphan").exists(), "crash debris is removed");
    }

    /// Deleting a session takes its bundles; stopping a process must not.
    /// A hibernated runtime has to find them still there when it wakes, which
    /// is the whole reason materialization can happen once.
    #[test]
    fn forgetting_a_runtime_removes_both_its_dirs() {
        let state = tempfile::tempdir().expect("tempdir");
        let plugins = state.path().join("plugins");
        std::fs::create_dir_all(plugins.join("rt-1/demo")).expect("mkdir");
        std::fs::create_dir_all(state.path().join("rt-1")).expect("mkdir");
        std::fs::write(state.path().join("rt-1/spec.json"), b"{}").expect("write");

        agent_with_bundles(state.path()).forget_runtime_dirs("rt-1");

        assert!(
            !state.path().join("rt-1").exists(),
            "the rebuild record is gone"
        );
        assert!(!plugins.join("rt-1").exists(), "and so are its bundles");
    }

    /// An agent that serves no bundles has no root to sweep, and must not
    /// wander into the state dir looking for one.
    #[test]
    fn sweeping_without_bundles_is_a_noop() {
        let state = tempfile::tempdir().expect("tempdir");
        std::fs::write(state.path().join("keep-me"), b"x").expect("write");
        let agent = RuntimeVendor::new(
            "test-vendor".to_string(),
            false,
            Arc::new(|_id: &str, _caps: Option<PathBuf>| Arc::new(NeverProvider)),
            Arc::new(ConnectedRuntimeRegistry::new()),
            Arc::new(FixedWorkspaces::new(HashMap::new())),
            state.path().to_path_buf(),
        );
        agent.sweep_plugin_dirs();
        assert!(state.path().join("keep-me").is_file());
    }

    /// The sandbox is applied before the runtime fetches its bundles, so the
    /// directory it unpacks into has to be writable — a read grant leaves the
    /// unpack failing, and provisioning is best-effort, so it fails silently to
    /// "no skills".
    #[test]
    fn the_written_caps_file_grants_the_runtimes_own_plugins_dir_and_hook_path() {
        let state = tempfile::tempdir().expect("tempdir");
        let agent = RuntimeVendor::new(
            "test-vendor".to_string(),
            false,
            Arc::new(|_id: &str, _caps: Option<PathBuf>| Arc::new(NeverProvider)),
            Arc::new(ConnectedRuntimeRegistry::new()),
            Arc::new(FixedWorkspaces::new(HashMap::new())),
            state.path().to_path_buf(),
        )
        .with_bundles(BundleDelivery {
            base_url: "http://127.0.0.1:3789".to_string(),
            dir: "/state/plugins".to_string(),
        })
        .with_hook_path(vec![PathBuf::from("/opt/node/bin")]);

        let path = agent.write_caps_file("rt-1").expect("write caps");
        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read caps")).expect("parse caps");
        // The grant union is fluorite-tagged: `{"type":"Dir","value":{"path":…}}`.
        let grant = |path: &str| {
            written["grants"]
                .as_array()
                .expect("grants array")
                .iter()
                .find(|g| {
                    g.get("value")
                        .and_then(|v| v.get("path"))
                        .and_then(serde_json::Value::as_str)
                        == Some(path)
                })
                .cloned()
        };

        let plugins = grant("/state/plugins/rt-1").expect("the runtime's own plugins dir");
        assert_eq!(plugins["value"]["access"], "ReadWrite");
        let hooks = grant("/opt/node/bin").expect("the hook interpreter dir");
        assert_eq!(hooks["value"]["access"], "Read");
        // Another runtime's directory is not granted.
        assert!(grant("/state/plugins/rt-2").is_none());
        // The baseline's own grants survive alongside them.
        assert!(grant("/usr").is_some());
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
        assert!(
            matches!(&err, AgentExit::Fatal(e) if e.contains("invalid server URL")),
            "{err:?}"
        );
    }

    /// Accept `attempts` dials, answer each `Ready` with `verdict`, and report
    /// the instance id every dial announced.
    ///
    /// The socket is dropped after each verdict, which is what a server does to
    /// a refused agent and what a network fault looks like to an accepted one —
    /// so one stub covers both tests.
    async fn stub_server(
        attempts: usize,
        verdict: fn() -> RuntimeVendorCommand,
    ) -> (u16, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let announced = Arc::new(Mutex::new(Vec::new()));
        let recorder = announced.clone();
        tokio::spawn(async move {
            for _ in 0..attempts {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                let Some(Ok(Message::Text(text))) = ws.next().await else {
                    return;
                };
                let msg: RuntimeVendorOutboundMessage = serde_json::from_str(&text).unwrap();
                if let RuntimeVendorEvent::Ready(ready) = msg.event {
                    recorder.lock().await.push(ready.instance_id);
                }
                let out = RuntimeVendorInboundMessage {
                    request_id: msg.request_id,
                    command: verdict(),
                };
                let _ = ws
                    .send(Message::Text(serde_json::to_string(&out).unwrap().into()))
                    .await;
            }
        });
        (port, announced)
    }

    fn rejected() -> RuntimeVendorCommand {
        RuntimeVendorCommand::VendorRejected(horsie_models::runtime_vendor::VendorRejected {
            reason: "vendor name \"test-vendor\" is already in use by another agent".to_string(),
        })
    }

    fn registered() -> RuntimeVendorCommand {
        RuntimeVendorCommand::VendorRegistered(horsie_models::runtime_vendor::VendorRegistered {})
    }

    /// The failure this whole change exists to end: a second agent on a name in
    /// use used to back off and re-dial forever, saying nothing, displacing the
    /// incumbent every time it won the race.
    #[tokio::test]
    async fn a_refused_registration_stops_the_agent_instead_of_retrying() {
        let (port, _) = stub_server(1, rejected).await;
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            agent().run(
                &format!("ws://127.0.0.1:{port}/api/vendor/connect"),
                no_credential(),
                CancellationToken::new(),
            ),
        )
        .await
        .expect("a refusal must not be retried — the backoff here is 60s")
        .expect_err("a refused agent exits non-zero rather than serving nothing");
        assert!(
            matches!(&err, AgentExit::NameRefused(reason) if reason.contains("already in use")),
            "a refusal must be distinguishable from any other fatal exit: {err:?}"
        );
    }

    /// The server tells this agent's redial apart from a second agent's dial by
    /// the instance id, so it has to be the same one every time.
    #[tokio::test]
    async fn every_dial_from_one_process_announces_the_same_instance_id() {
        let (port, announced) = stub_server(2, registered).await;
        let cancel = CancellationToken::new();
        let run = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                agent()
                    // Short enough that the second dial lands inside the test.
                    .with_backoff(Backoff::new(
                        std::time::Duration::from_millis(10),
                        std::time::Duration::from_millis(10),
                    ))
                    .run(
                        &format!("ws://127.0.0.1:{port}/api/vendor/connect"),
                        no_credential(),
                        cancel,
                    )
                    .await
            }
        });

        for _ in 0..200 {
            if announced.lock().await.len() >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        cancel.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), run).await;

        let ids = announced.lock().await.clone();
        assert_eq!(ids.len(), 2, "expected two dials, got {ids:?}");
        assert_eq!(ids[0], ids[1], "a reconnect must reclaim its own name");
        assert!(!ids[0].is_empty(), "an agent must announce an instance id");
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
