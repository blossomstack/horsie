//! The shared harness every component's tests are built on.
//!
//! Split out because the tests do not partition the way the code does. They
//! are organised around *scenario setup* — does this test need a live session,
//! a journal, a prompt recorder, a run mid-step — rather than around the unit
//! under test, so the stop-hook tests and the plugin-agent tests share
//! `agent_harness`, and the expansion tests build on
//! `spawn_session_with_provider`.
//!
//! Keeping the harness here is what lets a test live beside the component it
//! covers instead of beside the fixture it happens to reuse. Anything used by
//! more than one test belongs in this file; anything used by exactly one
//! belongs next to it.

#![allow(dead_code)]

use super::context::AgentRuntimeBinding;
use super::{ReadCommand, SubAgentCommand, TurnCommand};
use super::{
    context::{SessionAgentKind, SessionContextProvider},
    *,
};
use crate::agent_loop::{ContextProvider, StartTurn};
use crate::sessions::spec::RuntimeId;
use crate::sessions::spec::SessionSpec;
use crate::sessions::spec::{AgentSettings, AgentSource};
use crate::sessions::supervisor::SupervisorConfig;
use horsie_agentcore::LlmProvider;
use horsie_models::hooks::{HookAction, HookRecord, StopOutcome};
use std::sync::PoisonError;

pub(super) fn fold(events: Vec<SessionDomainEvent>) -> SessionState {
    events
        .into_iter()
        .fold(SessionState::default(), SessionActor::apply_event)
}

/// What this actor's orchestrator decides for a state. `drain` used to be a
/// method here; the decision moved to the orchestrator and the actor only
/// performs it, so these tests assert on the decision.
pub(super) fn decisions(actor: &SessionActor, state: &SessionState) -> Vec<AgentAction> {
    actor.next_actions(state)
}

pub(super) fn agent_settings_fixture() -> AgentSettings {
    AgentSettings {
        source: AgentSource::AdHoc,
        model: "mock".into(),
        instructions: None,
        allowed_tools: None,
        use_plugins: None,
        max_iterations: None,
        max_retries: 0,
        mcp_servers: vec![],
        memory_spaces: vec![],
        thinking_effort: None,
        max_concurrent_subagents: None,
        auto_compact: None,
        plugins: Vec::new(),
    }
}

pub(super) fn actor_spec_fixture() -> SessionSpec {
    use crate::sessions::spec::{SessionKind, WorkspaceDef};
    SessionSpec {
        kind: SessionKind::Agent {
            settings: Box::new(agent_settings_fixture()),
        },
        runtime: Some(crate::sessions::spec::RuntimeEnv {
            vendor: "mock".into(),
            workspaces: vec![WorkspaceDef {
                name: "main".into(),
            }],
            provision: vec![],
            env_vars: vec![],
            environment: None,
        }),
        plugins: vec![],
        origin: crate::sessions::spec::SessionOrigin::User,
    }
}

/// The same session, but asking for no runtime at all.
pub(super) fn runtime_less_spec_fixture() -> SessionSpec {
    SessionSpec {
        runtime: None,
        ..actor_spec_fixture()
    }
}

/// One account's deployment, on a fake runtime vendor.
///
/// Whole rather than a bag of dependencies, because a session is built by a
/// shard recipe now: it is handed an id and resolves everything else from its
/// account. So a test that wants a session has to have somewhere for one to
/// come from, and the cheapest honest "somewhere" is a real registry with the
/// wiring handed in.
pub(crate) struct ActorFixture {
    /// The wiring every session here runs on — the same value the account's
    /// bundle was built with, kept to hand for the tests that drive it
    /// directly.
    pub(super) deps: ServerDeps,
    pub(super) agent: crate::runtime_vendor::fake::FakeRuntimeVendor,
    pub(super) node: crate::testing::Deployment,
}

impl ActorFixture {
    /// Every actor's log, for a test that reads what was persisted.
    pub(super) fn journal(&self) -> Arc<dyn horsie_actor::Journal> {
        self.node.journal.clone()
    }

    /// Bring a session into being by telling it what it is.
    ///
    /// Exactly what the supervisor does on `Create`, and in one command for the
    /// same reason: a session with no log yet cannot know its own spec, so
    /// everything the create does has to travel with it rather than behind it.
    pub(super) async fn start(&self, id: Uuid, spec: SessionSpec) -> SessionRef {
        let session = self.node.session(id);
        let _ = session
            .tell(SessionCommand::Core(CoreCommand::Create {
                spec: Box::new(spec),
                name: None,
                message: None,
            }))
            .await;
        session
    }

    /// Where this account's session list stands right now.
    ///
    /// What a supervisor stand-in used to be watched for. A session reporting
    /// its status is what moves this, so "did it report?" is "did this move?".
    /// The counter replaced a broadcast of every status in order — nothing here
    /// needed the order, only that something arrived.
    pub(super) async fn list_revision(&self) -> crate::sessions::Revision {
        *self.node.services().await.revisions.list().borrow()
    }
}

pub(super) async fn actor_fixture() -> ActorFixture {
    actor_fixture_from(crate::runtime_vendor::fake::FakeRuntimeVendor::builder(
        "mock",
    ))
    .await
}

/// The same fixture over a fake told to hold its creates, so a test can put
/// a message underneath one that is genuinely in flight.
pub(super) async fn actor_fixture_blocking_creates() -> ActorFixture {
    actor_fixture_from(
        crate::runtime_vendor::fake::FakeRuntimeVendor::builder("mock").block_creates(),
    )
    .await
}

pub(super) async fn actor_fixture_from(
    builder: crate::runtime_vendor::fake::FakeRuntimeVendorBuilder,
) -> ActorFixture {
    let agent = builder.serve_in_process().await.expect("fake agent");
    fixture_over(agent, None).await
}

/// The wiring a test deployment runs on: one fake vendor under `mock`, an
/// empty provider registry the test fills in, and `plugins` as its library.
pub(crate) fn fake_deps(
    agent: &crate::runtime_vendor::fake::FakeRuntimeVendor,
    plugins: Option<Arc<dyn crate::plugins::PluginProvisioner>>,
) -> ServerDeps {
    let mut vendors = HashMap::new();
    vendors.insert(
        "mock".to_string(),
        agent.link() as std::sync::Arc<dyn crate::runtime_vendor::RuntimeVendor>,
    );
    let vendors = Arc::new(std::sync::RwLock::new(vendors));
    ServerDeps {
        artifacts: None,
        project: crate::projects::ProjectId::new("p-test"),
        runtimes: crate::runtime_manager::test_runtime_manager(&vendors),
        provider_registry: Arc::new(std::sync::RwLock::new(HashMap::new())),
        vendors,
        github_tokens: None,
        mcp: None,
        plugins,
        memory: None,
    }
}

/// A deployment over `agent`, with `plugins` as its plugin library.
pub(super) async fn fixture_over(
    agent: crate::runtime_vendor::fake::FakeRuntimeVendor,
    plugins: Option<Arc<dyn crate::plugins::PluginProvisioner>>,
) -> ActorFixture {
    fixture_on(
        Arc::new(horsie_actor::InMemoryJournal::new()),
        agent,
        plugins,
    )
    .await
}

/// The same, over a journal the test is watching.
pub(super) async fn fixture_on(
    journal: Arc<dyn horsie_actor::Journal>,
    agent: crate::runtime_vendor::fake::FakeRuntimeVendor,
    plugins: Option<Arc<dyn crate::plugins::PluginProvisioner>>,
) -> ActorFixture {
    let deps = fake_deps(&agent, plugins);
    // No background ticker: a session here is driven directly, and a sweep
    // nobody asked for would be a race in every test at once.
    let node = crate::testing::Deployment::on(
        journal,
        deps.clone(),
        SupervisorConfig {
            tick_interval: None,
            ..SupervisorConfig::default()
        },
    )
    .await;
    ActorFixture { deps, agent, node }
}

/// Every status a session published about itself, in order.
/// Poll until the session has reported anything at all (2s cap).
/// Wait for the session list to move past `from`.
///
/// Returns whether it did, rather than what it became: the caller is asking
/// whether the session said anything at all.
pub(super) async fn wait_for_report(
    fixture: &ActorFixture,
    from: crate::sessions::Revision,
) -> bool {
    for _ in 0..200 {
        if fixture.list_revision().await != from {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    false
}

pub(super) fn answer(id: &str, text: &str) -> AskAnswer {
    AskAnswer {
        tool_call_id: id.to_string(),
        text: text.to_string(),
    }
}

/// An `LlmProvider` that hangs until released, so a test can hold a run
/// genuinely `Running` for as long as it needs to.
pub(super) struct BlockingProvider {
    pub(super) gate: tokio::sync::Notify,
}

impl BlockingProvider {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            gate: tokio::sync::Notify::new(),
        })
    }

    pub(super) fn release(&self) {
        self.gate.notify_one();
    }
}

#[async_trait]
impl LlmProvider for BlockingProvider {
    fn model_id(&self) -> &str {
        "mock"
    }

    async fn complete(
        &self,
        _request: horsie_agentcore::CompletionRequest<'_>,
        _message_id: &str,
        _events: &dyn horsie_agentcore::EventSink,
    ) -> Result<horsie_agentcore::CompletionResponse, horsie_agentcore::LlmError> {
        self.gate.notified().await;
        Ok(horsie_agentcore::CompletionResponse {
            parts: vec![horsie_agentcore::ContentPart::Text(
                horsie_agentcore::TextPart {
                    text: "done".to_string(),
                },
            )],
            stop_reason: horsie_agentcore::StopReason::EndTurn,
            usage: horsie_agentcore::Usage::without_cache(1, 1),
        })
    }
}

/// A provider whose every call immediately ends the turn with plain text.
/// An [`EchoProvider`] that keeps the system prompt of every turn it answered.
///
/// The only way to assert on a prompt an agent was *actually run with*: the
/// composition happens inside the context provider the session builds for
/// itself, so a test that builds its own provider proves nothing about what the
/// session hands its agents.
#[derive(Default)]
pub(super) struct PromptRecordingProvider {
    prompts: std::sync::Mutex<Vec<String>>,
}

impl PromptRecordingProvider {
    /// Every system prompt seen so far, oldest first.
    pub(super) fn prompts(&self) -> Vec<String> {
        self.prompts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl LlmProvider for PromptRecordingProvider {
    fn model_id(&self) -> &str {
        "mock"
    }

    async fn complete(
        &self,
        request: horsie_agentcore::CompletionRequest<'_>,
        message_id: &str,
        events: &dyn horsie_agentcore::EventSink,
    ) -> Result<horsie_agentcore::CompletionResponse, horsie_agentcore::LlmError> {
        self.prompts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.system.clone().unwrap_or_default());
        EchoProvider.complete(request, message_id, events).await
    }
}

pub(super) struct EchoProvider;

#[async_trait]
impl LlmProvider for EchoProvider {
    fn model_id(&self) -> &str {
        "mock"
    }

    async fn complete(
        &self,
        _request: horsie_agentcore::CompletionRequest<'_>,
        _message_id: &str,
        _events: &dyn horsie_agentcore::EventSink,
    ) -> Result<horsie_agentcore::CompletionResponse, horsie_agentcore::LlmError> {
        Ok(horsie_agentcore::CompletionResponse {
            parts: vec![horsie_agentcore::ContentPart::Text(
                horsie_agentcore::TextPart {
                    text: "sub answer".to_string(),
                },
            )],
            stop_reason: horsie_agentcore::StopReason::EndTurn,
            usage: horsie_agentcore::Usage::without_cache(1, 1),
        })
    }
}

/// A live session on a fake runtime, with `provider` behind its model.
///
/// Returns only once the create the session started has landed. A session
/// answers commands the moment it exists, so without this the fixture hands
/// back a session whose own `Provision` is still in flight — and a test that
/// restarts the deployment then plants a history on a log that says
/// `InFlight`, which `RuntimeLifecycle::on_load` re-attempts and whose finish
/// flushes the turn boundary. That is a real boundary, so the seeded test then
/// fails for a reason it never set up.
pub(super) async fn spawn_session_with_provider(
    provider: Arc<dyn LlmProvider>,
) -> (
    ActorFixture,
    SessionRef,
    Uuid,
    Arc<dyn horsie_actor::Journal>,
) {
    let f = actor_fixture().await;
    let id = Uuid::new_v4();
    f.deps
        .runtimes
        .create(
            crate::runtime_manager::RuntimeAddress {
                session: &id.to_string(),
                runtime: &id.to_string(),
                incarnation: "i1",
            },
            "mock",
            &actor_spec_fixture()
                .runtime_env()
                .expect("the fixture has a runtime"),
        )
        .await
        .expect("create");
    f.deps.provider_registry.write().unwrap().insert(
        "mock".to_string(),
        crate::sessions::spec::ModelEntry::provider_only(provider),
    );
    let journal = f.journal();
    let session = f.start(id, actor_spec_fixture()).await;
    wait_for_state(&journal, id, "the session's create lands", |s| {
        matches!(
            s.root_runtime().map(|r| &r.provisioning),
            Some(ProvisioningState::Ready { .. })
        )
    })
    .await;
    (f, session, id, journal)
}

/// A two-step run: `triage` branches on its output to `fix` or `file`.
pub(super) fn run_spec_fixture(input: &str) -> crate::sessions::workflow::WorkflowRunSpec {
    use crate::sessions::workflow::{TransitionSpec, WorkflowRunSpec, WorkflowStepSpec};
    let settings = || agent_settings_fixture();
    WorkflowRunSpec {
        workflow: "fix-bug".into(),
        start: "triage".into(),
        steps: vec![
            WorkflowStepSpec {
                name: "triage".into(),
                agent: "triager".into(),
                prompt: "Triage it.".into(),
                // Triage reports a severity, and the graph routes on it.
                outcomes: vec![
                    horsie_models::workflow::StepOutcome {
                        value: "p0".into(),
                        description: "drop everything".into(),
                    },
                    horsie_models::workflow::StepOutcome {
                        value: "p2".into(),
                        description: "file it".into(),
                    },
                ],
                fields: Vec::new(),
                // The fixture's steps may ask: one test parks a step on a
                // question, and a step that is not interactive has no
                // `ask_user` tool to call at all.
                interactive: true,
                transitions: vec![
                    TransitionSpec {
                        to: "fix".into(),
                        when: Some(horsie_models::workflow::OutcomeFilter::In(
                            horsie_models::workflow::OutcomeIn {
                                values: vec!["p0".into()],
                            },
                        )),
                    },
                    TransitionSpec {
                        to: "file".into(),
                        when: None,
                    },
                ],
                settings: settings(),
            },
            WorkflowStepSpec {
                name: "fix".into(),
                agent: "coder".into(),
                prompt: "Fix it.".into(),
                outcomes: crate::sessions::workflow::default_outcomes(),
                fields: Vec::new(),
                // The fixture's steps may ask: one test parks a step on a
                // question, and a step that is not interactive has no
                // `ask_user` tool to call at all.
                interactive: true,
                transitions: vec![],
                settings: settings(),
            },
            WorkflowStepSpec {
                name: "file".into(),
                agent: "writer".into(),
                prompt: "File it.".into(),
                outcomes: crate::sessions::workflow::default_outcomes(),
                fields: Vec::new(),
                // The fixture's steps may ask: one test parks a step on a
                // question, and a step that is not interactive has no
                // `ask_user` tool to call at all.
                interactive: true,
                transitions: vec![],
                settings: settings(),
            },
        ],
        input: input.to_string(),
        max_steps: 100,
    }
}

/// A two-step run where each step carries its own settings, so a test can
/// prove a document reports the step it was asked about and not some
/// session-wide value: `plan` runs `gpt-5.6-terra`, `code` runs
/// `deepseek-v4-flash` with a concurrency cap of one. Plan routes to code
/// unconditionally.
pub(super) fn two_model_run_spec_fixture(
    input: &str,
) -> crate::sessions::workflow::WorkflowRunSpec {
    use crate::sessions::workflow::{TransitionSpec, WorkflowRunSpec, WorkflowStepSpec};
    let mut plan_settings = agent_settings_fixture();
    plan_settings.model = "gpt-5.6-terra".into();
    plan_settings.mcp_servers = vec![crate::mcp::selection::whole("planner-mcp")];
    let mut code_settings = agent_settings_fixture();
    code_settings.model = "deepseek-v4-flash".into();
    code_settings.thinking_effort = Some("high".into());
    code_settings.memory_spaces = vec!["codebase".into()];
    code_settings.max_concurrent_subagents = Some(1);
    WorkflowRunSpec {
        workflow: "two-model".into(),
        start: "plan".into(),
        steps: vec![
            WorkflowStepSpec {
                name: "plan".into(),
                agent: "planner".into(),
                prompt: "Plan it.".into(),
                outcomes: crate::sessions::workflow::default_outcomes(),
                fields: Vec::new(),
                interactive: false,
                transitions: vec![TransitionSpec {
                    to: "code".into(),
                    when: None,
                }],
                settings: plan_settings,
            },
            WorkflowStepSpec {
                name: "code".into(),
                agent: "coder".into(),
                prompt: "Code it.".into(),
                outcomes: crate::sessions::workflow::default_outcomes(),
                fields: Vec::new(),
                interactive: false,
                transitions: vec![],
                settings: code_settings,
            },
        ],
        input: input.to_string(),
        max_steps: 100,
    }
}

/// A session that is a run of [`run_spec_fixture`], on a scripted provider.
pub(super) async fn spawn_run_with_provider(
    provider: Arc<dyn LlmProvider>,
) -> (
    ActorFixture,
    SessionRef,
    Uuid,
    Arc<dyn horsie_actor::Journal>,
) {
    let f = actor_fixture().await;
    let id = Uuid::new_v4();
    let mut spec = actor_spec_fixture();
    spec.kind = crate::sessions::spec::SessionKind::Workflow {
        run: Arc::new(run_spec_fixture("the build is red")),
    };
    f.deps
        .runtimes
        .create(
            crate::runtime_manager::RuntimeAddress {
                session: &id.to_string(),
                runtime: &id.to_string(),
                incarnation: "i1",
            },
            "mock",
            &spec.runtime_env().expect("the fixture has a runtime"),
        )
        .await
        .expect("create");
    f.deps.provider_registry.write().unwrap().insert(
        "mock".to_string(),
        crate::sessions::spec::ModelEntry::provider_only(provider),
    );
    let journal = f.journal();
    let session = f.start(id, spec).await;
    (f, session, id, journal)
}

/// Poll one agent's folded state until `pred` holds (2s cap).
///
/// A step's timers, asks and nudge budget live on its own journal, so a test
/// asserting on how a *turn* ended has to wait on this rather than on the run.
pub(super) async fn wait_for_agent(
    journal: &Arc<dyn horsie_actor::Journal>,
    agent_id: Uuid,
    pred: impl Fn(&crate::agent_loop::AgentState) -> bool,
) -> crate::agent_loop::AgentState {
    for _ in 0..200 {
        let state = crate::sessions::events::fold_agent_state(journal, agent_id).await;
        if pred(&state) {
            return state;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let state = crate::sessions::events::fold_agent_state(journal, agent_id).await;
    panic!(
        "agent never satisfied the predicate: parked={} timers={} nudges={}",
        state.parked,
        state.timers.len(),
        state.nudges
    );
}

/// Poll the folded run until `pred` holds (2s cap).
pub(super) async fn wait_for_run(
    journal: &Arc<dyn horsie_actor::Journal>,
    session_id: Uuid,
    pred: impl Fn(&crate::sessions::workflow::WorkflowRunState) -> bool,
) -> crate::sessions::workflow::WorkflowRunState {
    for _ in 0..200 {
        let state = crate::sessions::events::fold_session_state(journal, session_id).await;
        if let Some((_, w)) = state.forest.root_workflow()
            && pred(&w.run)
        {
            return w.run.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let state = crate::sessions::events::fold_session_state(journal, session_id).await;
    panic!(
        "run never satisfied the predicate: {:?}",
        state.forest.root_workflow().map(|(_, w)| &w.run)
    );
}

/// A scripted `submit_result` call carrying this result.
///
/// `outcome` and `description` are added when the caller has not named them, so
/// a test that only cares about routing says `json!({"outcome": "p0"})` and the
/// payload still passes the step's own validation.
pub(super) fn concludes(output: serde_json::Value) -> horsie_agentcore::CompletionResponse {
    let mut input = output;
    if let Some(object) = input.as_object_mut() {
        object
            .entry("outcome")
            .or_insert_with(|| serde_json::json!("success"));
        object
            .entry("description")
            .or_insert_with(|| serde_json::json!("did it"));
    }
    horsie_agentcore::CompletionResponse {
        parts: vec![horsie_agentcore::ContentPart::ToolCall(
            horsie_agentcore::ToolCallPart {
                id: "c-1".into(),
                name: "submit_result".into(),
                input,
            },
        )],
        stop_reason: horsie_agentcore::StopReason::ToolUse,
        usage: horsie_agentcore::Usage::without_cache(1, 1),
    }
}

/// A scripted `ask_user` call.
///
/// A step asks with the same tool a session does, and parks the same way:
/// the call ends the run and stays dangling until an answer arrives against it.
pub(super) fn asks(question: &str) -> horsie_agentcore::CompletionResponse {
    horsie_agentcore::CompletionResponse {
        parts: vec![horsie_agentcore::ContentPart::ToolCall(
            horsie_agentcore::ToolCallPart {
                id: ASK_CALL_ID.into(),
                name: "ask_user".into(),
                input: serde_json::json!({"question": question}),
            },
        )],
        stop_reason: horsie_agentcore::StopReason::ToolUse,
        usage: horsie_agentcore::Usage::without_cache(1, 1),
    }
}

/// The tool-call id [`asks`] parks on, which is what an answer has to name.
pub(super) const ASK_CALL_ID: &str = "a-1";

/// Poll the session's folded state until the tree satisfies `pred` (2s
/// cap). Subagent progress is journal-first, so the fold is the honest
/// thing to wait on.
pub(super) async fn wait_for_tree(
    journal: &Arc<dyn horsie_actor::Journal>,
    session_id: Uuid,
    pred: impl Fn(&crate::sessions::run_forest::RunForest) -> bool,
) {
    for _ in 0..200 {
        let state = crate::sessions::events::fold_session_state(journal, session_id).await;
        if pred(&state.forest) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("tree condition not met within 2s");
}

/// Write a history into a session's log and load it from there.
///
/// A crash, from the session's point of view: every actor is stopped first, so
/// what was resident cannot write over the history being planted, and the
/// session that comes back reads only the log. `SpecRecorded` leads a log that
/// has none, because a session that never recorded one is a session that was
/// never created — recovery would have nothing to adopt and nothing to repair.
///
/// The `Create` at the end is a no-op the log already answers — and provisions
/// nothing, for the same reason. It is there because a command is what brings
/// the actor into being, and the test that follows wants to find it recovered.
///
/// What is planted lands on top of whatever the previous life journaled, so a
/// caller that started this session first must have waited for that start to
/// settle — otherwise the seeded state is whatever the stop happened to
/// interrupt, and the test asserts against a history it did not write.
pub(crate) async fn seed_session(
    f: &ActorFixture,
    id: Uuid,
    spec: SessionSpec,
    events: &[SessionDomainEvent],
) -> SessionRef {
    f.node.restart().await;
    let journal = f.journal();
    let pid = SessionActor::persistence_id_for(id);
    let at = journal.last_seq(&pid).await.unwrap();
    let mut encoded = Vec::new();
    if at == 0 {
        encoded.push(
            serde_json::to_vec(&SessionDomainEvent::SpecRecorded {
                at_ms: 0,
                session: id,
                spec: Box::new(spec.clone()),
            })
            .unwrap(),
        );
    }
    encoded.extend(events.iter().map(|e| serde_json::to_vec(e).unwrap()));
    journal.persist(&pid, &encoded, at).await.unwrap();
    f.start(id, spec).await
}

/// Bring a session into being, provisioning nothing. The session owns its
/// create now, so a test that wants a runtime asks for one.
pub(super) async fn spawn_unprovisioned(
    f: &ActorFixture,
    id: Uuid,
) -> (SessionRef, Arc<dyn horsie_actor::Journal>) {
    let journal = f.journal();
    let session = f.start(id, actor_spec_fixture()).await;
    (session, journal)
}

/// Poll a session's own journal until its decoded events satisfy `pred`.
///
/// Asserting on the *events* rather than the fold is what makes a transition
/// observable: a turn that begins and ends between two polls leaves the status
/// exactly where it started, and only the journal remembers it happened.
pub(super) async fn wait_for_events(
    journal: &Arc<dyn horsie_actor::Journal>,
    session_id: Uuid,
    what: &str,
    pred: impl Fn(&[SessionDomainEvent]) -> bool,
) -> Vec<SessionDomainEvent> {
    for _ in 0..200 {
        let events = crate::sessions::events::session_events(journal, session_id).await;
        if pred(&events) {
            return events;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let events = crate::sessions::events::session_events(journal, session_id).await;
    panic!("{what} not reached within 2s; journal: {events:?}");
}

/// Poll the folded session state until it satisfies `pred`.
pub(super) async fn wait_for_state(
    journal: &Arc<dyn horsie_actor::Journal>,
    session_id: Uuid,
    what: &str,
    pred: impl Fn(&SessionState) -> bool,
) -> SessionState {
    for _ in 0..200 {
        let state = crate::sessions::events::fold_session_state(journal, session_id).await;
        if pred(&state) {
            return state;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let events = crate::sessions::events::session_events(journal, session_id).await;
    panic!("{what} not reached within 2s; journal: {events:?}");
}

/// Entry count of the session's own journal (`session/<id>`), not the
/// agent's.
pub(super) async fn session_journal_len(
    journal: &Arc<dyn horsie_actor::Journal>,
    session_id: Uuid,
) -> u64 {
    use futures_util::StreamExt;
    let pid = SessionActor::persistence_id_for(session_id);
    let mut count = 0u64;
    #[expect(
        clippy::disallowed_methods,
        reason = "test-only inspection: counts what was journaled, which no actor reports"
    )]
    let mut stream = journal.replay(&pid, 0).await;
    while let Some(item) = stream.next().await {
        if item.is_ok() {
            count += 1;
        }
    }
    count
}

/// Poll the session's own journal until it holds at least `n` entries
/// (2s cap).
pub(super) async fn wait_for_journal_len(
    journal: &Arc<dyn horsie_actor::Journal>,
    session_id: Uuid,
    n: u64,
) {
    for _ in 0..200 {
        if session_journal_len(journal, session_id).await >= n {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("journal did not reach {n} entries within 2s");
}

/// Wraps a journal and counts `replay` calls, so a test can assert that
/// serving reads and streams never touches durable storage.
pub(super) struct CountingJournal {
    pub(super) inner: horsie_actor::InMemoryJournal,
    pub(super) replays: std::sync::atomic::AtomicUsize,
}

impl CountingJournal {
    pub(super) fn new() -> Self {
        Self {
            inner: horsie_actor::InMemoryJournal::new(),
            replays: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub(super) fn replays(&self) -> usize {
        self.replays.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl horsie_actor::Journal for CountingJournal {
    async fn persist(
        &self,
        pid: &horsie_actor::PersistenceId,
        events: &[Vec<u8>],
        expected_last_seq: u64,
    ) -> horsie_actor::JournalResult<()> {
        self.inner.persist(pid, events, expected_last_seq).await
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "this decorator's whole job is to count the inner journal's replays"
    )]
    async fn replay(
        &self,
        pid: &horsie_actor::PersistenceId,
        after_seq: u64,
    ) -> futures_util::stream::BoxStream<'_, horsie_actor::JournalResult<(u64, Vec<u8>)>> {
        self.replays
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.replay(pid, after_seq).await
    }

    async fn save_snapshot(
        &self,
        pid: &horsie_actor::PersistenceId,
        state: Vec<u8>,
        seq_nr: u64,
    ) -> horsie_actor::JournalResult<()> {
        self.inner.save_snapshot(pid, state, seq_nr).await
    }

    async fn latest_snapshot(
        &self,
        pid: &horsie_actor::PersistenceId,
    ) -> horsie_actor::JournalResult<Option<(Vec<u8>, u64)>> {
        self.inner.latest_snapshot(pid).await
    }

    async fn delete_events_before(
        &self,
        pid: &horsie_actor::PersistenceId,
        seq_nr: u64,
    ) -> horsie_actor::JournalResult<()> {
        self.inner.delete_events_before(pid, seq_nr).await
    }

    async fn copy_snapshot(
        &self,
        from: &horsie_actor::PersistenceId,
        to: &horsie_actor::PersistenceId,
    ) -> horsie_actor::JournalResult<()> {
        self.inner.copy_snapshot(from, to).await
    }

    async fn last_seq(
        &self,
        pid: &horsie_actor::PersistenceId,
    ) -> horsie_actor::JournalResult<u64> {
        self.inner.last_seq(pid).await
    }

    async fn clear(&self, pid: &horsie_actor::PersistenceId) -> horsie_actor::JournalResult<()> {
        self.inner.clear(pid).await
    }
}

/// Spawn a subagent under whatever this session's own "main" is right now:
/// the root run's step in flight for a workflow session, the main agent
/// otherwise — the same resolution the spawn tool performs from inside the
/// calling agent.
pub(super) async fn spawn_sub(session: &SessionRef, label: &str, task: &str) -> Uuid {
    let caller = current_main_agent(session).await;
    session
        .ask(|reply| {
            SessionCommand::SubAgent(SubAgentCommand::Spawn {
                caller,
                title: label.into(),
                task: task.into(),
                agent_type: None,
                reply,
            })
        })
        .await
        .unwrap()
        .unwrap()
}

/// The agent an unaddressed request means: the root run's step in flight, or
/// the main agent (whose id is the session's).
pub(super) async fn current_main_agent(session: &SessionRef) -> Uuid {
    session
        .ask(|reply| {
            SessionCommand::Run(crate::sessions::session_actor::RunCommand::State { reply })
        })
        .await
        .ok()
        .flatten()
        .and_then(|run| run.current_agent())
        .unwrap_or_else(|| session.session())
}

/// How each turn in one agent's log ended, in order.
///
/// A page folds `TurnBegan` as `Running` and clears it only on the matching
/// `TurnEnded`, so this is what "is that agent still working" is read off.
pub(super) fn turn_outcomes(
    page: &crate::agent_loop::LogPage,
) -> Vec<horsie_agentcore::TurnOutcome> {
    page.entries
        .iter()
        .filter_map(|e| match &e.body {
            horsie_agentcore::AgentLogBody::Lifecycle(
                horsie_agentcore::LifecycleEvent::TurnEnded(t),
            ) => Some(t.outcome.clone()),
            _ => None,
        })
        .collect()
}

/// How many turns began in one agent's log — the other half of
/// [`turn_outcomes`], and what an unmatched `TurnBegan` is counted against.
pub(super) fn turns_begun(page: &crate::agent_loop::LogPage) -> usize {
    page.entries
        .iter()
        .filter(|e| {
            matches!(
                &e.body,
                horsie_agentcore::AgentLogBody::Lifecycle(
                    horsie_agentcore::LifecycleEvent::TurnBegan(_)
                )
            )
        })
        .count()
}

pub(super) fn user_texts(page: &crate::agent_loop::LogPage) -> Vec<String> {
    page.messages()
        .filter(|m| m.role == horsie_agentcore::Role::User)
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            horsie_agentcore::ContentPart::Text(t) => Some(t.text.clone()),
            horsie_agentcore::ContentPart::ToolCall(_)
            | horsie_agentcore::ContentPart::ToolResult(_)
            | horsie_agentcore::ContentPart::Thinking(_)
            | horsie_agentcore::ContentPart::SubAgentResult(_)
            | horsie_agentcore::ContentPart::Artifact(_) => None,
        })
        .collect()
}

/// A user message's subagent-result parts, rendered the way the wire sees
/// them — the counterpart to `user_texts` now that a result is a part of
/// its own rather than text merged into what the person said.
/// Poll the main agent's subagent-result parts until `want` holds (2s cap).
///
/// The honest wait for "the main agent has taken a result". A subagent's
/// `notified` flag says the result was *handed over* — one message into main's
/// mailbox — and main has yet to process it, so the flag is not a boundary the
/// history can be read at.
///
/// Returns whatever it last saw either way, so the caller's own assertion is
/// what reports the failure and says what it wanted.
pub(super) async fn wait_for_subagent_text(
    session: &SessionRef,
    want: impl Fn(&[String]) -> bool,
) -> Vec<String> {
    let mut texts = Vec::new();
    for _ in 0..200 {
        texts = subagent_texts(&main_history(session).await);
        if want(&texts) {
            return texts;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    texts
}

pub(super) fn subagent_texts(page: &crate::agent_loop::LogPage) -> Vec<String> {
    page.messages()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            horsie_agentcore::ContentPart::SubAgentResult(r) => Some(r.to_wire_text()),
            horsie_agentcore::ContentPart::Text(_)
            | horsie_agentcore::ContentPart::ToolCall(_)
            | horsie_agentcore::ContentPart::ToolResult(_)
            | horsie_agentcore::ContentPart::Thinking(_)
            | horsie_agentcore::ContentPart::Artifact(_) => None,
        })
        .collect()
}

pub(super) fn hook_record(plugin: &str, call: &str) -> HookRecord {
    HookRecord {
        plugin: plugin.to_string(),
        duration_ms: 4,
        halt: None,
        action: horsie_models::hooks::HookAction::PreToolUse(
            horsie_models::hooks::PreToolUseRecord {
                call: horsie_models::hooks::ToolScope {
                    tool: "bash".to_string(),
                    tool_call_id: call.to_string(),
                },
                system_message: None,
                outcome: horsie_models::hooks::PreToolUseOutcome::Denied(
                    horsie_models::hooks::HookDenied {
                        reason: Some("not allowed".into()),
                    },
                ),
            },
        ),
    }
}

pub(super) async fn agent_history(
    session: &SessionRef,
    agent_id: Option<String>,
) -> crate::agent_loop::LogPage {
    session
        .ask(|reply| {
            SessionCommand::Read(ReadCommand::PageLog {
                agent_id,
                anchor: crate::agent_loop::Anchor::Tail,
                max: 50,
                filter: crate::agent_loop::LogFilter::everything(),
                reply,
            })
        })
        .await
        .unwrap()
        .expect("agent history")
}

pub(super) fn hook_ids(page: &crate::agent_loop::LogPage) -> Vec<String> {
    page.entries
        .iter()
        .filter_map(|e| match &e.body {
            horsie_agentcore::AgentLogBody::Hook(h) => Some(h.id.clone()),
            horsie_agentcore::AgentLogBody::Llm(_)
            | horsie_agentcore::AgentLogBody::Lifecycle(_)
            | horsie_agentcore::AgentLogBody::Compaction(_) => None,
        })
        .collect()
}

pub(super) fn stop_record(outcome: StopOutcome) -> HookRecord {
    HookRecord {
        plugin: "stopper".into(),
        duration_ms: 1,
        halt: None,
        action: HookAction::Stop(horsie_models::hooks::StopRecord {
            system_message: None,
            outcome,
        }),
    }
}

pub(super) fn stop_blocked(reason: &str) -> Vec<HookRecord> {
    vec![stop_record(StopOutcome::Blocked(
        horsie_models::hooks::HookBlocked {
            reason: Some(reason.to_string()),
        },
    ))]
}

/// An `EchoProvider` that also keeps every text part it was prompted with,
/// so a test can assert on what the model was actually told rather than on
/// what the transcript would translate to.
#[derive(Default)]
pub(super) struct PromptRecorder(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl LlmProvider for PromptRecorder {
    fn model_id(&self) -> &str {
        "mock"
    }

    async fn complete(
        &self,
        request: horsie_agentcore::CompletionRequest<'_>,
        _message_id: &str,
        _events: &dyn horsie_agentcore::EventSink,
    ) -> Result<horsie_agentcore::CompletionResponse, horsie_agentcore::LlmError> {
        let mut seen = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        for m in request.messages {
            for p in &m.parts {
                if let horsie_agentcore::ContentPart::Text(t) = p {
                    seen.push(t.text.clone());
                }
            }
        }
        drop(seen);
        Ok(horsie_agentcore::CompletionResponse {
            parts: vec![horsie_agentcore::ContentPart::Text(
                horsie_agentcore::TextPart {
                    text: "done".to_string(),
                },
            )],
            stop_reason: horsie_agentcore::StopReason::EndTurn,
            usage: horsie_agentcore::Usage::without_cache(1, 1),
        })
    }
}

/// A session whose runtime answers every `RunHooks` from `records`, with an
/// LLM that concludes on the first call.
pub(super) async fn stop_harness(records: Vec<Vec<HookRecord>>) -> (ActorFixture, SessionRef) {
    let (f, session, _, _, _) = stop_harness_full(records).await;
    (f, session)
}

/// The same harness, also handing back every prompt the model was sent.
pub(super) async fn stop_harness_with_prompts(
    records: Vec<Vec<HookRecord>>,
) -> (ActorFixture, SessionRef, Arc<Mutex<Vec<String>>>) {
    let (f, session, prompts, _, _) = stop_harness_full(records).await;
    (f, session, prompts)
}

/// The same harness, also handing back the journal, for a test that has to
/// read what was *persisted*. A spurious failure is overwritten in the
/// status by whatever lands next; the journal keeps it.
pub(super) async fn stop_harness_with_journal(
    records: Vec<Vec<HookRecord>>,
) -> (
    ActorFixture,
    SessionRef,
    Uuid,
    Arc<dyn horsie_actor::Journal>,
) {
    let (f, session, _, id, journal) = stop_harness_full(records).await;
    (f, session, id, journal)
}

pub(super) async fn stop_harness_full(
    records: Vec<Vec<HookRecord>>,
) -> (
    ActorFixture,
    SessionRef,
    Arc<Mutex<Vec<String>>>,
    Uuid,
    Arc<dyn horsie_actor::Journal>,
) {
    let agent = crate::runtime_vendor::fake::FakeRuntimeVendor::builder("mock")
        .hook_records(records)
        .serve_in_process()
        .await
        .expect("fake agent");
    let f = fixture_over(agent, None).await;
    let id = Uuid::new_v4();
    // `"0"` rather than a name of its own: the seeded log below says this
    // runtime was provisioned at 0, and the incarnation an agent addresses
    // comes from that log. Any other name here builds a sandbox nothing can
    // reach.
    f.deps
        .runtimes
        .create(
            crate::runtime_manager::RuntimeAddress {
                session: &id.to_string(),
                runtime: &id.to_string(),
                incarnation: "0",
            },
            "mock",
            &actor_spec_fixture()
                .runtime_env()
                .expect("the fixture has a runtime"),
        )
        .await
        .expect("create");
    let prompts: Arc<Mutex<Vec<String>>> = Arc::default();
    f.deps.provider_registry.write().unwrap().insert(
        "mock".to_string(),
        crate::sessions::spec::ModelEntry::provider_only(
            Arc::new(PromptRecorder(prompts.clone())) as Arc<dyn LlmProvider>
        ),
    );
    let journal = f.journal();
    let session = f.start(id, actor_spec_fixture()).await;
    (f, session, prompts, id, journal)
}

/// Every user-role message in the main agent's transcript, in order — which
/// is one per turn, so its length is the number of turns that ran.
pub(super) async fn turn_inputs(session: &SessionRef) -> Vec<String> {
    agent_history(session, None)
        .await
        .entries
        .iter()
        .filter_map(|e| match &e.body {
            horsie_agentcore::AgentLogBody::Llm(m) if m.role == horsie_agentcore::Role::User => {
                Some(m.parts.iter().fold(String::new(), |mut acc, p| {
                    if let horsie_agentcore::ContentPart::Text(t) = p {
                        acc.push_str(&t.text);
                    }
                    acc
                }))
            }
            horsie_agentcore::AgentLogBody::Llm(_)
            | horsie_agentcore::AgentLogBody::Hook(_)
            | horsie_agentcore::AgentLogBody::Lifecycle(_)
            | horsie_agentcore::AgentLogBody::Compaction(_) => None,
        })
        .collect()
}

/// The `Stop` outcomes journaled on the main agent's transcript.
pub(super) async fn stop_outcomes(session: &SessionRef) -> Vec<StopOutcome> {
    agent_history(session, None)
        .await
        .entries
        .iter()
        .filter_map(|e| match &e.body {
            horsie_agentcore::AgentLogBody::Hook(h) => match &h.record.action {
                HookAction::Stop(r) => Some(r.outcome.clone()),
                other => panic!("only Stop hooks run in these tests, got {other:?}"),
            },
            horsie_agentcore::AgentLogBody::Llm(_)
            | horsie_agentcore::AgentLogBody::Lifecycle(_)
            | horsie_agentcore::AgentLogBody::Compaction(_) => None,
        })
        .collect()
}

/// Wait until the transcript stops growing, so a test asserting "no further
/// turn ran" observes a real stop rather than a race it won.
///
/// The whole transcript, not the user messages it returns. Watching only the
/// inputs made this settle the moment the *first* entry of a turn had landed —
/// a turn writes exactly one of them, at the start — so it reported quiet with
/// the turn still running, and every assertion made after it about what the
/// turn produced was reading a transcript that had not finished being written.
/// On a fast machine the turn beat the window anyway; CI is where that stopped
/// being true.
///
/// Still a settle rather than a turn boundary, because its callers assert that
/// nothing *further* happened, and there is no event for a turn that never
/// began. [`await_turns`] is the exact wait, for a test that knows how many it
/// is expecting.
pub(super) async fn settled_inputs(session: &SessionRef) -> Vec<String> {
    let mut last = 0;
    let mut stable = 0;
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let now = agent_history(session, None).await.entries.len();
        if now == last {
            stable += 1;
            if stable == 5 {
                break;
            }
        } else {
            stable = 0;
            last = now;
        }
    }
    turn_inputs(session).await
}

/// Wait until `want` turns have ended on the main agent's log.
///
/// The boundary itself, for the tests that assert on what a turn produced: a
/// `TurnEnded` carries how it ended, so a halted or failed turn counts here
/// exactly as a completed one does. Nothing a turn writes can land after it.
pub(super) async fn await_turns(session: &SessionRef, want: usize) {
    for _ in 0..400 {
        let page = agent_history(session, None).await;
        if turn_outcomes(&page).len() >= want {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let page = agent_history(session, None).await;
    panic!(
        "only {} of {want} turns ended: {:?}",
        turn_outcomes(&page).len(),
        page.entries
    );
}

pub(super) async fn send(session: &SessionRef, text: &str) {
    session
        .ask(|reply| {
            SessionCommand::Turn(TurnCommand::UserMessage {
                agent_id: None,
                text: text.into(),
                reply,
                artifacts: Vec::new(),
            })
        })
        .await
        .unwrap()
        .unwrap();
}

/// Every session event that reached the journal, as its serialized payload.
/// Matched on as text, because the variant name is what a test cares about
/// and decoding buys nothing over reading it.
pub(super) async fn journaled_events(
    journal: &Arc<dyn horsie_actor::Journal>,
    session_id: Uuid,
) -> Vec<String> {
    use futures_util::StreamExt;
    let pid = SessionActor::persistence_id_for(session_id);
    #[expect(
        clippy::disallowed_methods,
        reason = "test-only inspection: names what was journaled, which no actor reports"
    )]
    let mut stream = journal.replay(&pid, 0).await;
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        if let Ok((_, bytes)) = item {
            out.push(String::from_utf8_lossy(&bytes).into_owned());
        }
    }
    out
}

/// A plugin library scripted with a fixed catalogue.
///
/// The seam's question is "what does this name mean?", and the answer comes
/// from the database. Ingesting a real bundle to answer it would test
/// `pack()` a second time and pay for a git clone per case.
pub(super) struct FakeLibrary(Vec<horsie_support::plugin::catalog::CatalogEntry>);

#[async_trait]
impl crate::plugins::PluginProvisioner for FakeLibrary {
    async fn resolve(
        &self,
        _names: &[String],
    ) -> Result<Vec<horsie_models::runtime::BundleRef>, String> {
        Ok(Vec::new())
    }

    async fn default_names(&self) -> Vec<String> {
        Vec::new()
    }

    async fn catalog(
        &self,
        _names: &[String],
    ) -> Vec<horsie_support::plugin::catalog::CatalogEntry> {
        self.0.clone()
    }
}

pub(super) fn catalog_entry(
    kind: horsie_support::plugin::catalog::CatalogKind,
    name: &str,
    template: Option<&str>,
) -> horsie_support::plugin::catalog::CatalogEntry {
    horsie_support::plugin::catalog::CatalogEntry {
        kind,
        name: name.into(),
        description: "d".into(),
        argument_hint: None,
        template: template.map(str::to_string),
    }
}

pub(super) async fn catalog_harness(
    entries: Vec<horsie_support::plugin::catalog::CatalogEntry>,
) -> (ActorFixture, SessionRef, Uuid) {
    catalog_harness_with(entries, Vec::new()).await
}

pub(super) async fn catalog_harness_with(
    entries: Vec<horsie_support::plugin::catalog::CatalogEntry>,
    hook_records: Vec<Vec<HookRecord>>,
) -> (ActorFixture, SessionRef, Uuid) {
    let agent = crate::runtime_vendor::fake::FakeRuntimeVendor::builder("mock")
        .hook_records(hook_records)
        .serve_in_process()
        .await
        .expect("fake agent");
    let f = fixture_over(agent, Some(Arc::new(FakeLibrary(entries)))).await;
    let id = Uuid::new_v4();
    f.deps
        .runtimes
        .create(
            crate::runtime_manager::RuntimeAddress {
                session: &id.to_string(),
                runtime: &id.to_string(),
                incarnation: "i1",
            },
            "mock",
            &actor_spec_fixture()
                .runtime_env()
                .expect("the fixture has a runtime"),
        )
        .await
        .expect("create");
    f.deps.provider_registry.write().unwrap().insert(
        "mock".to_string(),
        crate::sessions::spec::ModelEntry::provider_only(
            Arc::new(PromptRecorder(Arc::default())) as Arc<dyn LlmProvider>
        ),
    );
    let session = f.start(id, actor_spec_fixture()).await;
    (f, session, id)
}

pub(super) fn catalog_provider(
    f: &ActorFixture,
    session: &SessionRef,
    id: Uuid,
) -> SessionContextProvider {
    SessionContextProvider {
        runtimes: Mutex::new(AgentRuntimeBinding::On(Box::new(
            f.deps.runtimes.provider(
                id.to_string(),
                id.to_string(),
                "i1".to_string(),
                false,
                "mock".to_string(),
                crate::sessions::spec::SessionSpec::for_vendor("mock")
                    .runtime_env()
                    .expect("a vendor spec has a runtime"),
            ),
        ))),
        registry: f.deps.provider_registry.clone(),
        mcp: None,
        memory: None,
        services: None,
        settings: agent_settings_fixture(),
        step_result: Default::default(),
        session_id: id,
        kind: SessionAgentKind::Main,
        agent_type: None,
        origin: None,
        unattended: false,
        session: session.clone(),
        plugins: Vec::new(),
        plugin_library: f.deps.plugins.clone(),
        last_client: Mutex::new(None),
    }
}

/// The prompt the seam produced for this turn — the whole point of the
/// expansion, since it is what the model actually reads.
pub(super) async fn prepared_message(
    provider: &SessionContextProvider,
    prompt: &str,
) -> Option<String> {
    provider
        .start_hooks(StartTurn {
            start_source: None,
            prompt: Some(prompt.to_string()),
        })
        .await
        .expect("prepare")
        .message
}

/// A session whose runtime library declares `code-reviewer`, with a
/// `PromptRecorder` so the test can assert what the model was actually
/// told rather than what the transcript would render.
pub(super) async fn agent_harness() -> (ActorFixture, SessionRef, Uuid) {
    let agent = crate::runtime_vendor::fake::FakeRuntimeVendor::builder("mock")
        .shared_agents(vec![horsie_models::runtime::PluginAgent {
            plugin: "feature-dev".into(),
            rel_path: "feature-dev/agents/code-reviewer.md".into(),
            content: "---\nname: code-reviewer\ndescription: reviews diffs\n\
                      tools: Read, Grep\n---\nReport only high-confidence bugs."
                .into(),
        }])
        .serve_in_process()
        .await
        .expect("fake agent");
    let f = fixture_over(agent, None).await;
    let id = Uuid::new_v4();
    f.deps
        .runtimes
        .create(
            crate::runtime_manager::RuntimeAddress {
                session: &id.to_string(),
                runtime: &id.to_string(),
                incarnation: "i1",
            },
            "mock",
            &actor_spec_fixture()
                .runtime_env()
                .expect("the fixture has a runtime"),
        )
        .await
        .expect("create");
    let prompts: Arc<Mutex<Vec<String>>> = Arc::default();
    f.deps.provider_registry.write().unwrap().insert(
        "mock".to_string(),
        crate::sessions::spec::ModelEntry::provider_only(
            Arc::new(PromptRecorder(prompts.clone())) as Arc<dyn LlmProvider>
        ),
    );
    // Seeded rather than created: this harness built the runtime above, and a
    // `Create` would provision a second one that nothing here ever runs on.
    // Seeding puts the spec in the log first, which is what makes the create
    // the no-op a reload is.
    //
    // The runtime built above has to be *named* in the log too. Without the
    // ask and its outcome the session points at no runtime, and every agent
    // under it resolves to "unanswered" — which correctly refuses to run, so
    // the harness would sit there taking no turns.
    let session = seed_session(
        &f,
        id,
        actor_spec_fixture(),
        &harness_runtime_events(id, RuntimeId(id)),
    )
    .await;
    drop(prompts);
    (f, session, id)
}

/// The log a session leaves behind when it asked for `runtime` and got it.
///
/// For harnesses that build the sandbox themselves and then seed a history to
/// match: the incarnation is `0` because that is what the create above used,
/// and a mismatch there addresses a sandbox that does not exist.
pub(super) fn harness_runtime_events(owner: Uuid, runtime: RuntimeId) -> Vec<SessionDomainEvent> {
    vec![
        SessionDomainEvent::RuntimeRequested {
            at_ms: 0,
            runtime,
            owner,
            env: actor_spec_fixture()
                .runtime_env()
                .expect("the fixture has a runtime"),
        },
        SessionDomainEvent::ProvisioningStarted { at_ms: 0, runtime },
        SessionDomainEvent::ProvisioningSucceeded { at_ms: 0, runtime },
    ]
}

pub(super) async fn spawn_typed(
    session: &SessionRef,
    agent_type: Option<&str>,
) -> Result<Uuid, String> {
    session
        .ask(|reply| {
            SessionCommand::SubAgent(SubAgentCommand::Spawn {
                caller: session.session(),
                title: "review".into(),
                task: "look at the diff".into(),
                agent_type: agent_type.map(str::to_string),
                reply,
            })
        })
        .await
        .unwrap()
}

/// A provider for one subagent of `agent_harness`'s session, optionally
/// carrying a session-level tool allowlist.
pub(super) fn typed_provider(
    f: &ActorFixture,
    session: &SessionRef,
    id: Uuid,
    sub: Uuid,
    allowed_tools: Option<Vec<String>>,
) -> SessionContextProvider {
    let mut settings = agent_settings_fixture();
    settings.allowed_tools = allowed_tools;
    SessionContextProvider {
        runtimes: Mutex::new(AgentRuntimeBinding::On(Box::new(
            f.deps.runtimes.provider(
                id.to_string(),
                id.to_string(),
                "i1".to_string(),
                false,
                "mock".to_string(),
                crate::sessions::spec::SessionSpec::for_vendor("mock")
                    .runtime_env()
                    .expect("a vendor spec has a runtime"),
            ),
        ))),
        registry: f.deps.provider_registry.clone(),
        mcp: None,
        memory: None,
        services: None,
        settings,
        step_result: Default::default(),
        session_id: id,
        kind: SessionAgentKind::Sub(sub),
        agent_type: Some("code-reviewer".to_string()),
        origin: None,
        unattended: false,
        session: session.clone(),
        plugins: Vec::new(),
        plugin_library: None,
        last_client: Mutex::new(None),
    }
}

/// The parent's subagent reports, polled until `needle` is among them.
///
/// Polled, not read once. `SubAgentNotified` is journaled as soon as the
/// parent's *mailbox* accepts the report — it is a `tell` — so the flag means
/// "handed over", not "recorded". The parent appends to its own history a
/// scheduling hop later, and reading immediately races that.
///
/// Returns whatever it last saw, so the caller's assertion is what names the
/// failure rather than this.
pub(super) async fn await_subagent_text(session: &SessionRef, needle: &str) -> Vec<String> {
    let mut texts = Vec::new();
    for _ in 0..300 {
        texts = subagent_texts(&main_history(session).await);
        if texts.iter().any(|t| t.contains(needle)) {
            return texts;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    texts
}

pub(super) async fn main_history(session: &SessionRef) -> crate::agent_loop::LogPage {
    session
        .ask(|reply| {
            SessionCommand::Read(ReadCommand::PageLog {
                agent_id: None,
                anchor: crate::agent_loop::Anchor::Tail,
                max: 50,
                filter: crate::agent_loop::LogFilter::everything(),
                reply,
            })
        })
        .await
        .unwrap()
        .expect("main agent log")
}

/// Fails any completion whose session contains `needle`; answers
/// everything else with plain text. Distinguishes the subagent's run from
/// the main agent's when both share one provider.
pub(super) struct FailOnNeedleProvider {
    pub(super) needle: String,
}

#[async_trait]
impl LlmProvider for FailOnNeedleProvider {
    fn model_id(&self) -> &str {
        "mock"
    }

    async fn complete(
        &self,
        request: horsie_agentcore::CompletionRequest<'_>,
        _message_id: &str,
        _events: &dyn horsie_agentcore::EventSink,
    ) -> Result<horsie_agentcore::CompletionResponse, horsie_agentcore::LlmError> {
        let hit = request
            .messages
            .iter()
            .flat_map(|m| m.parts.iter())
            .any(|p| matches!(p, horsie_agentcore::ContentPart::Text(t) if t.text.contains(&self.needle)));
        if hit {
            return Err(horsie_agentcore::LlmError::ApiError {
                status: 401,
                message: "bad key".to_string(),
            });
        }
        Ok(horsie_agentcore::CompletionResponse {
            parts: vec![horsie_agentcore::ContentPart::Text(
                horsie_agentcore::TextPart {
                    text: "fine".to_string(),
                },
            )],
            stop_reason: horsie_agentcore::StopReason::EndTurn,
            usage: horsie_agentcore::Usage::without_cache(1, 1),
        })
    }
}

/// Answers a step by never returning, and everything else with plain text.
///
/// A step must stay in flight for the length of these tests: it is the tree a
/// spawn belongs in, and a concluded step takes its tree out of play. Told
/// apart by the step's own prompt, which no subagent session carries.
pub(super) struct StepStallsProvider;

#[async_trait]
impl LlmProvider for StepStallsProvider {
    fn model_id(&self) -> &str {
        "mock"
    }

    async fn complete(
        &self,
        request: horsie_agentcore::CompletionRequest<'_>,
        _message_id: &str,
        _events: &dyn horsie_agentcore::EventSink,
    ) -> Result<horsie_agentcore::CompletionResponse, horsie_agentcore::LlmError> {
        let is_step = request.messages.iter().any(|m| {
            m.parts.iter().any(|p| {
                matches!(p, horsie_agentcore::ContentPart::Text(t) if t.text.contains("Triage it."))
            })
        });
        if is_step {
            // Never returns. The step stays `Running` and its tree stays live.
            std::future::pending::<()>().await;
        }
        Ok(horsie_agentcore::CompletionResponse {
            parts: vec![horsie_agentcore::ContentPart::Text(
                horsie_agentcore::TextPart {
                    text: "sub answer".to_string(),
                },
            )],
            stop_reason: horsie_agentcore::StopReason::EndTurn,
            usage: horsie_agentcore::Usage::without_cache(1, 1),
        })
    }
}

/// A run whose first step is in flight and stays there, so it has a tree that
/// spawns belong in.
pub(super) async fn a_run_with_a_step_in_flight() -> (
    ActorFixture,
    SessionRef,
    Uuid,
    Arc<dyn horsie_actor::Journal>,
) {
    let (f, session, id, journal) = spawn_run_with_provider(Arc::new(StepStallsProvider)).await;
    wait_for_run(&journal, id, |r| r.current().is_some()).await;
    (f, session, id, journal)
}

/// A substrate that has to boot something, and says so.
///
/// No websocket-backed fake can stand in for one: a `horsie connect` link only
/// ever answers once its runtime is already up, which is exactly the case with
/// nothing to narrate. This is the other case — the one every cloud vendor in
/// the tree is — where the words are the only thing a person waiting has.
pub(super) struct BootingVendor;

/// The words this vendor uses, named so a test asserts on the same string the
/// vendor produced rather than on a copy of it.
pub(super) const BOOTING_CREATE: &str = "the machine is booting";
pub(super) const BOOTING_ACQUIRE: &str = "the machine is resuming";

pub(super) struct StubHandle;

#[async_trait]
impl horsie_runtime_host::RuntimeTransport for StubHandle {
    async fn relay(
        &self,
        message: horsie_models::runtime::RuntimeInboundMessage,
    ) -> Result<horsie_models::runtime::RuntimeOutboundMessage, horsie_runtime_host::TransportError>
    {
        // Agent provisioning is answered; everything else still reports a dead
        // link, which is what these tests are about.
        //
        // Not politeness. Provisioning is the one preparation step that fails
        // the turn rather than degrading it — an agent whose plugins did not
        // install would otherwise run with a silently reduced skill set. A
        // stub that refused it would fail every turn before the thing under
        // test ran, which is a property of the double rather than of the code.
        if let horsie_models::runtime::RuntimeInboundMessage::ProvisionAgent(req) = message {
            return Ok(
                horsie_models::runtime::RuntimeOutboundMessage::AgentProvisioned(
                    horsie_models::runtime::ProvisionAgentResponse {
                        call_id: req.call_id,
                        root: "/stub/plugins/agents/x".to_string(),
                        result: horsie_models::runtime::ProvisionResult::Ok(
                            horsie_models::runtime::ProvisionOk {
                                applied: Vec::new(),
                            },
                        ),
                    },
                ),
            );
        }
        Err(horsie_runtime_host::TransportError::Disconnected)
    }
    async fn send_oneway(
        &self,
        _: horsie_models::runtime::RuntimeInboundMessage,
    ) -> Result<(), horsie_runtime_host::TransportError> {
        Ok(())
    }
}

#[async_trait]
impl crate::runtime_vendor::RuntimeVendor for BootingVendor {
    fn name(&self) -> &str {
        "mock"
    }
    fn capabilities(&self) -> horsie_models::runtime_vendor::RuntimeVendorCapabilities {
        horsie_models::runtime_vendor::RuntimeVendorCapabilities {
            supports_provisioning: true,
        }
    }
    async fn create(
        &self,
        _: &str,
        _: &horsie_models::runtime_vendor::RuntimeSpec,
        _: horsie_runtime_host::RuntimeProgressSink,
    ) -> Result<horsie_runtime_host::RuntimeProgress, crate::runtime_vendor::RuntimeVendorError>
    {
        Ok(horsie_runtime_host::RuntimeProgress::Starting {
            detail: BOOTING_CREATE.into(),
        })
    }
    /// `Starting` first and the outcome on the sink, per the vendor contract —
    /// which is what makes this the acquisition a person waits through.
    async fn get(
        &self,
        runtime_id: &str,
        _: &horsie_models::runtime_vendor::RuntimeSpec,
        _provisioning: bool,
        progress: horsie_runtime_host::RuntimeProgressSink,
    ) -> Result<horsie_runtime_host::RuntimeProgress, crate::runtime_vendor::RuntimeVendorError>
    {
        let id = runtime_id.to_string();
        // Spawned after the return value is built, per the ordering rule.
        tokio::spawn(async move {
            let _ = progress
                .send(horsie_runtime_host::RuntimeEvent {
                    runtime_id: id,
                    progress: horsie_runtime_host::RuntimeProgress::Ready(Arc::new(StubHandle)),
                })
                .await;
        });
        Ok(horsie_runtime_host::RuntimeProgress::Starting {
            detail: BOOTING_ACQUIRE.into(),
        })
    }
    async fn hibernate(
        &self,
        _: &str,
        _: horsie_runtime_host::RuntimeProgressSink,
    ) -> Result<horsie_runtime_host::RuntimeProgress, crate::runtime_vendor::RuntimeVendorError>
    {
        Ok(horsie_runtime_host::RuntimeProgress::Stopped)
    }
    async fn delete(
        &self,
        _: &str,
        _: horsie_runtime_host::RuntimeProgressSink,
    ) -> Result<horsie_runtime_host::RuntimeProgress, crate::runtime_vendor::RuntimeVendorError>
    {
        Ok(horsie_runtime_host::RuntimeProgress::Gone {
            reason: "deleted".into(),
        })
    }
}
