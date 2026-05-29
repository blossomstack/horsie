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
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[derive(Parser)]
struct Cli {
    #[arg(long)]
    executor_url: String,
    #[arg(long)]
    runtime_id: String,
    #[arg(long)]
    working_dir: PathBuf,
}

type WsSink = Arc<
    Mutex<
        futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            Message,
        >,
    >,
>;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let (ws, _) = connect_async(&cli.executor_url).await.unwrap_or_else(|e| {
        eprintln!("failed to connect to executor: {e}");
        std::process::exit(1);
    });

    let (sink_raw, mut stream) = ws.split();
    let sink: WsSink = Arc::new(Mutex::new(sink_raw));

    // Announce ourselves
    let ready = serde_json::to_string(&RuntimeOutboundMessage::Ready(RuntimeReady {
        runtime_id: cli.runtime_id.clone(),
    }))
    .unwrap_or_else(|e| {
        eprintln!("serialization error: {e}");
        std::process::exit(1);
    });
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
                        let working_dir = cli.working_dir.clone();
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
                        // Send cancelled response
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
