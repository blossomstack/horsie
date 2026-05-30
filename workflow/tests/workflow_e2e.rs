//! End-to-end workflow tests.
//!
//! Each test spins up the real actor runtime with an in-memory journal, a
//! `mock-llm` HTTP server behind `AnthropicProvider`, and drives a
//! `WorkflowActor` through a scenario. Agents finish by calling the synthesized
//! `conclude` tool; the workflow routes their structured output through
//! expression-based transitions. Status is observed by folding the journal
//! exactly as recovery would.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use actor::{EventSourcedActor, InMemoryJournal, Journal, PersistenceId, spawn_root};
use agentcore::{AgentEvent, ContentPart, EventSink, ToolCallError, ToolSpec, Toolbox};
use anthropic::AnthropicProvider;
use async_trait::async_trait;
use futures_util::StreamExt;
use mock_llm::MockLlmServer;
use models::workflow::{WorkflowAgentDef, WorkflowDefinition, WorkflowTransition};
use runtime_client::{MockTransport, RuntimeClient};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use workflow::{
    AgentActor, AgentDomainEvent, CONCLUDE_TOOL, DefaultToolboxFactory, ToolboxFactory,
    WorkflowActor, WorkflowCommand, WorkflowDomainEvent, WorkflowNotification,
    WorkflowRuntimeContext, WorkflowState, WorkflowStatus, conclude_tool_spec,
};

// ── helpers ────────────────────────────────────────────────────────────────

struct NoopSink;
impl EventSink for NoopSink {
    fn emit(&self, _event: AgentEvent) {}
}

fn provider_at(url: &str) -> Arc<dyn agentcore::LlmProvider> {
    Arc::new(
        AnthropicProvider::with_api_key("test-key")
            .unwrap()
            .with_base_url(url)
            .with_retry_delay_secs(0),
    )
}

fn object_schema() -> Value {
    json!({ "type": "object" })
}

fn agent(name: &str) -> WorkflowAgentDef {
    WorkflowAgentDef {
        name: name.into(),
        system_prompt: None,
        model: "mock".into(),
        output_schema: Some(object_schema()),
        allow_ask_user: false,
        transitions: None,
        max_iterations: None,
        max_retries: None,
        allowed_tools: Some(vec![]),
    }
}

fn runtime_context(
    provider: Arc<dyn agentcore::LlmProvider>,
    factory: Arc<dyn ToolboxFactory>,
) -> (
    WorkflowRuntimeContext,
    tokio::sync::mpsc::Receiver<WorkflowNotification>,
) {
    let mut registry: HashMap<String, Arc<dyn agentcore::LlmProvider>> = HashMap::new();
    registry.insert("mock".into(), provider);
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    (
        WorkflowRuntimeContext {
            provider_registry: registry,
            toolbox_factory: factory,
            runtime_client: RuntimeClient::new(MockTransport::ok("")),
            event_sink: Arc::new(NoopSink),
            workflow_events: tx,
        },
        rx,
    )
}

async fn load_state(journal: &Arc<InMemoryJournal>, id: &str) -> WorkflowState {
    let pid = PersistenceId::new("workflow", id);
    let (mut state, seq) = match journal.latest_snapshot(&pid).await.unwrap() {
        Some((bytes, seq)) => (serde_json::from_slice(&bytes).unwrap(), seq),
        None => (WorkflowActor::initial_state(), 0),
    };
    let mut events = journal.replay(&pid, seq).await;
    while let Some(item) = events.next().await {
        let ev: WorkflowDomainEvent = serde_json::from_slice(&item.unwrap()).unwrap();
        state = WorkflowActor::apply_event(state, ev);
    }
    state
}

async fn wait_for_status(
    journal: &Arc<InMemoryJournal>,
    id: &str,
    target: WorkflowStatus,
) -> WorkflowState {
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let state = load_state(journal, id).await;
            if state.status == target {
                return state;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    match result {
        Ok(state) => state,
        Err(_) => {
            let state = load_state(journal, id).await;
            panic!(
                "timed out waiting for {target:?}; current status {:?}",
                state.status
            );
        }
    }
}

async fn recv_notification(
    rx: &mut tokio::sync::mpsc::Receiver<WorkflowNotification>,
) -> WorkflowNotification {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out waiting for notification")
        .expect("notification channel closed")
}

async fn post_snapshot_events(
    journal: &Arc<InMemoryJournal>,
    id: &str,
) -> Vec<WorkflowDomainEvent> {
    let mut out = Vec::new();
    let mut events = journal.replay(&PersistenceId::new("workflow", id), 0).await;
    while let Some(item) = events.next().await {
        out.push(serde_json::from_slice(&item.unwrap()).unwrap());
    }
    out
}

// ── two-agent workflow with a conditional transition ─────────────────────────

#[tokio::test]
async fn two_agent_workflow_routes_on_condition_and_finishes() {
    // researcher concludes with {score: 90}; the >80 transition routes to writer,
    // which concludes with the final report.
    let mock = MockLlmServer::builder()
        .tool_call(CONCLUDE_TOOL, json!({"score": 90}))
        .tool_call(CONCLUDE_TOOL, json!({"report": "all done"}))
        .build()
        .await;

    let mut researcher = agent("researcher");
    researcher.output_schema = Some(json!({
        "type": "object",
        "properties": { "score": { "type": "number" } }
    }));
    researcher.transitions = Some(vec![WorkflowTransition {
        to: "writer".into(),
        condition: Some("output.score > 80".into()),
    }]);
    let writer = agent("writer");

    let def = WorkflowDefinition {
        start: "researcher".into(),
        agents: vec![researcher, writer],
    };

    let journal = Arc::new(InMemoryJournal::new());
    let (rt, mut events) =
        runtime_context(provider_at(&mock.url()), Arc::new(DefaultToolboxFactory));
    let wf = spawn_root(WorkflowActor::new("wf-seq", def, rt), journal.clone());

    wf.tell(WorkflowCommand::Start {
        input: "research the topic".into(),
    })
    .await
    .unwrap();

    let state = wait_for_status(&journal, "wf-seq", WorkflowStatus::Finished).await;
    assert_eq!(state.current_agent.as_deref(), Some("writer"));

    // The push channel delivered a terminal Finished notification carrying the output.
    let n = recv_notification(&mut events).await;
    assert!(
        matches!(n, WorkflowNotification::Finished { output } if output["report"] == "all done"),
        "expected Finished notification with the writer output"
    );

    let events = post_snapshot_events(&journal, "wf-seq").await;
    assert!(events.iter().any(|e| matches!(
        e,
        WorkflowDomainEvent::WorkflowFinished { output } if output["report"] == "all done"
    )));
}

// ── ask / reply cycle (kind-tagged conclude) ─────────────────────────────────

#[tokio::test]
async fn ask_user_pauses_then_resume_injects_reply() {
    // The agent first asks (kind=ask), pausing the workflow; after the reply it
    // submits its output (kind=submit) and finishes.
    let mock = MockLlmServer::builder()
        .tool_call(
            CONCLUDE_TOOL,
            json!({"kind": "ask", "question": "what colour?"}),
        )
        .tool_call(
            CONCLUDE_TOOL,
            json!({"kind": "submit", "output": {"colour": "blue"}}),
        )
        .build()
        .await;

    let mut asker = agent("asker");
    asker.allow_ask_user = true;
    asker.output_schema = Some(json!({
        "type": "object",
        "properties": { "colour": { "type": "string" } }
    }));

    let def = WorkflowDefinition {
        start: "asker".into(),
        agents: vec![asker],
    };

    let journal = Arc::new(InMemoryJournal::new());
    let (rt, mut events) =
        runtime_context(provider_at(&mock.url()), Arc::new(DefaultToolboxFactory));
    let wf = spawn_root(WorkflowActor::new("wf-ask", def, rt), journal.clone());

    wf.tell(WorkflowCommand::Start {
        input: "pick a colour".into(),
    })
    .await
    .unwrap();

    wait_for_status(&journal, "wf-ask", WorkflowStatus::AwaitingUserInput).await;

    // The await transition pushed the question text on the channel.
    let n = recv_notification(&mut events).await;
    assert!(
        matches!(n, WorkflowNotification::AwaitingUserInput { question } if question == "what colour?"),
        "expected AwaitingUserInput carrying the question"
    );

    wf.tell(WorkflowCommand::Resume {
        message: "blue".into(),
    })
    .await
    .unwrap();

    let state = wait_for_status(&journal, "wf-ask", WorkflowStatus::Finished).await;
    assert_eq!(state.status, WorkflowStatus::Finished);
    let n = recv_notification(&mut events).await;
    assert!(matches!(n, WorkflowNotification::Finished { .. }));
}

// ── cancel / resume ──────────────────────────────────────────────────────────

/// A toolbox with a blocking `wait` tool plus the agent's `conclude` tool, so a
/// `Cancel` can land between agent iterations deterministically.
struct BlockingToolbox {
    conclude: ToolSpec,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl Toolbox for BlockingToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: "wait".into(),
                description: "blocks until released".into(),
                input_schema: json!({"type": "object"}),
            },
            self.conclude.clone(),
        ]
    }

    async fn execute(&self, name: &str, _input: Value) -> Result<Value, ToolCallError> {
        if name == "wait" {
            self.entered.notify_one();
            self.release.notified().await;
            return Ok(json!("released"));
        }
        Err(ToolCallError::ExecutionFailed(format!(
            "unexpected tool {name}"
        )))
    }
}

struct BlockingFactory {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl ToolboxFactory for BlockingFactory {
    fn for_agent(&self, def: &WorkflowAgentDef, _client: RuntimeClient) -> Arc<dyn Toolbox> {
        let conclude = conclude_tool_spec(def.output_schema.as_ref(), def.allow_ask_user)
            .expect("worker has an output schema");
        Arc::new(BlockingToolbox {
            conclude,
            entered: self.entered.clone(),
            release: self.release.clone(),
        })
    }
}

#[tokio::test]
async fn cancel_suspends_then_resume_continues() {
    let mock = MockLlmServer::builder()
        .tool_call("wait", json!({}))
        .tool_call(CONCLUDE_TOOL, json!({"done": true}))
        .build()
        .await;

    let mut worker = agent("worker");
    worker.allowed_tools = Some(vec!["wait".into()]);

    let def = WorkflowDefinition {
        start: "worker".into(),
        agents: vec![worker],
    };

    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let factory = Arc::new(BlockingFactory {
        entered: entered.clone(),
        release: release.clone(),
    });

    let journal = Arc::new(InMemoryJournal::new());
    let (rt, _events) = runtime_context(provider_at(&mock.url()), factory);
    let wf = spawn_root(WorkflowActor::new("wf-cancel", def, rt), journal.clone());

    wf.tell(WorkflowCommand::Start {
        input: "do work".into(),
    })
    .await
    .unwrap();

    // Wait until the agent is blocked inside the tool, then cancel.
    entered.notified().await;
    wf.tell(WorkflowCommand::Cancel).await.unwrap();
    wait_for_status(&journal, "wf-cancel", WorkflowStatus::Suspended).await;

    // Release the tool; the agent observes cancellation on its next iteration.
    release.notify_one();

    wf.tell(WorkflowCommand::Resume {
        message: "carry on".into(),
    })
    .await
    .unwrap();

    let state = wait_for_status(&journal, "wf-cancel", WorkflowStatus::Finished).await;
    assert_eq!(state.status, WorkflowStatus::Finished);
}

// ── crash recovery: journaled agent events rebuild the conversation ──────────

#[tokio::test]
async fn agent_session_history_reconstructs_from_journal() {
    let mock = MockLlmServer::builder()
        .tool_call(CONCLUDE_TOOL, json!({"answer": "42"}))
        .build()
        .await;

    let mut solo = agent("solo");
    solo.output_schema = Some(json!({
        "type": "object",
        "properties": { "answer": { "type": "string" } }
    }));

    let def = WorkflowDefinition {
        start: "solo".into(),
        agents: vec![solo],
    };

    let journal = Arc::new(InMemoryJournal::new());
    let (rt, _events) = runtime_context(provider_at(&mock.url()), Arc::new(DefaultToolboxFactory));
    let wf = spawn_root(WorkflowActor::new("wf-recover", def, rt), journal.clone());

    wf.tell(WorkflowCommand::Start {
        input: "what is the answer?".into(),
    })
    .await
    .unwrap();

    let state = wait_for_status(&journal, "wf-recover", WorkflowStatus::Finished).await;
    assert_eq!(state.current_agent.as_deref(), Some("solo"));
    let session_id = state.current_session_id.unwrap();

    // The workflow's final output is the agent's structured output.
    let events = post_snapshot_events(&journal, "wf-recover").await;
    assert!(events.iter().any(|e| matches!(
        e,
        WorkflowDomainEvent::WorkflowFinished { output } if output["answer"] == "42"
    )));

    // Fold the agent session's journal the way recovery would.
    // The agent journals under a run-scoped id: `<run_id>/sessions/<session_id>`.
    let history = reconstruct_agent_history(&journal, &session_id.to_string()).await;
    assert!(history.len() >= 2, "expected user + assistant messages");
    assert_eq!(history.first().unwrap().role, agentcore::Role::User);
    // The assistant's terminal turn called the conclude tool.
    let concluded = history.iter().any(|m| {
        m.parts
            .iter()
            .any(|p| matches!(p, ContentPart::ToolCall(tc) if tc.name == CONCLUDE_TOOL))
    });
    assert!(concluded, "expected a conclude tool call in the history");
}

async fn reconstruct_agent_history(
    journal: &Arc<InMemoryJournal>,
    session_id: &str,
) -> Vec<agentcore::Message> {
    let pid = PersistenceId::new("agent", session_id);
    let (mut state, seq) = match journal.latest_snapshot(&pid).await.unwrap() {
        Some((bytes, seq)) => (serde_json::from_slice(&bytes).unwrap(), seq),
        None => (AgentActor::initial_state(), 0),
    };
    let mut events = journal.replay(&pid, seq).await;
    while let Some(item) = events.next().await {
        let ev: AgentDomainEvent = serde_json::from_slice(&item.unwrap()).unwrap();
        state = AgentActor::apply_event(state, ev);
    }
    state.messages
}
