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
    ContentPart, LlmProvider, Message, Role, ToolCallError, ToolCallPart, ToolSpec, Toolbox,
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

    async fn execute(
        &self,
        name: &str,
        _input: Value,
        _tool_call_id: &str,
    ) -> Result<Value, ToolCallError> {
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
        created_at_ms: 0,
        started_at_ms: None,
        id: id.into(),
        role: Role::Assistant,
        parts: vec![ContentPart::ToolCall(ToolCallPart {
            id: call_id.into(),
            name: "read_file".into(),
            input: json!({"path": "README.md"}),
        })],
    }
}

/// The shape a session parked on a question leaves behind: `ask_user` is a
/// handoff tool, so the run ends on the call and it is never executed.
fn assistant_ask(id: &str, call_id: &str) -> Message {
    Message {
        created_at_ms: 0,
        started_at_ms: None,
        id: id.into(),
        role: Role::Assistant,
        parts: vec![ContentPart::ToolCall(ToolCallPart {
            id: call_id.into(),
            name: "ask_user".into(),
            input: json!({"question": "which commands?"}),
        })],
    }
}

fn assistant_text(id: &str, text: &str) -> Message {
    Message {
        created_at_ms: 0,
        started_at_ms: None,
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

/// Every `tool_result` in an Anthropic request body answering `tool_use_id`,
/// as its rendered text. More than one is the duplicate shape providers reject.
fn tool_results_for(body: &Value, tool_use_id: &str) -> Vec<String> {
    body["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .filter(|b| b["type"] == "tool_result" && b["tool_use_id"] == tool_use_id)
        .map(|b| b["content"].to_string())
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
                message: Message::user("u1", "read the readme", 0),
            },
            // The user pressed Stop here: the tool call is journaled, its result
            // never was.
            AgentDomainEvent::MessageComplete {
                message: assistant_tool_call("a1", "stopped-call"),
            },
            AgentDomainEvent::RunCancelled { at_ms: 0 },
            // Later turns completed on top of it, burying it mid-history.
            AgentDomainEvent::InputMessage {
                message: Message::user("u2", "never mind, just say hi", 0),
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
        .tell(AgentCommand::Resume {
            results: Vec::new(),
            message: Some("carry on".into()),
            subagent_results: Vec::new(),
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

/// Regression: a session parked on `ask_user` is idle, so idle offload unloads
/// it (180s) and the next message reloads it. Recovery treated the parked call
/// as wreckage from a dead process and journaled a synthetic "interrupted"
/// result for it; the user's answer was then appended to a `tool_use_id` that
/// already had one, and the provider 400d on the duplicate for the rest of the
/// session's life.
///
/// This seeds exactly what an offloaded parked session leaves behind, reloads a
/// real `AgentActor` on it, and answers the ask.
#[tokio::test]
async fn a_reloaded_agent_parked_on_an_ask_answers_it_exactly_once() {
    /// Advertises `ask_user`, like the server's `AskUserToolbox`. Executing it
    /// is a test failure: a handoff tool is never run.
    struct AskUserToolbox;
    #[async_trait]
    impl Toolbox for AskUserToolbox {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "ask_user".into(),
                description: "ask the user".into(),
                input_schema: json!({"type": "object"}),
            }]
        }
        async fn execute(
            &self,
            name: &str,
            _input: Value,
            _tool_call_id: &str,
        ) -> Result<Value, ToolCallError> {
            Err(ToolCallError::ExecutionFailed(format!(
                "unexpected tool call: {name}"
            )))
        }
    }

    let mock = MockLlmServer::builder().build().await;
    mock.queue_response("removing validate, daemon and job");
    mock.queue_response("done");

    let session_id = uuid::Uuid::new_v4();
    let journal = Arc::new(InMemoryJournal::new());
    seed(
        &journal,
        session_id,
        &[
            AgentDomainEvent::InputMessage {
                message: Message::user("u1", "remove some commands", 0),
            },
            AgentDomainEvent::MessageComplete {
                message: assistant_ask("a1", "ask-1"),
            },
        ],
    )
    .await;

    let (tx, mut outcomes) = tokio::sync::mpsc::channel(8);
    let ctx = AgentRuntimeContext {
        context_provider: Arc::new(FixedContextProvider {
            provider: provider_at(&mock.url()),
            toolbox: Arc::new(AskUserToolbox),
        }),
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
    // What makes the call a park rather than an interruption, exactly as
    // `SessionActor` configures its agent.
    params.optional_handoff_tool = Some("ask_user".into());

    let agent = spawn_root(AgentActor::new(ctx, params), journal.clone());
    agent
        .tell(AgentCommand::Resume {
            results: vec![horsie_models::agent::ToolResultInput {
                tool_call_id: "ask-1".into(),
                output: "validate, daemon, job".into(),
                is_error: false,
            }],
            message: None,
            subagent_results: Vec::new(),
        })
        .await
        .unwrap();

    let outcome = tokio::time::timeout(Duration::from_secs(5), outcomes.recv())
        .await
        .expect("timed out waiting for the answered turn")
        .expect("outcome channel closed");
    assert!(
        !matches!(outcome, AgentOutcome::Failed { .. }),
        "answering a parked ask must not fail: {outcome:?}"
    );

    // Take another turn: any synthetic result recovery journaled is in the
    // history by now, so this is what every later turn would carry forever.
    agent
        .tell(AgentCommand::Resume {
            results: Vec::new(),
            message: Some("carry on".into()),
            subagent_results: Vec::new(),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), outcomes.recv())
        .await
        .expect("timed out waiting for the follow-up turn")
        .expect("outcome channel closed");

    let body = last_request(&mock).await;
    let results = tool_results_for(&body, "ask-1");
    assert_eq!(
        results.len(),
        1,
        "the parked ask must carry exactly one result — the user's answer. Got {results:?} in {body}"
    );
    assert!(
        results[0].contains("validate, daemon, job"),
        "the surviving result must be the real answer, not a synthetic repair: {results:?}"
    );
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
        async fn provide(
            &self,
        ) -> Result<horsie_workflow::Contexts, horsie_workflow::ContextError> {
            std::future::pending().await
        }
    }

    tokio::time::timeout(Duration::from_secs(30), async {
        let journal = Arc::new(InMemoryJournal::new());
        let session_id = uuid::Uuid::new_v4();
        let (tx, _outcomes) = tokio::sync::mpsc::channel(8);
        let ctx = AgentRuntimeContext {
            context_provider: Arc::new(HangingContextProvider),
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
            .tell(AgentCommand::Resume {
                results: Vec::new(),
                message: Some("start something that wedges".into()),
                subagent_results: Vec::new(),
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

/// The repair a crash makes necessary is journaled once, at recovery, rather
/// than recomputed on a clone at the top of every turn — so the journal itself
/// records what the model was actually shown.
#[tokio::test]
async fn recovery_journals_the_repair_for_a_tool_call_the_crash_interrupted() {
    let mock = MockLlmServer::builder().build().await;

    let session_id = uuid::Uuid::new_v4();
    let journal = Arc::new(InMemoryJournal::new());
    seed(
        &journal,
        session_id,
        &[
            AgentDomainEvent::InputMessage {
                message: Message::user("u1", "read the readme", 0),
            },
            // The process died here: the call is journaled, its result is not.
            AgentDomainEvent::MessageComplete {
                message: assistant_tool_call("a1", "interrupted-call"),
            },
        ],
    )
    .await;

    let (tx, _outcomes) = tokio::sync::mpsc::channel(8);
    let ctx = AgentRuntimeContext {
        context_provider: Arc::new(FixedContextProvider {
            provider: provider_at(&mock.url()),
            toolbox: Arc::new(ReadFileToolbox),
        }),
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

    // Recovering alone must repair it — no turn is taken here at all.
    let agent = spawn_root(AgentActor::new(ctx, params), journal.clone());
    let page = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (reply, rx) = tokio::sync::oneshot::channel();
            agent
                .tell(AgentCommand::PageLog {
                    before: None,
                    max: 100,
                    reply,
                })
                .await
                .unwrap();
            let page = rx.await.unwrap();
            if page.messages().any(|m| m.role == Role::Tool) {
                return page;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("recovery must journal a result for the interrupted call");

    let results: Vec<&str> = page
        .messages()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            ContentPart::ToolResult(r) => Some(r.tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(results, vec!["interrupted-call"], "{:?}", page.entries);

    // Durable, not merely in memory: a second incarnation sees the same repair
    // and has nothing left to fix.
    let (tx2, _o2) = tokio::sync::mpsc::channel(8);
    let ctx2 = AgentRuntimeContext {
        context_provider: Arc::new(FixedContextProvider {
            provider: provider_at(&mock.url()),
            toolbox: Arc::new(ReadFileToolbox),
        }),
        parent: Arc::new(OutcomeChannel(tx2)),
        session_id,
    };
    let mut params2 = AgentParams::from_def(&horsie_workflow::AgentRunDef {
        system_prompt: None,
        output_schema: None,
        allow_ask_user: false,
        allow_timers: None,
        max_iterations: None,
        max_retries: None,
        allowed_tools: None,
    });
    params2.interactive = true;
    let agent2 = spawn_root(AgentActor::new(ctx2, params2), journal);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (reply, rx) = tokio::sync::oneshot::channel();
    agent2
        .tell(AgentCommand::PageLog {
            before: None,
            max: 100,
            reply,
        })
        .await
        .unwrap();
    let page2 = rx.await.unwrap();
    let tool_msgs = page2.messages().filter(|m| m.role == Role::Tool).count();
    assert_eq!(
        tool_msgs, 1,
        "the repair is recorded once, not re-applied on every load: {:?}",
        page2.entries
    );
}
