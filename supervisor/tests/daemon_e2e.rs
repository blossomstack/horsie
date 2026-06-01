//! End-to-end tests for the supervisor stack: SupervisorActor → JobActor →
//! WorkflowActor → AgentActor, driven by a mock LLM. A test [`JobRuntime`] stands
//! in for the production sandboxed assembly so no `october-runtime` child or nono
//! sandbox is needed; everything else (event sourcing, journaling, the registry,
//! parallelism, resume) is the real code path.
//!
//! Not covered here: auto-resume of a job that was *actively mid-LLM-turn* at crash
//! time. In one test process the interrupted background task survives (a real crash
//! kills it) and the old actor subtree keeps its persistence ids, so that path
//! can't be simulated in-process. Its logic is unit-tested via the actors'
//! `on_recovery_complete` and `sanitize_for_resume` in the workflow crate.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use actor::{ActorRef, FileJournal, Journal, spawn_root};
use agentcore::{AgentEvent, EventSink, EventSinkError, LlmProvider};
use anthropic::AnthropicProvider;
use async_trait::async_trait;
use mock_llm::MockLlmServer;
use models::capabilities::{CapabilitySpec, NetworkPolicy};
use models::daemon::JobStatus;
use models::runtime::{ToolCall, ToolResult};
use models::workflow::{WorkflowAgentDef, WorkflowDefinition};
use runtime_client::{RuntimeClient, RuntimeTransport, TransportError};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use supervisor::{
    JobRuntime, JobShutdown, JobSpec, Kickoff, LaunchParams, LaunchedJob, SupervisorActor,
    SupervisorCommand,
};
use tokio::sync::oneshot;
use workflow::{DefaultToolboxFactory, WorkflowActor, WorkflowCommand, WorkflowRuntimeContext};

// ── test doubles ────────────────────────────────────────────────────────────

/// A runtime transport that is never used (test workflows have no tools).
struct NoopTransport;

#[async_trait]
impl RuntimeTransport for NoopTransport {
    async fn invoke(&self, _call_id: &str, _call: ToolCall) -> Result<ToolResult, TransportError> {
        Err(TransportError::Disconnected)
    }
    async fn cancel(&self, _call_id: &str) -> Result<(), TransportError> {
        Ok(())
    }
}

/// The test runtime spawns no OS resources, so teardown is a no-op.
struct NoopShutdown;

#[async_trait]
impl JobShutdown for NoopShutdown {
    async fn shutdown(&self) {}
}

struct NoopSink;

#[async_trait]
impl EventSink for NoopSink {
    async fn emit(&self, _event: AgentEvent) -> Result<(), EventSinkError> {
        Ok(())
    }
}

/// A [`JobRuntime`] that spawns the real `WorkflowActor` with a mock-backed
/// provider registry and a no-op runtime client — exercising the full actor stack
/// without a sandbox child.
struct TestRuntime {
    registry: HashMap<String, Arc<dyn LlmProvider>>,
    journal: Arc<dyn Journal>,
}

#[async_trait]
impl JobRuntime for TestRuntime {
    async fn launch(&self, params: LaunchParams) -> Result<LaunchedJob, String> {
        let ctx = WorkflowRuntimeContext {
            provider_registry: self.registry.clone(),
            toolbox_factory: Arc::new(DefaultToolboxFactory),
            runtime_client: RuntimeClient::new(NoopTransport),
            event_sink: Arc::new(NoopSink),
            workflow_events: params.events,
        };
        let wf = spawn_root(
            WorkflowActor::new(params.job_id, params.spec.workflow.clone(), ctx),
            self.journal.clone(),
        );
        match params.kickoff {
            Kickoff::Start => wf
                .tell(WorkflowCommand::Start {
                    input: params.spec.input,
                })
                .await
                .map_err(|e| e.to_string())?,
            Kickoff::Resume(message) => wf
                .tell(WorkflowCommand::Resume { message })
                .await
                .map_err(|e| e.to_string())?,
            Kickoff::Recover => {}
        }
        Ok(LaunchedJob {
            workflow: wf,
            shutdown: Arc::new(NoopShutdown),
        })
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn provider_at(url: &str) -> Arc<dyn LlmProvider> {
    Arc::new(
        AnthropicProvider::with_api_key("test-key")
            .unwrap()
            .with_base_url(url)
            .with_retry_delay_secs(0),
    )
}

fn registry_for(url: &str) -> HashMap<String, Arc<dyn LlmProvider>> {
    let mut m = HashMap::new();
    m.insert("m".to_string(), provider_at(url));
    m
}

/// Build a supervisor over `dir` with a fresh journal handle — simulating one
/// daemon "process". Returns the journal (so a later "restart" can reuse the dir)
/// and the supervisor handle.
fn boot(dir: &Path, url: &str) -> (Arc<dyn Journal>, ActorRef<SupervisorCommand>) {
    let journal: Arc<dyn Journal> = Arc::new(FileJournal::new(dir.to_path_buf()));
    let runtime = Arc::new(TestRuntime {
        registry: registry_for(url),
        journal: journal.clone(),
    });
    let sup = spawn_root(SupervisorActor::new(runtime), journal.clone());
    (journal, sup)
}

fn agent(
    name: &str,
    output_schema: Option<serde_json::Value>,
    allow_ask_user: bool,
) -> WorkflowAgentDef {
    WorkflowAgentDef {
        name: name.into(),
        system_prompt: None,
        model: "m".into(),
        output_schema,
        allow_ask_user,
        transitions: None,
        max_iterations: None,
        max_retries: None,
        allowed_tools: Some(vec![]),
    }
}

/// Single-agent, no-tool workflow that ends with the model's plain text.
fn solo_workflow() -> WorkflowDefinition {
    WorkflowDefinition {
        start: "solo".into(),
        agents: vec![agent("solo", None, false)],
    }
}

/// Single-agent workflow that may ask the user (kind-tagged conclude).
fn ask_workflow() -> WorkflowDefinition {
    WorkflowDefinition {
        start: "solo".into(),
        agents: vec![agent(
            "solo",
            Some(serde_json::json!({"type": "object"})),
            true,
        )],
    }
}

fn spec(def: WorkflowDefinition) -> JobSpec {
    JobSpec {
        workflow: def,
        workflow_name: "wf".into(),
        workdir: PathBuf::from("/tmp"),
        input: "go".into(),
        capabilities: CapabilitySpec {
            network: NetworkPolicy::Block,
            grants: vec![],
        },
    }
}

async fn submit(sup: &ActorRef<SupervisorCommand>, job_spec: JobSpec) -> String {
    let (tx, rx) = oneshot::channel();
    sup.tell(SupervisorCommand::Submit {
        spec: job_spec,
        submitted_at: 0,
        reply: tx,
    })
    .await
    .unwrap();
    rx.await.unwrap()
}

async fn list(sup: &ActorRef<SupervisorCommand>) -> Vec<models::daemon::JobSummary> {
    let (tx, rx) = oneshot::channel();
    sup.tell(SupervisorCommand::List { reply: tx })
        .await
        .unwrap();
    rx.await.unwrap()
}

/// Poll `List` until `pred` holds for the named job, or panic after a timeout.
async fn wait_for(
    sup: &ActorRef<SupervisorCommand>,
    job_id: &str,
    pred: impl Fn(&JobStatus) -> bool,
) {
    let mut last = None;
    for _ in 0..200 {
        if let Some(j) = list(sup).await.into_iter().find(|j| j.job_id == job_id) {
            if pred(&j.status) {
                return;
            }
            last = Some(j.status);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("job {job_id} did not reach the expected status in time; last seen: {last:?}");
}

fn is_finished(s: &JobStatus) -> bool {
    matches!(s, JobStatus::Finished)
}

// ── tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn parallel_jobs_run_and_finish() {
    let mock = MockLlmServer::builder()
        .response("done one")
        .response("done two")
        .build()
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_journal, sup) = boot(dir.path(), &mock.url());

    let a = submit(&sup, spec(solo_workflow())).await;
    let b = submit(&sup, spec(solo_workflow())).await;
    assert_ne!(a, b, "each submit gets a distinct job id");

    wait_for(&sup, &a, is_finished).await;
    wait_for(&sup, &b, is_finished).await;

    let jobs = list(&sup).await;
    assert_eq!(jobs.len(), 2);
    assert!(jobs.iter().all(|j| matches!(j.status, JobStatus::Finished)));
}

#[tokio::test]
async fn ask_then_resume_finishes() {
    let mock = MockLlmServer::builder()
        // First turn: the agent asks the user.
        .tool_call(
            "conclude",
            serde_json::json!({"kind": "ask", "question": "which one?"}),
        )
        // After resume: the agent submits a final output (an object, matching the
        // agent's `{type: object}` output schema — a bare string would fail
        // validation and trigger a retry that drains the mock queue).
        .tool_call(
            "conclude",
            serde_json::json!({"kind": "submit", "output": {"answer": "done"}}),
        )
        .build()
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_journal, sup) = boot(dir.path(), &mock.url());

    let id = submit(&sup, spec(ask_workflow())).await;
    wait_for(&sup, &id, |s| matches!(s, JobStatus::AwaitingUserInput)).await;

    sup.tell(SupervisorCommand::Resume {
        job_id: id.clone(),
        message: "the first one".into(),
    })
    .await
    .unwrap();

    wait_for(&sup, &id, is_finished).await;
}

#[tokio::test]
async fn registry_recovers_from_journal_after_restart() {
    let mock = MockLlmServer::builder().response("done").build().await;
    let dir = tempfile::tempdir().unwrap();

    // First "process": submit a job and let it finish.
    let job_id = {
        let (_journal, sup) = boot(dir.path(), &mock.url());
        let id = submit(&sup, spec(solo_workflow())).await;
        wait_for(&sup, &id, is_finished).await;
        id
    };

    // "Restart": a fresh supervisor over the same dir must rebuild its registry by
    // replaying its own journal — the finished job is still listed, with its
    // terminal status. (Terminal jobs are not re-spawned, so no actor-id conflict.)
    let (_journal2, sup2) = boot(dir.path(), &mock.url());
    let jobs = list(&sup2).await;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].job_id, job_id);
    assert!(matches!(jobs[0].status, JobStatus::Finished));
}

// Auto-resume of a job that was *actively mid-LLM-turn* at crash time is not tested
// here: a true crash must kill the actor tree, but in one process the parked tasks
// survive and keep their persistence ids, so a same-process "restart" would have two
// live actors writing one journal. That re-drive logic is unit-tested in the workflow
// crate (`on_recovery_complete` + `sanitize_for_resume`); restart recovery of paused
// jobs is covered below and by `registry_recovers_from_journal_after_restart`.

#[tokio::test]
async fn suspended_job_recovers_and_resumes_after_restart() {
    let mock = MockLlmServer::builder()
        .tool_call(
            "conclude",
            serde_json::json!({"kind": "ask", "question": "which?"}),
        )
        .tool_call(
            "conclude",
            serde_json::json!({"kind": "submit", "output": {"answer": "done"}}),
        )
        .build()
        .await;
    let dir = tempfile::tempdir().unwrap();

    let job_id = {
        let (_journal, sup) = boot(dir.path(), &mock.url());
        let id = submit(&sup, spec(ask_workflow())).await;
        wait_for(&sup, &id, |s| matches!(s, JobStatus::AwaitingUserInput)).await;
        id
    };

    // Restart: the awaiting job is recovered as dormant (still AwaitingUserInput, no
    // sandbox child) until an explicit resume.
    let (_journal2, sup2) = boot(dir.path(), &mock.url());
    let recovered = list(&sup2)
        .await
        .into_iter()
        .find(|j| j.job_id == job_id)
        .expect("job present after restart");
    assert!(matches!(recovered.status, JobStatus::AwaitingUserInput));

    sup2.tell(SupervisorCommand::Resume {
        job_id: job_id.clone(),
        message: "the first".into(),
    })
    .await
    .unwrap();
    wait_for(&sup2, &job_id, is_finished).await;
}

async fn remove(sup: &ActorRef<SupervisorCommand>, job_id: &str) -> Result<(), String> {
    let (tx, rx) = oneshot::channel();
    sup.tell(SupervisorCommand::Remove {
        job_id: job_id.to_string(),
        reply: tx,
    })
    .await
    .unwrap();
    rx.await.unwrap()
}

#[tokio::test]
async fn remove_drops_terminal_job_but_refuses_active() {
    let mock = MockLlmServer::builder()
        .tool_call(
            "conclude",
            serde_json::json!({"kind": "ask", "question": "wait?"}),
        )
        .response("done")
        .build()
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_journal, sup) = boot(dir.path(), &mock.url());

    // An active (awaiting) job cannot be removed.
    let active = submit(&sup, spec(ask_workflow())).await;
    wait_for(&sup, &active, |s| matches!(s, JobStatus::AwaitingUserInput)).await;
    assert!(
        remove(&sup, &active).await.is_err(),
        "an active job must not be removable"
    );

    // A finished job can be removed and drops from the registry.
    let done = submit(&sup, spec(solo_workflow())).await;
    wait_for(&sup, &done, is_finished).await;
    assert!(remove(&sup, &done).await.is_ok());
    assert!(
        list(&sup).await.iter().all(|j| j.job_id != done),
        "removed job should be gone from the registry"
    );

    // Removing an unknown job errors.
    assert!(remove(&sup, "no-such-id").await.is_err());
}

#[tokio::test]
async fn render_history_replays_finished_job_from_journal() {
    let mock = MockLlmServer::builder()
        .response("the answer is 42")
        .build()
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (journal, sup) = boot(dir.path(), &mock.url());

    let id = submit(&sup, spec(solo_workflow())).await;
    wait_for(&sup, &id, is_finished).await;

    // History is replayed purely from durable journals — no live broadcaster — so
    // it works for a job whose actor has already stopped.
    let frames = supervisor::render_history(&journal, &id).await;
    let text: String = frames.into_iter().map(|f| f.text).collect();
    assert!(
        text.contains("workflow started"),
        "history should include the workflow start marker; got: {text}"
    );
    assert!(
        text.contains("finished"),
        "history should include the finish marker; got: {text}"
    );
}
