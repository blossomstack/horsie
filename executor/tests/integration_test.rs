#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::wildcard_enum_match_arm
    )
)]

use async_trait::async_trait;
use executor::{Executor, HealthStatus, RuntimeError, RuntimeHandle, RuntimeProvider};
use models::executor::{ExecutorEvent, RuntimeConfig, RuntimeState};
use server::{ExecutorEventHandler, Server};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

// --- test doubles ---

struct MockHandle;

#[async_trait]
impl RuntimeHandle for MockHandle {
    async fn stop(&self) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn health_check(&self) -> Result<HealthStatus, RuntimeError> {
        Ok(HealthStatus::Healthy)
    }
}

struct MockProvider;

#[async_trait]
impl RuntimeProvider for MockProvider {
    async fn create(
        &self,
        _id: &str,
        _config: &RuntimeConfig,
    ) -> Result<Arc<dyn RuntimeHandle>, RuntimeError> {
        Ok(Arc::new(MockHandle))
    }
}

struct CollectingHandler {
    events: Mutex<Vec<(String, String, ExecutorEvent)>>,
}

impl CollectingHandler {
    fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }

    fn events(&self) -> Vec<(String, String, ExecutorEvent)> {
        self.events.lock().unwrap().clone()
    }
}

impl ExecutorEventHandler for CollectingHandler {
    fn on_event(&self, executor_id: &str, request_id: &str, event: &ExecutorEvent) {
        self.events.lock().unwrap().push((
            executor_id.to_string(),
            request_id.to_string(),
            event.clone(),
        ));
    }
}

// --- helpers ---

async fn start_server() -> (Server, Arc<CollectingHandler>) {
    let handler = Arc::new(CollectingHandler::new());
    let server = Server::bind("127.0.0.1:0", handler.clone()).await.unwrap();
    (server, handler)
}

fn make_executor(id: &str, port: u16) -> (Executor, CancellationToken) {
    let cancel = CancellationToken::new();
    let executor = Executor::new(
        id.to_string(),
        format!("ws://127.0.0.1:{port}"),
        Box::new(MockProvider),
    )
    .with_health_check_interval(Duration::from_secs(3600));
    (executor, cancel)
}

// --- tests ---

#[tokio::test]
async fn test_executor_connects_and_creates_runtime() {
    let (server, handler) = start_server().await;
    let port = server.local_addr().port();
    let (executor, cancel) = make_executor("ex-1", port);
    let c = cancel.clone();
    tokio::spawn(async move { executor.run(c).await });

    tokio::time::sleep(Duration::from_millis(100)).await;

    server
        .create_runtime("ex-1", "rt-1", RuntimeConfig {})
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    let events = handler.events();
    let got_running = events.iter().any(|(_, _, ev)| {
        if let ExecutorEvent::RuntimeStateChanged(e) = ev {
            e.runtime_id == "rt-1" && matches!(e.state, RuntimeState::Running)
        } else {
            false
        }
    });
    assert!(
        got_running,
        "expected RuntimeStateChanged(Running) for rt-1; got: {events:?}"
    );

    cancel.cancel();
}

#[tokio::test]
async fn test_query_runtimes_returns_created_runtime() {
    let (server, handler) = start_server().await;
    let port = server.local_addr().port();
    let (executor, cancel) = make_executor("ex-2", port);
    let c = cancel.clone();
    tokio::spawn(async move { executor.run(c).await });

    tokio::time::sleep(Duration::from_millis(100)).await;

    server
        .create_runtime("ex-2", "rt-a", RuntimeConfig {})
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    server.query_runtimes("ex-2").await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let events = handler.events();
    let listed = events.iter().any(|(_, _, ev)| {
        if let ExecutorEvent::RuntimesListed(e) = ev {
            e.runtimes.iter().any(|r| r.runtime_id == "rt-a")
        } else {
            false
        }
    });
    assert!(
        listed,
        "expected RuntimesListed containing rt-a; got: {events:?}"
    );

    cancel.cancel();
}

#[tokio::test]
async fn test_destroy_runtime_transitions_to_stopped() {
    let (server, handler) = start_server().await;
    let port = server.local_addr().port();
    let (executor, cancel) = make_executor("ex-3", port);
    let c = cancel.clone();
    tokio::spawn(async move { executor.run(c).await });

    tokio::time::sleep(Duration::from_millis(100)).await;

    server
        .create_runtime("ex-3", "rt-b", RuntimeConfig {})
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    server.destroy_runtime("ex-3", "rt-b").await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let events = handler.events();
    let got_stopped = events.iter().any(|(_, _, ev)| {
        if let ExecutorEvent::RuntimeStateChanged(e) = ev {
            e.runtime_id == "rt-b" && matches!(e.state, RuntimeState::Stopped)
        } else {
            false
        }
    });
    assert!(
        got_stopped,
        "expected RuntimeStateChanged(Stopped) for rt-b; got: {events:?}"
    );

    cancel.cancel();
}

#[tokio::test]
async fn test_command_to_unknown_executor_fails() {
    let (server, _handler) = start_server().await;
    let result = server
        .create_runtime("nobody", "rt-x", RuntimeConfig {})
        .await;
    assert!(result.is_err());
}
