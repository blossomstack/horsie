//! Recovery-path end-to-end tests for `AgentActor`.
//!
//! Each test seeds a journal with hand-written `AgentDomainEvent`s — the state a
//! previous incarnation would have left behind — recovers a real `AgentActor` on
//! it, takes a turn against `MockLlmServer` through `AnthropicProvider`, and
//! asserts on the request that actually reached the wire.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use async_trait::async_trait;
use horsie_actor::{InMemoryJournal, Journal, spawn_root};
use horsie_agentcore::{
    AgentEvent, ContentPart, EventSink, EventSinkError, LlmProvider, Message, Role, ToolCallError,
    ToolCallPart, ToolSpec, Toolbox,
};
use horsie_anthropic::AnthropicProvider;
use horsie_mock_llm::MockLlmServer;
use horsie_models::agent::TextPart;
use horsie_workflow::{
    AgentActor, AgentCommand, AgentDomainEvent, AgentOutcome, AgentOutcomeSink, AgentParams,
    AgentRuntimeContext, FixedContextProvider,
};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

// ── harness ──────────────────────────────────────────────────────────────────

struct NoopSink;
#[async_trait]
impl EventSink for NoopSink {
    async fn emit(&self, _event: AgentEvent) -> Result<(), EventSinkError> {
        Ok(())
    }
}

/// Forwards the agent's terminal outcome to the test, so it can await the turn
/// instead of polling the journal.
struct OutcomeChannel(tokio::sync::mpsc::Sender<AgentOutcome>);

#[async_trait]
impl AgentOutcomeSink for OutcomeChannel {
    async fn deliver(&self, outcome: AgentOutcome) {
        let _ = self.0.send(outcome).await;
    }
}

/// Advertises `read_file` so the recovered history's tool call names a tool the
/// agent still has. Calling it is a test failure — these turns are plain text.
struct ReadFileToolbox;

#[async_trait]
impl Toolbox for ReadFileToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "read_file".into(),
            description: "read a file".into(),
            input_schema: json!({"type": "object"}),
        }]
    }

    async fn execute(&self, name: &str, _input: Value) -> Result<Value, ToolCallError> {
        Err(ToolCallError::ExecutionFailed(format!(
            "unexpected tool call: {name}"
        )))
    }
}

fn provider_at(url: &str) -> Arc<dyn LlmProvider> {
    Arc::new(
        AnthropicProvider::with_api_key("test-key")
            .unwrap()
            .with_base_url(url)
            .with_retry_delay_secs(0),
    )
}

fn assistant_tool_call(id: &str, call_id: &str) -> Message {
    Message {
        id: id.into(),
        role: Role::Assistant,
        parts: vec![ContentPart::ToolCall(ToolCallPart {
            id: call_id.into(),
            name: "read_file".into(),
            input: json!({"path": "README.md"}),
        })],
    }
}

fn assistant_text(id: &str, text: &str) -> Message {
    Message {
        id: id.into(),
        role: Role::Assistant,
        parts: vec![ContentPart::Text(TextPart { text: text.into() })],
    }
}

async fn seed(journal: &Arc<InMemoryJournal>, session_id: uuid::Uuid, events: &[AgentDomainEvent]) {
    let encoded: Vec<Vec<u8>> = events
        .iter()
        .map(|e| serde_json::to_vec(e).unwrap())
        .collect();
    journal
        .persist(&AgentActor::persistence_id_for(session_id), &encoded)
        .await
        .unwrap();
}

/// The most recent request body the mock LLM received.
async fn last_request(mock: &MockLlmServer) -> Value {
    let bodies: Vec<Value> = reqwest::get(format!("{}/received", mock.url()))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    bodies.into_iter().next().expect("a captured request")
}

/// Every `tool_use` id in an Anthropic request body with no `tool_result`
/// answering it — exactly what the provider 400s on.
fn unmatched_tool_uses(body: &Value) -> Vec<String> {
    let blocks = || {
        body["messages"]
            .as_array()
            .expect("messages array")
            .iter()
            .filter_map(|m| m["content"].as_array())
            .flatten()
    };
    let answered: Vec<&str> = blocks()
        .filter(|b| b["type"] == "tool_result")
        .filter_map(|b| b["tool_use_id"].as_str())
        .collect();
    blocks()
        .filter(|b| b["type"] == "tool_use")
        .filter_map(|b| b["id"].as_str())
        .filter(|id| !answered.contains(id))
        .map(str::to_string)
        .collect()
}

// ── tests ────────────────────────────────────────────────────────────────────

/// Regression for #54: a Stop mid-turn journals an assistant tool call with no
/// result. Once later turns bury it, a history rebuilt from that journal carried
/// an unanswered `tool_use` into every request, and the provider rejected the
/// session's every turn with a 400 for the rest of its life.
#[tokio::test]
async fn recovered_agent_repairs_a_stopped_mid_history_tool_call() {
    let mock = MockLlmServer::builder().build().await;
    mock.queue_response("here you go");

    let session_id = uuid::Uuid::new_v4();
    let journal = Arc::new(InMemoryJournal::new());
    seed(
        &journal,
        session_id,
        &[
            AgentDomainEvent::InputMessage {
                message: Message::user("u1", "read the readme"),
            },
            // The user pressed Stop here: the tool call is journaled, its result
            // never was.
            AgentDomainEvent::MessageComplete {
                message: assistant_tool_call("a1", "stopped-call"),
            },
            AgentDomainEvent::RunCancelled,
            // Later turns completed on top of it, burying it mid-history.
            AgentDomainEvent::InputMessage {
                message: Message::user("u2", "never mind, just say hi"),
            },
            AgentDomainEvent::MessageComplete {
                message: assistant_text("a2", "hi"),
            },
        ],
    )
    .await;

    let (tx, mut outcomes) = tokio::sync::mpsc::channel(8);
    let ctx = AgentRuntimeContext {
        context_provider: Arc::new(FixedContextProvider {
            provider: provider_at(&mock.url()),
            toolbox: Arc::new(ReadFileToolbox),
        }),
        event_sink: Arc::new(NoopSink),
        parent: Arc::new(OutcomeChannel(tx)),
        session_id,
    };
    let mut params = AgentParams::from_def(&horsie_workflow::AgentRunDef {
        system_prompt: None,
        output_schema: None,
        allow_ask_user: false,
        allow_timers: None,
        max_iterations: None,
        max_retries: None,
        allowed_tools: None,
    });
    // Interactive, like a server session: recovery waits for the user's next
    // message rather than self-continuing.
    params.interactive = true;

    let agent = spawn_root(AgentActor::new(ctx, params), journal.clone());
    agent
        .tell(AgentCommand::Run {
            input: "carry on".into(),
        })
        .await
        .unwrap();

    let outcome = tokio::time::timeout(Duration::from_secs(5), outcomes.recv())
        .await
        .expect("timed out waiting for the turn to finish")
        .expect("outcome channel closed");
    assert!(
        !matches!(outcome, AgentOutcome::Failed { .. }),
        "the turn must not fail: {outcome:?}"
    );

    let body = last_request(&mock).await;
    assert!(
        unmatched_tool_uses(&body).is_empty(),
        "request carried unanswered tool_use ids {:?}: {body}",
        unmatched_tool_uses(&body)
    );
}

/// #61 item 5b: `context_provider.provide()` sat outside the run's cancel
/// `select!`, so the place most likely to hang was the one place `Stop` could not
/// reach.
///
/// `provide()` awaits an MCP connect, a workspace scan and a `SessionStart` hook —
/// three process boundaries. With a stalled peer the run hung, `halt()` gave up
/// after `HALT_CANCEL_TIMEOUT`, and the task leaked for the process lifetime. The
/// user saw a session wedged in `Running` with a Stop button that did nothing.
#[tokio::test]
async fn cancelling_a_run_stuck_in_provide_returns_promptly() {
    /// A provider that never returns — a wedged runtime or a silent MCP server.
    struct HangingContextProvider;
    #[async_trait]
    impl horsie_workflow::ContextProvider for HangingContextProvider {
        async fn provide(&self) -> Result<horsie_workflow::Contexts, String> {
            std::future::pending().await
        }
    }

    tokio::time::timeout(Duration::from_secs(30), async {
        let journal = Arc::new(InMemoryJournal::new());
        let session_id = uuid::Uuid::new_v4();
        let (tx, _outcomes) = tokio::sync::mpsc::channel(8);
        let ctx = AgentRuntimeContext {
            context_provider: Arc::new(HangingContextProvider),
            event_sink: Arc::new(NoopSink),
            parent: Arc::new(OutcomeChannel(tx)),
            session_id,
        };
        let mut params = AgentParams::from_def(&horsie_workflow::AgentRunDef {
            system_prompt: None,
            output_schema: None,
            allow_ask_user: false,
            allow_timers: None,
            max_iterations: None,
            max_retries: None,
            allowed_tools: None,
        });
        params.interactive = true;

        let agent = spawn_root(AgentActor::new(ctx, params), journal);
        agent
            .tell(AgentCommand::Run {
                input: "start something that wedges".into(),
            })
            .await
            .unwrap();
        // Let the run reach `provide()` and block there.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // The ack is the contract `halt()` waits on: it fires when the run is
        // genuinely over, not when the token was merely flipped.
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        agent
            .tell(AgentCommand::Cancel { ack: Some(ack_tx) })
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(5), ack_rx)
            .await
            .expect("Stop must reach a run blocked in provide(); it hung instead")
            .expect("the cancel ack channel must not drop");
    })
    .await
    .expect("test timed out");
}
