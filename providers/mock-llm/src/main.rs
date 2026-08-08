//! Runs `async-llm`'s mock LLM server as its own process.
//!
//! Rust tests use `async_llm::mock::MockLlmServer` in-process. The web-UI
//! Playwright suite cannot: it needs a real `horsie-server` talking to a real
//! socket, so it launches this. The crate exists only to give that harness a
//! `cargo build -p horsie-mock-llm` target — a binary in a registry dependency
//! is not buildable from this workspace.
//!
//! Usage: `horsie-mock-llm [--port <N>] [--bind-all]`.

#[tokio::main]
async fn main() {
    async_llm::mock::run_cli().await;
}
