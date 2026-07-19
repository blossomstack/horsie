//! Standalone process wrapper for the mock Anthropic LLM server.
//!
//! The crate is primarily a library (`MockLlmServer::builder()…`) used
//! in-process by Rust integration tests. This binary lets the same server run
//! as its own process so out-of-process harnesses — notably the web-UI
//! Playwright suite — can point a real `horsie-server` at it and program
//! deterministic responses over the control plane (`/queue`, `/reset`,
//! `/scenarios/*`).
//!
//! Usage: `horsie-mock-llm [--port <N>] [--bind-all]`. With no `--port` an
//! ephemeral port is chosen; the bound URL is printed to stdout as
//! `mock-llm listening on <url>` so a parent process can capture it.

use horsie_mock_llm::MockLlmServer;

#[tokio::main]
async fn main() {
    let mut port: u16 = 0;
    let mut bind_all = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => {
                port = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| fail("--port requires a valid u16"));
            }
            "--bind-all" => bind_all = true,
            "-h" | "--help" => {
                println!("Usage: horsie-mock-llm [--port <N>] [--bind-all]");
                return;
            }
            other => fail(&format!("unknown argument: {other}")),
        }
    }

    // Fall back to $PORT if no --port was given (some CI harnesses set it).
    if port == 0
        && let Ok(v) = std::env::var("PORT")
        && let Ok(p) = v.parse()
    {
        port = p;
    }

    let mut builder = MockLlmServer::builder().port(port);
    if bind_all {
        builder = builder.bind_all_interfaces();
    }
    let server = builder.build().await;

    // Print the resolved URL so a parent harness can read the actual port
    // (important when --port 0 picks an ephemeral one). Flush by using println!.
    println!("mock-llm listening on {}", server.url());

    // The server runs on a background task inside `build()`. Park forever; the
    // parent harness terminates the process at teardown.
    std::future::pending::<()>().await;
}

fn fail(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(2);
}
