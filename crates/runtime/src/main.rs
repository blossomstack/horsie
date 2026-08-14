#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::wildcard_enum_match_arm
    )
)]

use clap::{CommandFactory, Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use horsie_models::runtime::{
    CancelledResponse, McpDiscoverResponse, McpInvokeResponse, PongResponse,
    ProvisionAgentResponse, ProvisionError, ProvisionOk, ProvisionResult,
    ProvisionWorkspaceResponse, RequestRefused, RunHooksResponse, RuntimeInboundMessage,
    RuntimeOutboundMessage, RuntimeReady, ScanResponse, ToolCallResponse, ToolError, ToolOutput,
    ToolResult,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex;
use tokio_tungstenite::{WebSocketStream, client_async, connect_async, tungstenite::Message};

/// Default retry policy for the initial server connection. A long-lived
/// `horsie connect` daemon should tolerate transient server restarts, so we
/// retry for ~13 minutes total before giving up.
const CONNECT_RETRIES: usize = 30;
const CONNECT_BASE_DELAY: Duration = Duration::from_secs(1);
const CONNECT_MAX_DELAY: Duration = Duration::from_secs(30);

// Run mode requires `--endpoint`, `--runtime-id`, and at least one `--workspace`;
// the `probe` subcommand must not inherit them. clap 4 can't express "required
// unless a subcommand is present" (`subcommand_negates_reqs` doesn't cover an
// optional subcommand), so these stay optional here and main() enforces them in
// run mode with the same exit-2 usage-error contract.
#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
    /// `ws://host:port` (TCP/WebSocket) or `unix:/path/to.sock` (unix socket).
    /// Required in run mode.
    #[arg(long)]
    endpoint: Option<String>,
    /// Required in run mode.
    #[arg(long)]
    runtime_id: Option<String>,
    /// Repeatable `name=path` workspace root. At least one is required in run mode.
    #[arg(long = "workspace", value_parser = parse_workspace_arg)]
    workspaces: Vec<horsie_models::Workspace>,
    /// Capability file confining tool execution with the nono sandbox before
    /// connecting (fail-closed). Its presence enables the sandbox; absent → no
    /// sandbox. The file fully defines the allowed capabilities.
    #[arg(long = "sandbox-caps")]
    sandbox_caps: Option<PathBuf>,
    /// Directory prepended to PATH when running plugin hooks (repeatable), e.g. the
    /// node bin dir.
    #[arg(long = "hook-path")]
    hook_path: Vec<PathBuf>,
    /// Where to mirror the per-agent cwd/env map, so a runtime that is stopped
    /// and started again resumes with it intact. Passed by vendors that can
    /// respawn a runtime; absent keeps that state in memory only.
    #[arg(long = "state-file")]
    state_file: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Commands {
    /// Answer git's credential protocol on stdin. Configured by this binary at
    /// startup as `credential.https://github.com.helper`, and invoked by git —
    /// never by a person.
    ///
    /// Mints a GitHub token for the repository git names, scoped to that one
    /// repository and lasting only as long as the operation. Prints nothing and
    /// exits 0 when it cannot, which is how a helper says "no credentials" and
    /// what keeps a public-repo clone working on a deployment with no GitHub
    /// connection.
    GitCredential {
        /// `get`, `store` or `erase`. Only `get` does anything.
        operation: String,
    },
    /// Apply the sandbox from --sandbox-caps, then exit — no endpoint, no connect,
    /// no retry. Exit 0 = sandbox applied; 3 = unsupported on this platform/build.
    /// Lets callers probe confinement support in milliseconds instead of burning
    /// the ~13-minute connect-retry budget against an unroutable endpoint.
    Probe {
        /// Capability file fully defining the sandbox to apply.
        #[arg(long = "sandbox-caps", required = true)]
        sandbox_caps: PathBuf,
        /// Repeatable `name=path` workspace root the sandbox must open up.
        #[arg(long = "workspace", required = true, value_parser = parse_workspace_arg)]
        workspaces: Vec<horsie_models::Workspace>,
    },
}

fn parse_workspace_arg(s: &str) -> Result<horsie_models::Workspace, String> {
    horsie_runtime::workspace::WorkspaceRegistry::parse_arg(s)
}

enum Endpoint {
    Ws(String),
    Unix(PathBuf),
}

/// The dial-back request, carrying the bearer when one was supplied.
///
/// Built in one place so the fail-fast validation and the retry loop cannot
/// diverge on what is actually sent — the retry closure clones this rather than
/// rebuilding it.
fn dial_request(
    url: &str,
    token: Option<&str>,
) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, String> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = url
        .into_client_request()
        .map_err(|e| format!("not a dialable endpoint: {e}"))?;
    if let Some(token) = token {
        let value = format!("Bearer {token}")
            .parse()
            .map_err(|_| "the connect token is not a valid header value".to_string())?;
        request.headers_mut().insert("authorization", value);
    }
    Ok(request)
}

fn parse_endpoint(s: &str) -> Result<Endpoint, String> {
    if let Some(rest) = s.strip_prefix("unix:") {
        Ok(Endpoint::Unix(PathBuf::from(rest)))
    } else if s.starts_with("ws://") || s.starts_with("wss://") {
        Ok(Endpoint::Ws(s.to_string()))
    } else {
        Err(format!("unsupported endpoint scheme: {s}"))
    }
}

fn main() {
    let cli = Cli::parse();

    // Credential mode: answer git and exit. Runs before endpoint parsing and
    // before the sandbox, because this process *is* a child of a git command
    // already running inside one — it inherits that confinement rather than
    // applying its own.
    if let Some(Commands::GitCredential { operation }) = &cli.command {
        let operation = operation.clone();
        match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt.block_on(horsie_runtime::git_credential::run(&operation)),
            Err(e) => eprintln!("failed to build tokio runtime: {e}"),
        }
        std::process::exit(0);
    }

    // Probe mode: apply the sandbox and exit — before endpoint parsing, the tokio
    // runtime, provisioning, and any connect attempt.
    if let Some(Commands::Probe {
        sandbox_caps,
        workspaces,
    }) = &cli.command
    {
        let dirs: Vec<PathBuf> = workspaces.iter().map(|w| w.path.clone()).collect();
        apply_sandbox_or_exit(&dirs, None, sandbox_caps);
        std::process::exit(0);
    }

    // Run mode from here on — enforce the required args clap can't (see above),
    // keeping the exit-2 usage-error contract for a stale/mistaken invocation.
    let (Some(endpoint), Some(runtime_id)) = (cli.endpoint.clone(), cli.runtime_id.clone()) else {
        Cli::command()
            .error(
                clap::error::ErrorKind::MissingRequiredArgument,
                "run mode requires --endpoint, --runtime-id and at least one --workspace",
            )
            .exit()
    };
    if cli.workspaces.is_empty() {
        Cli::command()
            .error(
                clap::error::ErrorKind::MissingRequiredArgument,
                "run mode requires --endpoint, --runtime-id and at least one --workspace",
            )
            .exit();
    }

    let endpoint = match parse_endpoint(&endpoint) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };

    if let Some(caps_file) = &cli.sandbox_caps {
        let socket = match &endpoint {
            Endpoint::Unix(p) => Some(p.as_path()),
            Endpoint::Ws(_) => None,
        };
        let dirs: Vec<PathBuf> = cli.workspaces.iter().map(|w| w.path.clone()).collect();
        apply_sandbox_or_exit(&dirs, socket, caps_file);
    }

    configure_git_credentials();

    // Build the multi-threaded runtime only after confinement is in place, so
    // every worker thread it spawns inherits the Landlock domain.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("failed to build tokio runtime: {e}");
            std::process::exit(1);
        }
    };
    runtime.block_on(run(cli, runtime_id, endpoint));
}

/// Point git at this binary's credential helper, for every process this runtime
/// ever spawns.
///
/// Set on *this* process's environment rather than on each git command, so it
/// reaches the provision-step clone and the agent's own `bash` tool calls
/// alike — including a `git push`, which had no credential at all when the
/// clone passed a one-shot `http.extraHeader` and left nothing behind.
///
/// `useHttpPath` is not optional. Without it git omits `path=` from what it
/// sends the helper, and the server would have no repository to scope a token
/// to; the helper declines rather than asking for something broader.
///
/// **On `set_var` being unsafe.** It races any concurrent `getenv`, so it is
/// sound only while this process is single-threaded — which is exactly here:
/// after `Cli::parse`, before the tokio runtime is built, and before anything
/// has been spawned. Doing it later, or per-command, would either be a data
/// race or would miss the descendants this exists to reach.
fn configure_git_credentials() {
    let Ok(exe) = std::env::current_exe() else {
        // Without our own path there is no helper to name. Git then has no
        // credentials, which is the same position a tokenless clone was in.
        return;
    };
    let pairs = [
        (
            "credential.https://github.com.helper",
            format!("{} git-credential", exe.display()),
        ),
        (
            "credential.https://github.com.useHttpPath",
            "true".to_string(),
        ),
    ];
    // SAFETY: single-threaded at this point — see the doc comment above.
    unsafe {
        std::env::set_var("GIT_CONFIG_COUNT", pairs.len().to_string());
        for (i, (key, value)) in pairs.iter().enumerate() {
            std::env::set_var(format!("GIT_CONFIG_KEY_{i}"), key);
            std::env::set_var(format!("GIT_CONFIG_VALUE_{i}"), value);
        }
    }
}

/// Apply the sandbox from `caps_file`, exiting 3 on any failure (fail-closed).
///
/// Must run BEFORE starting the async runtime. Landlock's `restrict_self`
/// confines only the calling thread plus threads and child processes created
/// AFTER it — so it must run on this single startup thread, before tokio spawns
/// its worker/blocking pool, for every worker (and any subprocess a tool later
/// forks) to inherit the confinement. Applying it inside `#[tokio::main]` left
/// workers spawned before `apply` unconfined, so a tool forked onto one of them
/// could escape the workdir non-deterministically.
fn apply_sandbox_or_exit(
    dirs: &[PathBuf],
    socket: Option<&std::path::Path>,
    caps_file: &std::path::Path,
) {
    #[cfg(feature = "sandbox")]
    {
        if let Err(e) = horsie_runtime::sandbox::apply(dirs, socket, caps_file) {
            eprintln!("sandbox apply failed: {e}");
            std::process::exit(3);
        }
    }
    #[cfg(not(feature = "sandbox"))]
    {
        let _ = (dirs, socket, caps_file);
        eprintln!("--sandbox-caps given but this binary was built without the `sandbox` feature");
        std::process::exit(3);
    }
}

/// The async body, run inside a runtime built after the sandbox was applied.
async fn run(cli: Cli, runtime_id: String, endpoint: Endpoint) {
    // Taken before `cli` is partially moved into the workspace registry below.
    let state_file = cli.state_file.clone();

    // Where this runtime keeps plugins: the bundle store and every agent's
    // tree. Bundles are fetched per agent on `ProvisionAgent`, not here — the
    // whole session's manifest used to be fetched at startup, before the
    // runtime had even dialled, which is why a failed fetch was invisible.
    //
    // A `horsie connect --plugins-dir` library also arrives through this path,
    // as plugins sitting directly under the root. An agent that selects no
    // bundles of its own is linked to them.
    let plugins_dir = std::env::var(horsie_models::ENV_PLUGINS_DIR)
        .ok()
        .map(PathBuf::from);
    let registry = Arc::new(
        horsie_runtime::workspace::WorkspaceRegistry::new(cli.workspaces)
            .with_plugins(plugins_dir, cli.hook_path),
    );

    // Per-agent cwd/env state, keyed by the agent id stamped on each tool call.
    // Built once, above the reconnect loop: a dropped socket is not a reason to
    // forget where each agent was working. File-backed when the vendor can
    // respawn this runtime, so a hibernate does not reset it either.
    let state = Arc::new(match state_file {
        Some(path) => horsie_runtime::state::RuntimeState::with_file(path),
        None => horsie_runtime::state::RuntimeState::new(),
    });

    // From the environment, never argv: a bearer in argv is readable by any
    // process on the host through `ps`.
    let token = std::env::var(horsie_models::ENV_CONNECT_TOKEN).ok();
    // The bearer rides the unix path too. A vendor verifies every dial the same
    // way whatever the socket family, so skipping it there would make a local
    // runtime unable to register at all.
    let dial_url = match &endpoint {
        Endpoint::Ws(url) => url.clone(),
        Endpoint::Unix(_) => "ws://localhost/".to_string(),
    };
    let request = match dial_request(&dial_url, token.as_deref()) {
        Ok(request) => request,
        Err(e) => {
            eprintln!("cannot dial {dial_url}: {e}");
            std::process::exit(1);
        }
    };

    // One loop per socket family rather than one shared: the two produce
    // different stream types, and `serve_until_disconnected` is generic
    // precisely so that is the only thing that differs.
    //
    // `reconnect` differs too, and that is not an accident. A unix endpoint is
    // a socket belonging to the vendor process that spawned this runtime as its
    // child: if that link drops, the parent is gone, nothing will ever answer
    // that path again, and retrying leaves an orphan burning a machine for the
    // whole connect budget. A ws endpoint is a server across a network, where a
    // restart or a blink is ordinary and coming back is the whole point.
    match endpoint {
        Endpoint::Ws(url) => {
            serve_until_disconnected(
                &format!("connect to {url}"),
                true,
                || {
                    let request = request.clone();
                    // Normalised to a string so both socket families report a
                    // failure the same way, and `is_retryable` reads one shape.
                    async move { connect_async(request).await.map_err(|e| e.to_string()) }
                },
                registry,
                runtime_id,
                state,
            )
            .await;
        }
        Endpoint::Unix(path) => {
            serve_until_disconnected(
                &format!("connect to unix socket {}", path.display()),
                false,
                || {
                    let request = request.clone();
                    let path = path.clone();
                    async move {
                        let stream = tokio::net::UnixStream::connect(&path)
                            .await
                            .map_err(|e| format!("connect failed: {e}"))?;
                        client_async(request, stream)
                            .await
                            .map_err(|e| format!("handshake failed: {e}"))
                    }
                },
                registry,
                runtime_id,
                state,
            )
            .await;
        }
    }
}

/// Dial, serve, and — when `reconnect` — dial again for as long as the process
/// lives.
///
/// A runtime across a network outlives its link. The server it dials restarts,
/// a resumed machine lands on a new socket, a laptop's network blinks — and
/// exiting on the first dropped frame takes the workspace with it. For a vendor
/// that hibernates by stopping a machine, it would mean a runtime could never
/// be resumed at all, only rebuilt from scratch.
///
/// A runtime on a local socket is the opposite case: see the call site. There
/// the first dropped frame is its parent's death, and returning is correct.
///
/// Otherwise only two things end this: a dial refused with a 4xx, which no
/// retry can change, and a connect budget exhausted. Both are reported and exit
/// non-zero, so a supervisor sees a failure rather than a silent stop.
async fn serve_until_disconnected<S, C, Fut>(
    label: &str,
    reconnect: bool,
    connect: C,
    registry: Arc<horsie_runtime::workspace::WorkspaceRegistry>,
    runtime_id: String,
    state: Arc<horsie_runtime::state::RuntimeState>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    C: Fn() -> Fut,
    Fut: Future<
        Output = Result<
            (
                WebSocketStream<S>,
                tokio_tungstenite::tungstenite::handshake::client::Response,
            ),
            String,
        >,
    >,
{
    let mut first = true;
    loop {
        let connected = retry(
            label,
            &connect,
            CONNECT_RETRIES,
            CONNECT_BASE_DELAY,
            CONNECT_MAX_DELAY,
        )
        .await;
        let ws = match connected {
            Ok((ws, _)) => ws,
            Err(e) => {
                eprintln!("failed to {label}: {e}");
                std::process::exit(1);
            }
        };
        if !first {
            eprintln!("reconnected: {label}");
        }
        first = false;

        run_loop(ws, registry.clone(), runtime_id.clone(), state.clone()).await;
        if !reconnect {
            eprintln!("link closed; the vendor that owns this runtime is gone");
            return;
        }
        eprintln!("link closed; reconnecting");
    }
}

/// Retry an async operation with capped exponential backoff. Logs each failed
/// attempt to stderr so a foreground `horsie connect` is not silent while the
/// server is unreachable.
/// Whether an error is worth another attempt.
///
/// A refused dial is terminal: the vendor rejected this runtime's credential,
/// and the identical handshake will earn the identical answer. Retrying one
/// burns the whole connect budget — thirty attempts backing off to 30s is over
/// ten minutes of waiting to learn something the first response already said.
fn is_retryable(error: &str) -> bool {
    !error.contains("HTTP error: 4")
}

async fn retry<F, Fut, T, E>(
    label: &str,
    operation: F,
    max_attempts: usize,
    base_delay: Duration,
    max_delay: Duration,
) -> Result<T, E>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let mut delay = base_delay;
    for attempt in 1..=max_attempts {
        match operation().await {
            Ok(t) => return Ok(t),
            Err(e) if attempt == max_attempts => return Err(e),
            Err(e) if !is_retryable(&e.to_string()) => {
                eprintln!("{label} was refused, which no retry can change: {e}");
                return Err(e);
            }
            Err(e) => {
                eprintln!(
                    "{label} attempt {attempt}/{max_attempts} failed: {e}; retrying in {delay:?}"
                );
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(max_delay);
            }
        }
    }
    unreachable!("loop returns inside its branches")
}

/// Refuse a request naming an agent this runtime has never provisioned.
///
/// Fail-closed on purpose. Every request that carries an `agent_id` reads that
/// agent's plugin tree, and an unprovisioned agent's tree is simply absent — so
/// without this the scan returns no skills, the hook run matches nothing, and
/// discovery finds no servers, all silently and all looking exactly like an
/// agent that legitimately selected no bundles. A sequencing bug would then
/// present as a model that has mysteriously forgotten how to do its job.
///
/// An agent that selected no bundles is still provisioned, with an empty set, so
/// it passes here. "Nothing was asked for" and "nobody asked" stay distinct.
fn refuse_unprovisioned(call_id: &str, agent_id: &str) -> RuntimeOutboundMessage {
    RuntimeOutboundMessage::RequestRefused(RequestRefused {
        call_id: call_id.to_string(),
        reason: format!(
            "agent '{agent_id}' has not been provisioned on this runtime; \
             ProvisionAgent must precede any request that reads its plugins"
        ),
    })
}

/// The runtime message loop, generic over the underlying socket so TCP and unix
/// share one implementation. Announces `RuntimeReady`, then services requests.
///
/// `Ready` goes out immediately and unconditionally. It used to be preceded by a
/// provisioning phase whose only way to report a failure was to exit — so the
/// one thing most likely to have gone wrong was the one thing no caller could
/// see. Provisioning is a request now, answered from the loop below like any
/// other.
///
/// `state` outlives the connection: it is the per-agent working directory and
/// environment, which a dropped socket has no business resetting.
async fn run_loop<S>(
    ws: WebSocketStream<S>,
    registry: Arc<horsie_runtime::workspace::WorkspaceRegistry>,
    runtime_id: String,
    state: Arc<horsie_runtime::state::RuntimeState>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (sink_raw, mut stream) = ws.split();
    let sink = Arc::new(Mutex::new(sink_raw));

    let ready = match serde_json::to_string(&RuntimeOutboundMessage::Ready(RuntimeReady {
        runtime_id: runtime_id.clone(),
    })) {
        Ok(json) => json,
        Err(e) => {
            eprintln!("serialization error: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = sink.lock().await.send(Message::Text(ready.into())).await {
        eprintln!("failed to send RuntimeReady: {e}");
        std::process::exit(1);
    }

    // in-flight task map: call_id → AbortHandle
    // Plugin-declared MCP servers, live for as long as this connection: a stdio
    // child respawned per tool call would cost more than the call.
    let mcp = Arc::new(horsie_runtime::mcp::McpRegistry::default());
    let in_flight: Arc<Mutex<HashMap<String, tokio::task::AbortHandle>>> =
        Arc::new(Mutex::new(HashMap::new()));

    while let Some(msg) = stream.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                let inbound = match serde_json::from_str::<RuntimeInboundMessage>(&text) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                match inbound {
                    RuntimeInboundMessage::ToolCall(req) => {
                        if !registry.is_provisioned(&req.agent_id) {
                            let refusal = refuse_unprovisioned(&req.call_id, &req.agent_id);
                            if let Ok(json) = serde_json::to_string(&refusal) {
                                let _ = sink.lock().await.send(Message::Text(json.into())).await;
                            }
                            continue;
                        }

                        let call_id = req.call_id.clone();
                        let agent_id = req.agent_id.clone();
                        let registry = registry.clone();
                        let state = state.clone();
                        let sink_clone = sink.clone();
                        let in_flight_clone = in_flight.clone();

                        let handle = tokio::spawn(async move {
                            // Tool hooks run here, inline with the call: this is
                            // the only place the plugin files exist, it costs no
                            // extra round-trip, and a slow hook is interrupted by
                            // the same `CancelCall` that interrupts the tool.
                            let (result, hooks) = horsie_runtime::hooks::dispatch_with_hooks(
                                &registry, &state, &agent_id, &call_id, req.call,
                            )
                            .await;
                            let response = serde_json::to_string(
                                &RuntimeOutboundMessage::ToolCallResponse(ToolCallResponse {
                                    call_id: call_id.clone(),
                                    result,
                                    hooks,
                                }),
                            );
                            if let Ok(json) = response {
                                let _ = sink_clone
                                    .lock()
                                    .await
                                    .send(Message::Text(json.into()))
                                    .await;
                            }
                            in_flight_clone.lock().await.remove(&call_id);
                        });

                        in_flight
                            .lock()
                            .await
                            .insert(req.call_id, handle.abort_handle());
                    }
                    RuntimeInboundMessage::ScanWorkspace(req) => {
                        if !registry.is_provisioned(&req.agent_id) {
                            let refusal = refuse_unprovisioned(&req.call_id, &req.agent_id);
                            if let Ok(json) = serde_json::to_string(&refusal) {
                                let _ = sink.lock().await.send(Message::Text(json.into())).await;
                            }
                            continue;
                        }

                        let call_id = req.call_id.clone();
                        let map_id = req.call_id.clone();
                        let registry = registry.clone();
                        let sink_clone = sink.clone();
                        let in_flight_clone = in_flight.clone();

                        let handle = tokio::spawn(async move {
                            let agent_id = req.agent_id.clone();
                            let workspaces = horsie_runtime::scan::exec(&registry, req);
                            let shared_skills =
                                horsie_runtime::scan::shared_skills(&registry, &agent_id);
                            let shared_root =
                                horsie_runtime::scan::shared_root(&registry, &agent_id);
                            let shared_agents =
                                horsie_runtime::scan::shared_agents(&registry, &agent_id);
                            let response = serde_json::to_string(
                                &RuntimeOutboundMessage::ScanResult(ScanResponse {
                                    call_id: call_id.clone(),
                                    workspaces,
                                    shared_skills,
                                    shared_agents: Some(shared_agents),
                                    shared_root,
                                }),
                            );
                            if let Ok(json) = response {
                                let _ = sink_clone
                                    .lock()
                                    .await
                                    .send(Message::Text(json.into()))
                                    .await;
                            }
                            in_flight_clone.lock().await.remove(&call_id);
                        });

                        in_flight.lock().await.insert(map_id, handle.abort_handle());
                    }
                    RuntimeInboundMessage::Ping(req) => {
                        // On its own task, like every other request, and for a
                        // reason this one cannot compromise on: the caller fails
                        // *every* outstanding call against a runtime that does
                        // not answer a ping. A ping queued behind a running tool
                        // would therefore abort exactly the long work it exists
                        // to protect, so answering concurrently is the contract
                        // rather than an optimisation.
                        //
                        // Deliberately not entered in the in-flight map: a ping
                        // that reported itself as executing would make every
                        // runtime look permanently busy, and what the caller
                        // reconciles against is the tool calls it issued.
                        let sink_clone = sink.clone();
                        let in_flight_clone = in_flight.clone();
                        tokio::spawn(async move {
                            let executing: Vec<String> =
                                in_flight_clone.lock().await.keys().cloned().collect();
                            let response = serde_json::to_string(&RuntimeOutboundMessage::Pong(
                                PongResponse {
                                    call_id: req.call_id,
                                    in_flight: executing,
                                },
                            ));
                            if let Ok(json) = response {
                                let _ = sink_clone
                                    .lock()
                                    .await
                                    .send(Message::Text(json.into()))
                                    .await;
                            }
                        });
                    }
                    RuntimeInboundMessage::ProvisionWorkspace(req) => {
                        let call_id = req.call_id.clone();
                        let map_id = req.call_id.clone();
                        let registry = registry.clone();
                        let sink_clone = sink.clone();
                        let in_flight_clone = in_flight.clone();

                        // Registered in `in_flight` like a tool call, and for the
                        // same two reasons: a clone of a large repository is
                        // exactly the long work the caller's reconciler must see
                        // as running rather than cancel as an orphan, and exactly
                        // the work a user hitting Stop must be able to abandon.
                        let handle = tokio::spawn(async move {
                            // Named before the run, so a failure still reports
                            // the steps that were asked for. `run_steps` is
                            // fail-fast, so anything after the failing step did
                            // not run — the reason names which one it was.
                            let applied: Vec<String> =
                                req.steps.iter().map(|s| s.name.clone()).collect();
                            let result =
                                match horsie_runtime::steps::run_steps(&registry, &req.steps).await
                                {
                                    Ok(()) => ProvisionResult::Ok(ProvisionOk { applied }),
                                    Err(reason) => ProvisionResult::Err(ProvisionError { reason }),
                                };
                            let response =
                                serde_json::to_string(&RuntimeOutboundMessage::ProvisionResult(
                                    ProvisionWorkspaceResponse {
                                        call_id: call_id.clone(),
                                        result,
                                    },
                                ));
                            if let Ok(json) = response {
                                let _ = sink_clone
                                    .lock()
                                    .await
                                    .send(Message::Text(json.into()))
                                    .await;
                            }
                            in_flight_clone.lock().await.remove(&call_id);
                        });

                        in_flight.lock().await.insert(map_id, handle.abort_handle());
                    }
                    RuntimeInboundMessage::ProvisionAgent(req) => {
                        let call_id = req.call_id.clone();
                        let map_id = req.call_id.clone();
                        let registry = registry.clone();
                        let sink_clone = sink.clone();
                        let in_flight_clone = in_flight.clone();

                        // Tracked like every other command: installing a fleet
                        // of bundles is a download, and a user hitting Stop must
                        // be able to abandon it.
                        let handle = tokio::spawn(async move {
                            let agent_id = req.agent_id.clone();
                            let bundles: Vec<horsie_runtime::plugin_store::BundleRef> =
                                req.bundles.iter().map(Into::into).collect();
                            let outcome = match registry.plugins_root() {
                                Some(root) => {
                                    let store = horsie_runtime::plugin_store::PluginStore::new(
                                        root.to_path_buf(),
                                    );
                                    let source = horsie_runtime::plugin_store::HttpBundles::new(
                                        std::env::var(horsie_models::ENV_SERVER_URL)
                                            .unwrap_or_default(),
                                        std::env::var(horsie_models::ENV_CONNECT_TOKEN).ok(),
                                    );
                                    store
                                        .provision_agent(&agent_id, &bundles, &source)
                                        .await
                                        .map(|dir| dir.display().to_string())
                                }
                                // No plugins root at all. An empty set is still a
                                // success — the agent asked for nothing and got
                                // nothing — but anything else cannot be honoured
                                // and must say so rather than come up bare.
                                None if bundles.is_empty() => Ok(String::new()),
                                None => {
                                    Err("this runtime has nowhere to install plugins".to_string())
                                }
                            };
                            let (result, root) = match outcome {
                                Ok(root) => (
                                    ProvisionResult::Ok(ProvisionOk {
                                        applied: Vec::new(),
                                    }),
                                    root,
                                ),
                                Err(reason) => (
                                    ProvisionResult::Err(ProvisionError { reason }),
                                    String::new(),
                                ),
                            };
                            let response = serde_json::to_string(
                                &RuntimeOutboundMessage::AgentProvisioned(ProvisionAgentResponse {
                                    call_id: call_id.clone(),
                                    root,
                                    result,
                                }),
                            );
                            if let Ok(json) = response {
                                let _ = sink_clone
                                    .lock()
                                    .await
                                    .send(Message::Text(json.into()))
                                    .await;
                            }
                            in_flight_clone.lock().await.remove(&call_id);
                        });

                        in_flight.lock().await.insert(map_id, handle.abort_handle());
                    }
                    RuntimeInboundMessage::CancelCall(req) => {
                        if let Some(handle) = in_flight.lock().await.remove(&req.call_id) {
                            handle.abort();
                        }
                        // Its own reply, not a tool-shaped one. Every
                        // server-initiated command is cancellable now, and a
                        // synthetic `ToolCallResponse` resolved five of their
                        // waiters with "the runtime answered a workspace scan
                        // with the wrong message" — a protocol confusion
                        // reported in place of the cancellation that happened.
                        let response = serde_json::to_string(&RuntimeOutboundMessage::Cancelled(
                            CancelledResponse {
                                call_id: req.call_id,
                            },
                        ));
                        if let Ok(json) = response {
                            let _ = sink.lock().await.send(Message::Text(json.into())).await;
                        }
                    }
                    RuntimeInboundMessage::RunHooks(req) => {
                        if !registry.is_provisioned(&req.agent_id) {
                            let refusal = refuse_unprovisioned(&req.call_id, &req.agent_id);
                            if let Ok(json) = serde_json::to_string(&refusal) {
                                let _ = sink.lock().await.send(Message::Text(json.into())).await;
                            }
                            continue;
                        }

                        let call_id = req.call_id.clone();
                        let map_id = req.call_id.clone();
                        let event = req.event;
                        let hook_agent = req.agent_id.clone();
                        let registry = registry.clone();
                        let sink_clone = sink.clone();
                        let in_flight_clone = in_flight.clone();

                        // Spawned and registered in `in_flight` like a tool
                        // call, so a slow hook stays cancellable: a user hitting
                        // Stop must not have to wait out a 30-second guard.
                        let handle = tokio::spawn(async move {
                            let records =
                                horsie_runtime::hooks::run_hooks(&registry, &hook_agent, &event)
                                    .await;
                            let response = serde_json::to_string(
                                &RuntimeOutboundMessage::HookRecords(RunHooksResponse {
                                    call_id: call_id.clone(),
                                    records,
                                }),
                            );
                            if let Ok(json) = response {
                                let _ = sink_clone
                                    .lock()
                                    .await
                                    .send(Message::Text(json.into()))
                                    .await;
                            }
                            in_flight_clone.lock().await.remove(&call_id);
                        });

                        in_flight.lock().await.insert(map_id, handle.abort_handle());
                    }
                    RuntimeInboundMessage::McpDiscover(req) => {
                        if !registry.is_provisioned(&req.agent_id) {
                            let refusal = refuse_unprovisioned(&req.call_id, &req.agent_id);
                            if let Ok(json) = serde_json::to_string(&refusal) {
                                let _ = sink.lock().await.send(Message::Text(json.into())).await;
                            }
                            continue;
                        }

                        let call_id = req.call_id.clone();
                        let map_id = req.call_id.clone();
                        let mcp_agent = req.agent_id.clone();
                        let registry = registry.clone();
                        let mcp = mcp.clone();
                        let sink_clone = sink.clone();
                        let in_flight_clone = in_flight.clone();

                        // Cancellable like a hook run: starting a fleet of MCP
                        // servers can take seconds, and a user hitting Stop must
                        // not wait them all out.
                        let handle = tokio::spawn(async move {
                            let discovery = match registry.plugins_dir_for(&mcp_agent) {
                                Some(dir) => {
                                    mcp.discover(&mcp_agent, &dir, registry.default_cwd()).await
                                }
                                None => horsie_runtime::mcp::Discovery {
                                    tools: Vec::new(),
                                    failures: Vec::new(),
                                },
                            };
                            let response = serde_json::to_string(
                                &RuntimeOutboundMessage::McpTools(McpDiscoverResponse {
                                    call_id: call_id.clone(),
                                    tools: discovery.tools,
                                    failures: discovery.failures,
                                }),
                            );
                            if let Ok(json) = response {
                                let _ = sink_clone
                                    .lock()
                                    .await
                                    .send(Message::Text(json.into()))
                                    .await;
                            }
                            in_flight_clone.lock().await.remove(&call_id);
                        });

                        in_flight.lock().await.insert(map_id, handle.abort_handle());
                    }
                    RuntimeInboundMessage::McpInvoke(req) => {
                        if !registry.is_provisioned(&req.agent_id) {
                            let refusal = refuse_unprovisioned(&req.call_id, &req.agent_id);
                            if let Ok(json) = serde_json::to_string(&refusal) {
                                let _ = sink.lock().await.send(Message::Text(json.into())).await;
                            }
                            continue;
                        }

                        let call_id = req.call_id.clone();
                        let map_id = req.call_id.clone();
                        let mcp_agent = req.agent_id.clone();
                        let tool = req.tool.clone();
                        let arguments = req.arguments.clone();
                        let registry = registry.clone();
                        let mcp = mcp.clone();
                        let sink_clone = sink.clone();
                        let in_flight_clone = in_flight.clone();

                        let handle = tokio::spawn(async move {
                            let args: serde_json::Value =
                                serde_json::from_str(&arguments).unwrap_or(serde_json::Value::Null);
                            let result = match registry.plugins_dir_for(&mcp_agent) {
                                Some(dir) => mcp
                                    .invoke(&mcp_agent, &dir, registry.default_cwd(), &tool, args)
                                    .await
                                    .map(|stdout| {
                                        ToolResult::Ok(ToolOutput {
                                            stdout,
                                            stderr: String::new(),
                                            exit_code: 0,
                                        })
                                    })
                                    .unwrap_or_else(|reason| ToolResult::Err(ToolError { reason })),
                                None => ToolResult::Err(ToolError {
                                    reason: "this runtime has no plugin library".to_string(),
                                }),
                            };
                            let response = serde_json::to_string(
                                &RuntimeOutboundMessage::McpResult(McpInvokeResponse {
                                    call_id: call_id.clone(),
                                    result,
                                }),
                            );
                            if let Ok(json) = response {
                                let _ = sink_clone
                                    .lock()
                                    .await
                                    .send(Message::Text(json.into()))
                                    .await;
                            }
                            in_flight_clone.lock().await.remove(&call_id);
                        });

                        in_flight.lock().await.insert(map_id, handle.abort_handle());
                    }
                }
            }
            Ok(Message::Close(_)) | Err(_) => break,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn retry_succeeds_after_failures() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let attempts = AtomicUsize::new(0);
        let result = retry(
            "test",
            || async {
                let n = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                if n < 3 {
                    Err(format!("attempt {n}"))
                } else {
                    Ok("success")
                }
            },
            5,
            Duration::from_millis(1),
            Duration::from_millis(10),
        )
        .await;
        assert_eq!(result, Ok("success"));
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retry_exhausts_attempts_and_returns_last_error() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let attempts = AtomicUsize::new(0);
        let result: Result<&str, String> = retry(
            "test",
            || async {
                let n = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                Err(format!("attempt {n}"))
            },
            3,
            Duration::from_millis(1),
            Duration::from_millis(10),
        )
        .await;
        assert_eq!(result, Err("attempt 3".to_string()));
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn parse_endpoint_ws() {
        assert!(matches!(
            parse_endpoint("ws://localhost:8080"),
            Ok(Endpoint::Ws(_))
        ));
        assert!(matches!(
            parse_endpoint("wss://example.com/socket"),
            Ok(Endpoint::Ws(_))
        ));
    }

    #[test]
    fn parse_endpoint_unix() {
        match parse_endpoint("unix:/tmp/rt.sock") {
            Ok(Endpoint::Unix(p)) => assert_eq!(p, PathBuf::from("/tmp/rt.sock")),
            Ok(Endpoint::Ws(_)) => panic!("expected unix endpoint, got ws"),
            Err(e) => panic!("expected unix endpoint, got error: {e}"),
        }
    }

    #[test]
    fn parse_endpoint_bad_scheme() {
        assert!(parse_endpoint("http://localhost").is_err());
        assert!(parse_endpoint("localhost:9000").is_err());
    }
}
