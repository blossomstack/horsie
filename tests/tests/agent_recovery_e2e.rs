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

/// A second `Run` taken while a run is already in flight must be refused, not
/// started. `start_run` overwrites `self.running` with a fresh cancel token, so
/// accepting one orphans the first run's token and leaves two background loops
/// persisting interleaved events into a single `agent/<id>` journal.
///
/// This is the last line of defence: the server no longer reaches it (a session
/// conflicts a mid-turn message at the `SessionActor`), but `WorkflowActor` also
/// issues these commands, and nothing in the type system prevents a third caller.
#[tokio::test]
async fn a_second_run_while_one_is_in_flight_is_refused() {
    let mock = MockLlmServer::builder().build().await;
    // Turn 1 hangs inside the provider until the test releases it, so the second
    // command lands with `self.running` still `Some`.
    let block = mock.blocking_response("turn one");

    let session_id = uuid::Uuid::new_v4();
    let journal = Arc::new(InMemoryJournal::new());
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
    params.interactive = true;

    let agent = spawn_root(AgentActor::new(ctx, params), journal.clone());
    agent
        .tell(AgentCommand::Run {
            input: "first".into(),
        })
        .await
        .unwrap();
    block.wait_until_received().await;

    // The racing second command, delivered while turn 1 is provably in flight.
    agent
        .tell(AgentCommand::Run {
            input: "second".into(),
        })
        .await
        .unwrap();

    block.release();
    tokio::time::timeout(Duration::from_secs(5), outcomes.recv())
        .await
        .expect("timed out waiting for turn one")
        .expect("outcome channel closed");

    // The durable record is the assertion that matters: a refused command
    // persists nothing, so "second" must never have entered the history. If it
    // did, a second run took it — and the two runs share this journal.
    let inputs = input_messages(&journal, session_id).await;
    assert_eq!(
        inputs,
        vec!["first".to_string()],
        "the refused command must not reach the journal"
    );
}

/// The text of every `InputMessage` in an agent's journal, in order.
async fn input_messages(journal: &Arc<InMemoryJournal>, session_id: uuid::Uuid) -> Vec<String> {
    use futures_util::StreamExt;
    let pid = AgentActor::persistence_id_for(session_id);
    let mut out = Vec::new();
    let mut stream = journal.replay(&pid, 0).await;
    while let Some(Ok(bytes)) = stream.next().await {
        if let Ok(AgentDomainEvent::InputMessage { message }) =
            serde_json::from_slice::<AgentDomainEvent>(&bytes)
        {
            for part in &message.parts {
                if let ContentPart::Text(t) = part {
                    out.push(t.text.clone());
                }
            }
        }
    }
    out
}
