#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::wildcard_enum_match_arm
    )
)]

use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use horsie_models::runtime::{
    RuntimeInboundMessage, RuntimeOutboundMessage, RuntimeProvisionFailed, RuntimeProvisioning,
    RuntimeReady, ScanResponse, SessionStartResponse, ToolCallResponse, ToolError, ToolResult,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex;
use tokio_tungstenite::{WebSocketStream, client_async, connect_async, tungstenite::Message};

#[derive(Parser)]
struct Cli {
    /// `ws://host:port` (TCP/WebSocket) or `unix:/path/to.sock` (unix socket).
    #[arg(long)]
    endpoint: String,
    #[arg(long)]
    runtime_id: String,
    /// Repeatable `name=path` workspace root. At least one is required.
    #[arg(long = "workspace", required = true, value_parser = parse_workspace_arg)]
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

    let endpoint = match parse_endpoint(&cli.endpoint) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };

    // Apply the sandbox BEFORE starting the async runtime. Landlock's
    // `restrict_self` confines only the calling thread plus threads and child
    // processes created AFTER it — so it must run on this single startup thread,
    // before tokio spawns its worker/blocking pool, for every worker (and any
    // subprocess a tool later forks) to inherit the confinement. Applying it
    // inside `#[tokio::main]` left workers spawned before `apply` unconfined, so
    // a tool forked onto one of them could escape the workdir non-deterministically.
    if let Some(caps_file) = &cli.sandbox_caps {
        #[cfg(feature = "sandbox")]
        {
            let socket = match &endpoint {
                Endpoint::Unix(p) => Some(p.as_path()),
                Endpoint::Ws(_) => None,
            };
            let dirs: Vec<PathBuf> = cli.workspaces.iter().map(|w| w.path.clone()).collect();
            if let Err(e) = horsie_runtime::sandbox::apply(&dirs, socket, caps_file) {
                eprintln!("sandbox apply failed: {e}");
                std::process::exit(3);
            }
        }
        #[cfg(not(feature = "sandbox"))]
        {
            let _ = caps_file;
            eprintln!(
                "--sandbox-caps given but this binary was built without the `sandbox` feature"
            );
            std::process::exit(3);
        }
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
    runtime.block_on(run(cli, endpoint));
}

/// The async body, run inside a runtime built after the sandbox was applied.
async fn run(cli: Cli, endpoint: Endpoint) {
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
        Endpoint::Ws(url) => match connect_async(&url).await {
            Ok((ws, _)) => run_loop(ws, registry, cli.runtime_id, steps).await,
            Err(e) => {
                eprintln!("failed to connect to {url}: {e}");
                std::process::exit(1);
            }
        },
        Endpoint::Unix(path) => match tokio::net::UnixStream::connect(&path).await {
            Ok(stream) => match client_async("ws://localhost/", stream).await {
                Ok((ws, _)) => run_loop(ws, registry, cli.runtime_id, steps).await,
                Err(e) => {
                    eprintln!("ws handshake failed on unix socket: {e}");
                    std::process::exit(1);
                }
            },
            Err(e) => {
                eprintln!("failed to connect to unix socket {}: {e}", path.display());
                std::process::exit(1);
            }
        },
    }
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
                        let registry = registry.clone();
                        let sink_clone = sink.clone();
                        let in_flight_clone = in_flight.clone();

                        let handle = tokio::spawn(async move {
                            let result = horsie_runtime::tools::dispatch(&registry, req.call).await;
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
