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
    RuntimeInboundMessage, RuntimeOutboundMessage, RuntimeProvisionFailed, RuntimeProvisioning,
    RuntimeReady, ScanResponse, SessionStartResponse, ToolCallResponse, ToolError, ToolResult,
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
    /// Shared plugin library root, exposed to agents as the `horsie_shared`
    /// workspace (read-only). Absent → no shared library.
    #[arg(long = "plugins-dir")]
    plugins_dir: Option<PathBuf>,
    /// Directory prepended to PATH when running plugin hooks (repeatable), e.g. the
    /// node bin dir.
    #[arg(long = "hook-path")]
    hook_path: Vec<PathBuf>,
}

#[derive(Subcommand)]
enum Commands {
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
    // In-sandbox hackamore self-provisioning — under the same confinement as the
    // job and before the message loop. Fail closed: a daemon that injected
    // hackamore env expects a provisioned runtime, so any failure fails the job.
    if let Err(e) = horsie_runtime::provision::provision_from_env().await {
        eprintln!("hackamore provisioning failed: {e}");
        std::process::exit(4);
    }

    // Provision steps (vendor-injected JSON). Parsed before connecting so a
    // malformed payload fails fast; executed after connecting so failures are
    // reported over the wire instead of as a silent death.
    let steps = match horsie_runtime::steps::steps_from_env(
        std::env::var(horsie_models::ENV_PROVISION).ok(),
    ) {
        Ok(steps) => steps,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(5);
        }
    };

    // Fetch the session's selected plugin bundles (if the server injected a
    // manifest) and scan that dir; otherwise fall back to any `--plugins-dir`.
    let plugins_dir = match horsie_runtime::plugins_fetch::provision_plugins().await {
        Some(dir) => Some(dir),
        None => cli.plugins_dir,
    };
    let registry = Arc::new(
        horsie_runtime::workspace::WorkspaceRegistry::new(cli.workspaces)
            .with_plugins(plugins_dir, cli.hook_path),
    );

    match endpoint {
        Endpoint::Ws(url) => {
            let ws = match retry(
                &format!("connect to {url}"),
                || connect_async(url.clone()),
                CONNECT_RETRIES,
                CONNECT_BASE_DELAY,
                CONNECT_MAX_DELAY,
            )
            .await
            {
                Ok((ws, _)) => ws,
                Err(e) => {
                    eprintln!("failed to connect to {url}: {e}");
                    std::process::exit(1);
                }
            };
            run_loop(ws, registry, runtime_id, steps).await;
        }
        Endpoint::Unix(path) => {
            let ws = match retry(
                &format!("connect to unix socket {}", path.display()),
                || async {
                    let stream = tokio::net::UnixStream::connect(&path)
                        .await
                        .map_err(|e| format!("connect failed: {e}"))?;
                    client_async("ws://localhost/", stream)
                        .await
                        .map_err(|e| format!("handshake failed: {e}"))
                },
                CONNECT_RETRIES,
                CONNECT_BASE_DELAY,
                CONNECT_MAX_DELAY,
            )
            .await
            {
                Ok((ws, _)) => ws,
                Err(e) => {
                    eprintln!("failed to connect to unix socket {}: {e}", path.display());
                    std::process::exit(1);
                }
            };
            run_loop(ws, registry, runtime_id, steps).await;
        }
    }
}

/// Retry an async operation with capped exponential backoff. Logs each failed
/// attempt to stderr so a foreground `horsie connect` is not silent while the
/// server is unreachable.
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

/// The runtime message loop, generic over the underlying socket so TCP and unix
/// share one implementation. Announces `RuntimeReady`, then services tool calls.
async fn run_loop<S>(
    ws: WebSocketStream<S>,
    registry: Arc<horsie_runtime::workspace::WorkspaceRegistry>,
    runtime_id: String,
    steps: Vec<horsie_models::executor::ProvisionStep>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (sink_raw, mut stream) = ws.split();
    let sink = Arc::new(Mutex::new(sink_raw));

    if !steps.is_empty() {
        let announce = match serde_json::to_string(&RuntimeOutboundMessage::Provisioning(
            RuntimeProvisioning {
                runtime_id: runtime_id.clone(),
            },
        )) {
            Ok(json) => json,
            Err(e) => {
                eprintln!("serialization error: {e}");
                std::process::exit(1);
            }
        };
        if let Err(e) = sink.lock().await.send(Message::Text(announce.into())).await {
            eprintln!("failed to send Provisioning: {e}");
            std::process::exit(1);
        }
        let token = std::env::var(horsie_models::ENV_GITHUB_TOKEN).ok();
        if let Err(message) =
            horsie_runtime::steps::run_steps(&registry, &steps, token.as_deref()).await
        {
            eprintln!("provisioning failed: {message}");
            if let Ok(json) = serde_json::to_string(&RuntimeOutboundMessage::ProvisionFailed(
                RuntimeProvisionFailed {
                    runtime_id: runtime_id.clone(),
                    message,
                },
            )) {
                let _ = sink.lock().await.send(Message::Text(json.into())).await;
                let _ = sink.lock().await.flush().await;
            }
            std::process::exit(5);
        }
    }

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
    let in_flight: Arc<Mutex<HashMap<String, tokio::task::AbortHandle>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Per-agent cwd/env state, keyed by the agent id stamped on each tool call;
    // shared by every task this connection spawns.
    let state = Arc::new(horsie_runtime::state::RuntimeState::new());

    while let Some(msg) = stream.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                let inbound = match serde_json::from_str::<RuntimeInboundMessage>(&text) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                match inbound {
                    RuntimeInboundMessage::ToolCall(req) => {
                        let call_id = req.call_id.clone();
                        let agent_id = req.agent_id.clone();
                        let registry = registry.clone();
                        let state = state.clone();
                        let sink_clone = sink.clone();
                        let in_flight_clone = in_flight.clone();

                        let handle = tokio::spawn(async move {
                            let result = horsie_runtime::tools::dispatch(
                                &registry, &state, &agent_id, req.call,
                            )
                            .await;
                            let response = serde_json::to_string(
                                &RuntimeOutboundMessage::ToolCallResponse(ToolCallResponse {
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

                        in_flight
                            .lock()
                            .await
                            .insert(req.call_id, handle.abort_handle());
                    }
                    RuntimeInboundMessage::ScanWorkspace(req) => {
                        let call_id = req.call_id.clone();
                        let map_id = req.call_id.clone();
                        let registry = registry.clone();
                        let sink_clone = sink.clone();
                        let in_flight_clone = in_flight.clone();

                        let handle = tokio::spawn(async move {
                            let include_shared = req.include_shared;
                            let workspaces = horsie_runtime::scan::exec(&registry, req);
                            let shared_skills =
                                horsie_runtime::scan::shared_skills(&registry, include_shared);
                            let response = serde_json::to_string(
                                &RuntimeOutboundMessage::ScanResult(ScanResponse {
                                    call_id: call_id.clone(),
                                    workspaces,
                                    shared_skills,
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
                        let response = serde_json::to_string(
                            &RuntimeOutboundMessage::ToolCallResponse(ToolCallResponse {
                                call_id: req.call_id,
                                result: ToolResult::Err(ToolError {
                                    reason: "cancelled".to_string(),
                                }),
                            }),
                        );
                        if let Ok(json) = response {
                            let _ = sink.lock().await.send(Message::Text(json.into())).await;
                        }
                    }
                    RuntimeInboundMessage::SessionStart(req) => {
                        let call_id = req.call_id.clone();
                        let map_id = req.call_id.clone();
                        let registry = registry.clone();
                        let sink_clone = sink.clone();
                        let in_flight_clone = in_flight.clone();

                        let handle = tokio::spawn(async move {
                            let context = match registry.plugins_dir() {
                                Some(dir) => {
                                    horsie_runtime::plugins::run_session_start(
                                        dir,
                                        registry.hook_path(),
                                    )
                                    .await
                                }
                                None => String::new(),
                            };
                            let response = serde_json::to_string(
                                &RuntimeOutboundMessage::SessionStartResult(SessionStartResponse {
                                    call_id: call_id.clone(),
                                    context,
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
