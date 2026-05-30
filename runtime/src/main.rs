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
use models::runtime::{
    RuntimeInboundMessage, RuntimeOutboundMessage, RuntimeReady, ToolCallResponse, ToolError,
    ToolResult,
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
    #[arg(long)]
    working_dir: PathBuf,
    /// Confine tool execution with the nono sandbox before connecting (fail-closed).
    #[arg(long)]
    sandbox: bool,
    /// Extra read-only paths granted inside the sandbox.
    #[arg(long = "sandbox-read")]
    sandbox_read: Vec<PathBuf>,
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

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let endpoint = match parse_endpoint(&cli.endpoint) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };

    if cli.sandbox {
        #[cfg(feature = "sandbox")]
        {
            let socket = match &endpoint {
                Endpoint::Unix(p) => Some(p.as_path()),
                Endpoint::Ws(_) => None,
            };
            if let Err(e) = runtime::sandbox::apply(&cli.working_dir, socket, &cli.sandbox_read) {
                eprintln!("sandbox apply failed: {e}");
                std::process::exit(3);
            }
        }
        #[cfg(not(feature = "sandbox"))]
        {
            eprintln!(
                "--sandbox requested but this binary was built without the `sandbox` feature"
            );
            std::process::exit(3);
        }
    }

    match endpoint {
        Endpoint::Ws(url) => match connect_async(&url).await {
            Ok((ws, _)) => run_loop(ws, cli.working_dir, cli.runtime_id).await,
            Err(e) => {
                eprintln!("failed to connect to {url}: {e}");
                std::process::exit(1);
            }
        },
        Endpoint::Unix(path) => match tokio::net::UnixStream::connect(&path).await {
            Ok(stream) => match client_async("ws://localhost/", stream).await {
                Ok((ws, _)) => run_loop(ws, cli.working_dir, cli.runtime_id).await,
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
async fn run_loop<S>(ws: WebSocketStream<S>, working_dir: PathBuf, runtime_id: String)
where
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
                        let working_dir = working_dir.clone();
                        let sink_clone = sink.clone();
                        let in_flight_clone = in_flight.clone();

                        let handle = tokio::spawn(async move {
                            let result = runtime::tools::dispatch(&working_dir, req.call).await;
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
