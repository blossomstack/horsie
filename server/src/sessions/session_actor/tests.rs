//! Tests for the session actor.
//!
//! In their own file rather than an inline `mod tests`: at ~3,800 lines they
//! are three times the actor they cover, and inline they buried it. Privacy is
//! unchanged — a child module still sees everything its parent can.

use super::{
    context::{SessionAgentKind, SessionContextProvider, scoped_client},
    hooks::MAX_STOP_CONTINUATIONS,
    *,
};
use crate::sessions::spec::AgentSettings;
use horsie_agentcore::LlmProvider;
use horsie_models::hooks::{HookAction, StopOutcome};
use horsie_workflow::{ContextProvider, Contexts, StartTurn};
use std::sync::PoisonError;

/// A snapshot written before `mode` existed carries `subagents` at the top
/// level, flat. It must load with its tree intact — anything else silently
/// drops every subagent of every deployed session.
#[test]
fn a_pre_mode_snapshot_keeps_its_subagents() {
    let legacy = serde_json::json!({
        "status": "Idle",
        "inbox": [],
        "subagents": { "nodes": { "3f1a2b4c-0000-4000-8000-000000000001": {
            "parent": "Main", "label": "reader", "task": "read the file", "depth": 1,
            "status": "Completed", "output": "done", "error": null, "notified": true
        }}}
    });
    let state: SessionState = serde_json::from_value(legacy).unwrap();
    let id = Uuid::parse_str("3f1a2b4c-0000-4000-8000-000000000001").unwrap();
    assert_eq!(state.subagents.node(id).unwrap().label, "reader");
    assert_eq!(state.subagents.owner_of(id), Some(TreeOwner::Main));
}

/// A snapshot written after `mode` existed nests the tree under
/// `mode.subagents` for a conversation.
#[test]
fn a_mode_tagged_conversation_snapshot_keeps_its_subagents() {
    let legacy = serde_json::json!({
        "status": "Idle",
        "mode": { "kind": "Interactive", "subagents": { "nodes": {
            "3f1a2b4c-0000-4000-8000-000000000002": {
                "parent": "Main", "label": "auditor", "task": "t", "depth": 1,
                "status": "Running", "output": null, "error": null, "notified": false
            }}}}
    });
    let state: SessionState = serde_json::from_value(legacy).unwrap();
    let id = Uuid::parse_str("3f1a2b4c-0000-4000-8000-000000000002").unwrap();
    assert_eq!(state.subagents.node(id).unwrap().label, "auditor");
    assert_eq!(state.subagents.active_count(), 1);
}

/// A run's snapshot nested one tree per step. Each must land under that step's
/// agent id, and the run itself must survive.
#[test]
fn a_workflow_snapshot_lands_each_steps_tree_under_that_step() {
    let step_agent = "3f1a2b4c-0000-4000-8000-0000000000aa";
    let child_id = "3f1a2b4c-0000-4000-8000-0000000000bb";
    let legacy = serde_json::json!({
        "status": "Running",
        "mode": { "kind": "Workflow", "run": {
            "status": "Running",
            "steps": [{
                "step": "review", "agent": step_agent, "attempt": 1, "from": null,
                "via": null, "input": "go", "status": "Running", "output": null,
                "error": null, "started_at_ms": 1, "ended_at_ms": null,
                "subagents": { "nodes": { child_id: {
                    "parent": "Main", "label": "helper", "task": "t", "depth": 1,
                    "status": "Completed", "output": "kid done", "error": null,
                    "notified": false
                }}}
            }],
            "output": null, "error": null
        }}
    });
    let state: SessionState = serde_json::from_value(legacy).unwrap();
    let owner = TreeOwner::Step(Uuid::parse_str(step_agent).unwrap());
    let child = Uuid::parse_str(child_id).unwrap();
    assert_eq!(state.subagents.owner_of(child), Some(owner));
    assert_eq!(state.run.as_ref().unwrap().steps.len(), 1);
    // The aggregate that answered 0 before this change.
    assert_eq!(state.subagents.owed().len(), 1);
}

/// The new shape round-trips.
#[test]
fn the_new_state_shape_round_trips() {
    let mut state = SessionState::default();
    let id = Uuid::new_v4();
    state.subagents.tree_mut(TreeOwner::Main).apply_spawned(
        id,
        SubAgentParent::Main,
        "x".into(),
        "t".into(),
        1,
        100,
        None,
    );
    let json = serde_json::to_value(&state).unwrap();
    let back: SessionState = serde_json::from_value(json).unwrap();
    assert_eq!(back.subagents.node(id).unwrap().label, "x");
}

fn queued(id: &str, text: &str) -> SessionDomainEvent {
    SessionDomainEvent::MessageQueued {
        id: id.to_string(),
        text: text.to_string(),
        at_ms: 0,
    }
}

use crate::sessions::orchestrator::MERGE_SEPARATOR;

fn fold(events: Vec<SessionDomainEvent>) -> SessionState {
    events
        .into_iter()
        .fold(SessionState::default(), SessionActor::apply_event)
}

/// What this actor's orchestrator decides for a state. `drain` used to be a
/// method here; the decision moved to the orchestrator and the actor only
/// performs it, so these tests assert on the decision.
fn decisions(actor: &SessionActor, state: &SessionState) -> Vec<AgentAction> {
    actor.orchestrator.next_actions(state)
}

/// A session is `Provisioning` from the moment its create is journaled
/// until the event that says how the create ended. Nothing else reaches
/// this status, and no turn can run inside it.
#[test]
fn a_created_session_provisions_before_it_is_idle() {
    let started = fold(vec![SessionDomainEvent::ProvisioningStarted { at_ms: 0 }]);
    assert_eq!(started.status, SessionStatus::Provisioning);

    let ready = fold(vec![
        SessionDomainEvent::ProvisioningStarted { at_ms: 0 },
        SessionDomainEvent::ProvisioningSucceeded { at_ms: 1 },
    ]);
    assert_eq!(ready.status, SessionStatus::Idle);
}

/// The message the session was created with waits in the inbox rather than
/// racing the vendor, and is still owed an answer once the runtime lands.
#[test]
fn a_message_sent_while_provisioning_waits_for_the_runtime() {
    let waiting = fold(vec![
        SessionDomainEvent::ProvisioningStarted { at_ms: 0 },
        queued("m1", "hello"),
    ]);
    assert_eq!(waiting.status, SessionStatus::Provisioning);
    assert_eq!(waiting.inbox.len(), 1, "the message is owed an answer");

    let ready = SessionActor::apply_event(
        waiting,
        SessionDomainEvent::ProvisioningSucceeded { at_ms: 2 },
    );
    assert_eq!(ready.status, SessionStatus::Idle);
    assert_eq!(ready.inbox.len(), 1, "still owed, now startable");
}

/// A create that failed on something retryable — an offline vendor, a
/// GitHub token that could not be minted — leaves a session that can try
/// again, and reports the reason the vendor actually gave rather than the
/// "no such runtime" a later `get` would have invented.
#[test]
fn a_retryable_create_failure_is_reported_verbatim() {
    let s = fold(vec![
        SessionDomainEvent::ProvisioningStarted { at_ms: 0 },
        SessionDomainEvent::ProvisioningFailed {
            at_ms: 1,
            error: "runtime vendor unavailable: vendor 'local' is not connected".into(),
            terminal: false,
        },
    ]);
    // Its own status, not the `Failed` a failed *turn* leaves. The two look
    // identical to a reader and mean opposite things to the session: a
    // failed turn has a runtime and can simply run again, while this one has
    // no runtime at all and must build one before it can do anything.
    assert_eq!(
        s.status,
        SessionStatus::ProvisioningFailed {
            reason: "runtime vendor unavailable: vendor 'local' is not connected".into(),
        }
    );
    assert!(s.last_error.is_some());
}

/// The status a failed create leaves must not let a turn start — the turn
/// would ask for a runtime that was never built and be told, terminally,
/// that it is gone. That is the whole defect in #239.
#[test]
fn a_failed_create_starts_no_turn() {
    let s = fold(vec![
        SessionDomainEvent::ProvisioningStarted { at_ms: 0 },
        SessionDomainEvent::ProvisioningFailed {
            at_ms: 1,
            error: "runtime vendor unavailable".into(),
            terminal: false,
        },
        queued("m1", "try again"),
    ]);
    assert!(
        InteractiveOrchestrator.next_actions(&s).is_empty(),
        "a session with no runtime must build one before it runs anything"
    );
    assert_eq!(s.inbox.len(), 1, "and the message is still owed an answer");
}

/// A live vendor refusing to build the runtime is the terminal case, and
/// the only one: it is the same `Gone` a `get` reports.
#[test]
fn a_terminal_create_failure_ends_the_session() {
    let s = fold(vec![
        SessionDomainEvent::ProvisioningStarted { at_ms: 0 },
        SessionDomainEvent::ProvisioningFailed {
            at_ms: 1,
            error: "runtime is gone: vendor cannot provision".into(),
            terminal: true,
        },
    ]);
    assert!(matches!(s.status, SessionStatus::Unrecoverable { .. }));
}

#[test]
fn a_fresh_session_is_idle_with_an_empty_inbox() {
    let s = SessionState::default();
    assert_eq!(s.status, SessionStatus::Idle);
    assert!(s.inbox.is_empty());
}

#[test]
fn queued_messages_accumulate_without_changing_status() {
    let s = fold(vec![queued("m1", "one"), queued("m2", "two")]);
    assert_eq!(s.status, SessionStatus::Idle, "queueing is not running");
    assert_eq!(s.inbox.len(), 2);
}

#[test]
fn a_turn_consumes_exactly_the_messages_it_names() {
    let s = fold(vec![
        queued("m1", "one"),
        queued("m2", "two"),
        SessionDomainEvent::TurnBegan {
            at_ms: 0,
            consumed: vec!["m1".into()],
            answering: None,
            answered: Vec::new(),
        },
        queued("m3", "three"),
    ]);
    assert_eq!(s.status, SessionStatus::Running);
    let ids: Vec<&str> = s.inbox.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["m2", "m3"],
        "a message that arrived after the turn began must still be owed an answer"
    );
}

#[test]
fn a_turn_that_answers_an_ask_clears_it() {
    // `answering` is how turns before multi-ask recorded it; a journal
    // written then must still fold to the same place.
    let s = fold(vec![
        SessionDomainEvent::AskRecorded {
            at_ms: 0,
            tool_call_id: Some("call-1".into()),
            question: "which branch?".into(),
        },
        queued("m1", "main"),
        SessionDomainEvent::TurnBegan {
            at_ms: 0,
            consumed: vec!["m1".into()],
            answering: Some("call-1".into()),
            answered: Vec::new(),
        },
    ]);
    assert_eq!(s.status, SessionStatus::Running);
    assert!(s.pending_asks.is_empty(), "the ask was answered");
}

#[test]
fn two_asks_in_one_turn_are_both_pending_until_a_turn_begins() {
    let asked = |id: &str, q: &str| SessionDomainEvent::AskRecorded {
        at_ms: 0,
        tool_call_id: Some(id.to_string()),
        question: q.to_string(),
    };
    let s = fold(vec![
        asked("call-1", "which branch?"),
        asked("call-2", "which model?"),
    ]);
    let SessionStatus::AwaitingInput { asks } = &s.status else {
        panic!("expected AwaitingInput, got {:?}", s.status);
    };
    assert_eq!(asks.len(), 2, "the status carries what must be answered");
    assert_eq!(asks[0].question, "which branch?");
    assert_eq!(asks[1].question, "which model?");
    assert_eq!(s.pending_asks.len(), 2);

    // Answered together, or abandoned together — either way the turn that
    // begins is the end of the park.
    let s = SessionActor::apply_event(
        s,
        SessionDomainEvent::TurnBegan {
            at_ms: 0,
            consumed: Vec::new(),
            answering: None,
            answered: vec!["call-1".into(), "call-2".into()],
        },
    );
    assert_eq!(s.status, SessionStatus::Running);
    assert!(s.pending_asks.is_empty());
}

#[test]
fn an_ask_survives_a_crash_so_the_answer_is_not_re_asked() {
    // TurnBegan is what clears the ask, and it is journaled with the
    // consumption in one step: a crash before it replays to "still asking".
    let s = fold(vec![
        SessionDomainEvent::AskRecorded {
            at_ms: 0,
            tool_call_id: Some("call-1".into()),
            question: "which branch?".into(),
        },
        queued("m1", "main"),
    ]);
    assert!(matches!(s.status, SessionStatus::AwaitingInput { .. }));
    assert_eq!(
        s.pending_asks
            .first()
            .and_then(|a| a.tool_call_id.as_deref()),
        Some("call-1")
    );
    assert_eq!(s.inbox.len(), 1, "the answer is still owed");
}

#[test]
fn stop_and_interrupt_both_land_idle_and_keep_the_inbox() {
    for boundary in [
        SessionDomainEvent::TurnStopped { at_ms: 0 },
        SessionDomainEvent::TurnInterrupted { at_ms: 0 },
    ] {
        let s = fold(vec![
            queued("m1", "one"),
            SessionDomainEvent::TurnBegan {
                at_ms: 0,
                consumed: vec!["m1".into()],
                answering: None,
                answered: Vec::new(),
            },
            queued("m2", "queued while running"),
            boundary,
        ]);
        assert_eq!(s.status, SessionStatus::Idle);
        assert_eq!(
            s.inbox.len(),
            1,
            "an accepted message is a promise; a stop cancels the turn, not the promise"
        );
    }
}

#[test]
fn a_failed_turn_is_sticky_but_not_terminal() {
    let s = fold(vec![
        queued("m1", "still owed an answer"),
        SessionDomainEvent::TurnFailed {
            at_ms: 0,
            error: "provider exploded".into(),
        },
    ]);
    assert!(matches!(s.status, SessionStatus::Failed { .. }));
    assert_eq!(s.last_error.as_deref(), Some("provider exploded"));
    assert_eq!(
        s.inbox.len(),
        1,
        "a turn that failed answered nothing; the queue is still owed"
    );

    // The next turn moves it straight back to Running.
    let s = SessionActor::apply_event(
        s,
        SessionDomainEvent::TurnBegan {
            at_ms: 0,
            consumed: vec![],
            answering: None,
            answered: Vec::new(),
        },
    );
    assert_eq!(s.status, SessionStatus::Running);
    // The detail endpoint reports `last_error`, so a turn that has just
    // started must not still be advertising the previous turn's failure.
    assert_eq!(s.last_error, None);
}

#[test]
fn a_gone_runtime_is_terminal() {
    let s = fold(vec![SessionDomainEvent::SessionFailed {
        at_ms: 0,
        reason: "vendor has no runtime".into(),
    }]);
    assert!(matches!(s.status, SessionStatus::Unrecoverable { .. }));
}

#[test]
fn usage_is_recorded_per_agent() {
    let s = fold(vec![SessionDomainEvent::UsageRecorded {
        at_ms: 0,
        agent_id: MAIN_AGENT_ID.to_string(),
        usage_total: UsageTotal {
            input_tokens: 10,
            output_tokens: 5,
            cache_creation_tokens: None,
            cache_read_tokens: None,
        },
    }]);
    assert_eq!(s.agent_usage.get(MAIN_AGENT_ID).unwrap().input_tokens, 10);
}

#[test]
fn subagent_events_fold_into_the_tree() {
    use crate::sessions::subagents::{SubAgentParent, SubAgentStatus};
    let id = Uuid::new_v4();
    let s = fold(vec![SessionDomainEvent::SubAgentSpawned {
        at_ms: 0,
        id,
        parent: SubAgentParent::Main,
        label: "research".into(),
        task: "look into it".into(),
        depth: 1,
        agent_type: None,
    }]);
    assert_eq!(s.subagents.active_count(), 1);

    let s = SessionActor::apply_event(
        s,
        SessionDomainEvent::SubAgentCompleted {
            at_ms: 0,
            id,
            output: "answer".into(),
        },
    );
    let rec = s.subagents.node(id).unwrap();
    assert_eq!(rec.status, SubAgentStatus::Completed);
    assert!(!rec.notified);

    let s = SessionActor::apply_event(s, SessionDomainEvent::SubAgentNotified { at_ms: 0, id });
    assert!(s.subagents.node(id).unwrap().notified);
}

#[test]
fn a_running_then_failed_subagent_reads_as_interrupted_then_terminal() {
    use crate::sessions::subagents::SubAgentParent;
    let id = Uuid::new_v4();
    let s = fold(vec![SessionDomainEvent::SubAgentSpawned {
        at_ms: 0,
        id,
        parent: SubAgentParent::Main,
        label: "w".into(),
        task: "t".into(),
        depth: 1,
        agent_type: None,
    }]);
    assert_eq!(s.subagents.interrupted(), vec![id]);
    let s = SessionActor::apply_event(
        s,
        SessionDomainEvent::SubAgentFailed {
            at_ms: 0,
            id,
            error: "interrupted by restart".into(),
        },
    );
    assert!(s.subagents.interrupted().is_empty());
}

#[test]
fn merging_joins_in_arrival_order_with_a_blank_line() {
    let s = fold(vec![queued("m1", "one"), queued("m2", "two")]);
    let merged = s
        .inbox
        .iter()
        .map(|m| m.text.as_str())
        .collect::<Vec<_>>()
        .join(MERGE_SEPARATOR);
    assert_eq!(merged, "one\n\ntwo");
}

#[test]
fn a_title_is_derived_from_the_first_line_only() {
    assert_eq!(derive_title("hello\nworld").as_deref(), Some("hello"));
    assert!(derive_title("   \n").is_none());
    let long = "x".repeat(TITLE_MAX_CHARS + 10);
    let title = derive_title(&long).unwrap();
    assert!(title.ends_with('…'));
    assert_eq!(title.chars().count(), TITLE_MAX_CHARS + 1);
}

// ── Actor-level coverage: `drain()` and `PrepareOffload`'s refuse-if-running
// branch. The rewrite that introduced the durable inbox dropped both.

fn actor_spec_fixture() -> SessionSpec {
    use crate::sessions::spec::WorkspaceDef;
    SessionSpec {
        name: Some("test".into()),
        agent: AgentSettings {
            model: "mock".into(),
            allowed_tools: None,
            use_plugins: None,
            max_iterations: None,
            max_retries: 0,
            mcp_servers: vec![],
            memory_spaces: vec![],
            thinking_effort: None,
            max_concurrent_subagents: None,
        },
        workspaces: vec![WorkspaceDef {
            name: "main".into(),
        }],
        provision: vec![],
        vendor: "mock".into(),
        plugins: vec![],
        origin: crate::sessions::spec::SessionOrigin::User,
        workflow: None,
    }
}

struct ActorFixture {
    deps: ServerDeps,
    agent: crate::runtime_vendor::fake::FakeRuntimeVendor,
    _tmp: tempfile::TempDir,
}

async fn actor_fixture() -> ActorFixture {
    actor_fixture_from(crate::runtime_vendor::fake::FakeRuntimeVendor::builder(
        "mock",
    ))
    .await
}

/// The same fixture over a fake told to hold its creates, so a test can put
/// a message underneath one that is genuinely in flight.
async fn actor_fixture_blocking_creates() -> ActorFixture {
    actor_fixture_from(
        crate::runtime_vendor::fake::FakeRuntimeVendor::builder("mock").block_creates(),
    )
    .await
}

async fn actor_fixture_from(
    builder: crate::runtime_vendor::fake::FakeRuntimeVendorBuilder,
) -> ActorFixture {
    let tmp = tempfile::tempdir().unwrap();
    let agent = builder.serve_in_process().await.expect("fake agent");
    let mut vendors = HashMap::new();
    vendors.insert("mock".to_string(), agent.link());
    let vendors = Arc::new(std::sync::RwLock::new(vendors));
    let deps = ServerDeps {
        runtimes: crate::runtime_manager::test_runtime_manager(&vendors),
        provider_registry: Arc::new(std::sync::RwLock::new(HashMap::new())),
        vendors,
        github_tokens: None,
        mcp: None,
        plugins: None,
        memory: None,
    };
    ActorFixture {
        deps,
        agent,
        _tmp: tmp,
    }
}

/// A supervisor stand-in for tests that spawn a bare `SessionActor`: it
/// answers nothing, and exists only so `report()`'s `.tell()` has a live
/// mailbox to land in.
struct DeafSupervisor;

#[async_trait]
impl EventSourcedActor for DeafSupervisor {
    type Command = SessionSupervisorCommand;
    type Event = ();
    type State = ();

    fn persistence_id(&self) -> PersistenceId {
        PersistenceId::new("test", "deaf-supervisor")
    }

    fn initial_state() {}

    fn apply_event((): (), (): ()) {}

    async fn handle_command(
        &mut self,
        (): &(),
        _cmd: SessionSupervisorCommand,
        _ctx: &mut ActorContext<Self>,
    ) -> CommandEffect<()> {
        CommandEffect::none()
    }
}

/// The frame channel a supervisor would hand the actor. Owned by the test,
/// exactly as the real one is owned by the supervisor rather than the actor.
fn spawn_deaf_supervisor() -> ActorRef<SessionSupervisorCommand> {
    horsie_actor::spawn_root(
        DeafSupervisor,
        Arc::new(horsie_actor::InMemoryJournal::new()),
    )
}

#[tokio::test]
async fn drain_does_nothing_when_the_inbox_is_empty() {
    let f = actor_fixture().await;
    let parent = spawn_deaf_supervisor();
    let actor = SessionActor::new(
        Uuid::new_v4(),
        actor_spec_fixture(),
        f.deps,
        parent,
        crate::sessions::Positions::default(),
    );
    let actions = decisions(&actor, &SessionState::default());
    assert!(actions.is_empty());
}

#[tokio::test]
async fn drain_does_nothing_while_a_turn_is_already_running() {
    let f = actor_fixture().await;
    let parent = spawn_deaf_supervisor();
    let actor = SessionActor::new(
        Uuid::new_v4(),
        actor_spec_fixture(),
        f.deps,
        parent,
        crate::sessions::Positions::default(),
    );
    let state = fold(vec![
        queued("m1", "one"),
        SessionDomainEvent::TurnBegan {
            at_ms: 0,
            consumed: vec!["m1".into()],
            answering: None,
            answered: Vec::new(),
        },
        queued("m2", "queued while running"),
    ]);
    let actions = decisions(&actor, &state);
    assert!(
        actions.is_empty(),
        "a run in flight must never be drained into a second one"
    );
}

#[tokio::test]
async fn drain_refuses_once_the_session_is_unrecoverable() {
    let f = actor_fixture().await;
    let parent = spawn_deaf_supervisor();
    let actor = SessionActor::new(
        Uuid::new_v4(),
        actor_spec_fixture(),
        f.deps,
        parent,
        crate::sessions::Positions::default(),
    );
    let state = fold(vec![
        queued("m1", "one"),
        SessionDomainEvent::SessionFailed {
            at_ms: 0,
            reason: "runtime gone".into(),
        },
    ]);
    let actions = decisions(&actor, &state);
    assert!(
        actions.is_empty(),
        "a terminal session must never start another turn"
    );
}

/// A failed turn is a turn boundary that deliberately does *not* drain. The
/// cause is usually stuck — an expired key, a dead vendor — and draining
/// would turn three queued messages into three back-to-back failures the
/// user never asked for. The next message they send drains them.
#[tokio::test]
async fn a_failed_turn_does_not_drain() {
    let f = actor_fixture().await;
    let parent = spawn_deaf_supervisor();
    let id = Uuid::new_v4();
    let journal: Arc<dyn horsie_actor::Journal> = Arc::new(horsie_actor::InMemoryJournal::new());
    // A turn is running, and a message arrived while it was.
    let prior = [
        queued("m1", "one"),
        SessionDomainEvent::TurnBegan {
            at_ms: 0,
            consumed: vec!["m1".into()],
            answering: None,
            answered: Vec::new(),
        },
        queued("m2", "queued while running"),
    ];
    let bytes: Vec<Vec<u8>> = prior
        .iter()
        .map(|e| serde_json::to_vec(e).unwrap())
        .collect();
    journal
        .persist(&SessionActor::persistence_id_for(id), &bytes)
        .await
        .unwrap();
    let session = horsie_actor::spawn_root(
        SessionActor::new(
            id,
            actor_spec_fixture(),
            f.deps,
            parent,
            crate::sessions::Positions::default(),
        ),
        journal.clone(),
    );
    // Recovery reconciles the interrupted turn first (event 4); wait for
    // that to settle so the failure is the only thing left to observe.
    wait_for_journal_len(&journal, id, 4).await;

    session
        .tell(SessionCommand::AgentOutcome(AgentOutcome::Failed {
            session_id: id,
            error: "provider exploded".into(),
            recoverable: true,
            terminal: false,
        }))
        .await
        .unwrap();

    // The failure lands (event 5) — and nothing follows: no drain into a
    // back-to-back failure.
    wait_for_journal_len(&journal, id, 5).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        session_journal_len(&journal, id).await,
        5,
        "a failed turn records the failure and nothing else"
    );
    // Asked of the actor, which is the only thing that reads this journal.
    let snapshot = session
        .ask(|reply| SessionCommand::Snapshot { reply })
        .await
        .unwrap();
    assert!(matches!(
        snapshot.status,
        crate::sessions::spec::SessionStatus::Failed { .. }
    ));
    assert_eq!(snapshot.inbox.len(), 1, "the queued message is still owed");
}

/// Stop is a turn boundary like any other: it cancels the turn, not the
/// promise. Whatever was queued while the cancelled turn ran starts the
/// next one immediately — which is exactly why the client marks queued
/// messages as unread, so that next turn does not look self-inflicted.
#[tokio::test]
async fn stop_then_a_queued_message_starts_the_next_turn() {
    let f = actor_fixture().await;
    let parent = spawn_deaf_supervisor();
    let actor = SessionActor::new(
        Uuid::new_v4(),
        actor_spec_fixture(),
        f.deps,
        parent,
        crate::sessions::Positions::default(),
    );
    let running = fold(vec![
        queued("m1", "one"),
        SessionDomainEvent::TurnBegan {
            at_ms: 0,
            consumed: vec!["m1".into()],
            answering: None,
            answered: Vec::new(),
        },
        queued("m2", "queued while running"),
    ]);

    let stopped = SessionActor::apply_event(running, SessionDomainEvent::TurnStopped { at_ms: 0 });
    assert_eq!(stopped.status, SessionStatus::Idle);
    let actions = decisions(&actor, &stopped);
    assert_eq!(actions.len(), 1, "{actions:?}");
    let AgentAction::StartTurn { consumed, .. } = &actions[0] else {
        panic!("an interactive session starts turns, not steps");
    };
    assert_eq!(consumed, &vec!["m2".to_string()]);
}

#[tokio::test]
async fn drain_consumes_the_whole_inbox_and_starts_a_turn() {
    let f = actor_fixture().await;
    let parent = spawn_deaf_supervisor();
    let actor = SessionActor::new(
        Uuid::new_v4(),
        actor_spec_fixture(),
        f.deps,
        parent,
        crate::sessions::Positions::default(),
    );
    let state = fold(vec![queued("m1", "one"), queued("m2", "two")]);
    let actions = decisions(&actor, &state);
    assert_eq!(actions.len(), 1);
    let AgentAction::StartTurn { consumed, .. } = &actions[0] else {
        panic!("an interactive session starts turns, not steps");
    };
    assert_eq!(consumed, &vec!["m1".to_string(), "m2".to_string()]);
}

#[tokio::test]
async fn drain_abandons_pending_asks_rather_than_answering_them() {
    let f = actor_fixture().await;
    let parent = spawn_deaf_supervisor();
    let actor = SessionActor::new(
        Uuid::new_v4(),
        actor_spec_fixture(),
        f.deps,
        parent,
        crate::sessions::Positions::default(),
    );
    let state = fold(vec![
        SessionDomainEvent::AskRecorded {
            at_ms: 0,
            tool_call_id: Some("call-1".into()),
            question: "which?".into(),
        },
        queued("m1", "main"),
    ]);
    let actions = decisions(&actor, &state);
    assert_eq!(actions.len(), 1);
    let AgentAction::StartTurn {
        consumed,
        answered,
        input,
        ..
    } = &actions[0]
    else {
        panic!("an interactive session starts turns, not steps");
    };
    assert_eq!(consumed, &vec!["m1".to_string()]);
    assert_eq!(
        input.results.len(),
        1,
        "the parked call still gets a result"
    );
    assert!(input.results[0].is_error);
    assert!(
        answered.is_empty(),
        "a plain message abandons the question rather than answering it — \
         answers come through `Answer`, which requires all of them at once"
    );
}

/// A session parked on two questions, with an actor to answer them on.
async fn parked_on_two_asks() -> (SessionActor, SessionState) {
    let f = actor_fixture().await;
    let parent = spawn_deaf_supervisor();
    let actor = SessionActor::new(
        Uuid::new_v4(),
        actor_spec_fixture(),
        f.deps,
        parent,
        crate::sessions::Positions::default(),
    );
    let state = fold(vec![
        SessionDomainEvent::AskRecorded {
            at_ms: 0,
            tool_call_id: Some("call-1".into()),
            question: "which branch?".into(),
        },
        SessionDomainEvent::AskRecorded {
            at_ms: 0,
            tool_call_id: Some("call-2".into()),
            question: "which model?".into(),
        },
    ]);
    (actor, state)
}

fn answer(id: &str, text: &str) -> AskAnswer {
    AskAnswer {
        tool_call_id: id.to_string(),
        text: text.to_string(),
    }
}

#[tokio::test]
async fn a_partial_answer_set_is_refused_and_journals_nothing() {
    // Resuming on half the answers would send the provider a `tool_use` with
    // no result, which is exactly the 400 this whole change exists to stop.
    let (mut actor, state) = parked_on_two_asks().await;
    let (tx, rx) = oneshot::channel();

    let effect = actor
        .on_answer(&state, vec![answer("call-1", "main")], tx)
        .await;

    assert!(
        effect.events().is_empty(),
        "a refused answer set changes nothing"
    );
    match rx.await.unwrap() {
        Err(AnswerError::Incomplete {
            missing,
            unexpected,
        }) => {
            assert_eq!(missing, vec!["call-2".to_string()]);
            assert!(unexpected.is_empty());
        }
        other => panic!("expected Incomplete, got {other:?}"),
    }
}

#[tokio::test]
async fn an_answer_for_a_call_that_is_not_pending_is_refused() {
    let (mut actor, state) = parked_on_two_asks().await;
    let (tx, rx) = oneshot::channel();

    let effect = actor
        .on_answer(
            &state,
            vec![
                answer("call-1", "main"),
                answer("call-2", "kimi"),
                answer("call-9", "who asked?"),
            ],
            tx,
        )
        .await;

    assert!(effect.events().is_empty());
    match rx.await.unwrap() {
        Err(AnswerError::Incomplete { unexpected, .. }) => {
            assert_eq!(unexpected, vec!["call-9".to_string()]);
        }
        other => panic!("expected Incomplete, got {other:?}"),
    }
}

#[tokio::test]
async fn a_complete_answer_set_begins_a_turn_naming_every_ask() {
    let (mut actor, state) = parked_on_two_asks().await;
    let (tx, rx) = oneshot::channel();

    let effect = actor
        .on_answer(
            &state,
            vec![answer("call-1", "main"), answer("call-2", "kimi")],
            tx,
        )
        .await;

    assert!(rx.await.unwrap().is_ok());
    let events = effect.events();
    assert_eq!(events.len(), 1);
    let SessionDomainEvent::TurnBegan {
        consumed, answered, ..
    } = &events[0]
    else {
        panic!("expected TurnBegan, got {:?}", events[0]);
    };
    assert!(consumed.is_empty(), "an answer consumes no queued message");
    let mut answered = answered.clone();
    answered.sort();
    assert_eq!(answered, vec!["call-1".to_string(), "call-2".to_string()]);

    // And the park is over: folding the event clears every pending ask.
    let next = SessionActor::apply_event(state, events[0].clone());
    assert!(next.pending_asks.is_empty());
    assert_eq!(next.status, SessionStatus::Running);
}

#[tokio::test]
async fn answering_a_session_that_is_not_parked_is_refused() {
    let f = actor_fixture().await;
    let parent = spawn_deaf_supervisor();
    let mut actor = SessionActor::new(
        Uuid::new_v4(),
        actor_spec_fixture(),
        f.deps,
        parent,
        crate::sessions::Positions::default(),
    );
    let (tx, rx) = oneshot::channel();

    let effect = actor
        .on_answer(&SessionState::default(), vec![answer("call-1", "main")], tx)
        .await;

    assert!(effect.events().is_empty());
    assert_eq!(rx.await.unwrap(), Err(AnswerError::NothingPending));
}

/// An `LlmProvider` that hangs until released, so a test can hold a run
/// genuinely `Running` for as long as it needs to.
struct BlockingProvider {
    gate: tokio::sync::Notify,
}

impl BlockingProvider {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            gate: tokio::sync::Notify::new(),
        })
    }

    fn release(&self) {
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

#[tokio::test]
async fn prepare_offload_refuses_while_a_run_is_in_flight() {
    let f = actor_fixture().await;
    let id = Uuid::new_v4();
    f.deps
        .runtimes
        .create(&id.to_string(), "mock", &actor_spec_fixture())
        .await
        .expect("create");
    let provider = BlockingProvider::new();
    f.deps
        .provider_registry
        .write()
        .unwrap()
        .insert("mock".to_string(), provider.clone() as Arc<dyn LlmProvider>);

    let parent = spawn_deaf_supervisor();
    let journal: Arc<dyn horsie_actor::Journal> = Arc::new(horsie_actor::InMemoryJournal::new());
    let session = horsie_actor::spawn_root(
        SessionActor::new(
            id,
            actor_spec_fixture(),
            f.deps.clone(),
            parent,
            crate::sessions::Positions::default(),
        ),
        journal,
    );

    session
        .ask(|reply| SessionCommand::UserMessage {
            text: "go".into(),
            reply,
        })
        .await
        .unwrap()
        .unwrap();

    let offloadable = session
        .ask(|reply| SessionCommand::PrepareOffload { reply })
        .await
        .unwrap();
    assert!(
        !offloadable,
        "a run in flight must never be offloaded out from under itself"
    );
    assert!(
        f.agent
            .signals()
            .iter()
            .all(|s| !s.starts_with("hibernate:")),
        "refusing must not touch the runtime: {:?}",
        f.agent.signals()
    );

    // Refusing must leave the actor exactly as it was, still answering
    // commands normally rather than having torn itself down.
    provider.release();
    let (tx, rx) = oneshot::channel();
    session
        .tell(SessionCommand::UsageStats { reply: tx })
        .await
        .unwrap();
    rx.await.unwrap();
}

/// A provider whose every call immediately ends the turn with plain text.
struct EchoProvider;

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

async fn spawn_session_with_provider(
    provider: Arc<dyn LlmProvider>,
) -> (
    ActorFixture,
    ActorRef<SessionCommand>,
    Uuid,
    Arc<dyn horsie_actor::Journal>,
) {
    let f = actor_fixture().await;
    let id = Uuid::new_v4();
    f.deps
        .runtimes
        .create(&id.to_string(), "mock", &actor_spec_fixture())
        .await
        .expect("create");
    f.deps
        .provider_registry
        .write()
        .unwrap()
        .insert("mock".to_string(), provider);
    let parent = spawn_deaf_supervisor();
    let journal: Arc<dyn horsie_actor::Journal> = Arc::new(horsie_actor::InMemoryJournal::new());
    let session = horsie_actor::spawn_root(
        SessionActor::new(
            id,
            actor_spec_fixture(),
            f.deps.clone(),
            parent,
            crate::sessions::Positions::default(),
        ),
        journal.clone(),
    );
    (f, session, id, journal)
}

/// A two-step run: `triage` branches on its output to `fix` or `file`.
fn run_spec_fixture(input: &str) -> crate::sessions::workflow::WorkflowRunSpec {
    use crate::sessions::workflow::{TransitionSpec, WorkflowRunSpec, WorkflowStepSpec};
    let settings = |()| AgentSettings {
        model: "mock".into(),
        allowed_tools: None,
        use_plugins: None,
        max_iterations: None,
        max_retries: 0,
        mcp_servers: vec![],
        memory_spaces: vec![],
        thinking_effort: None,
        max_concurrent_subagents: None,
    };
    WorkflowRunSpec {
        workflow: "fix-bug".into(),
        start: "triage".into(),
        steps: vec![
            WorkflowStepSpec {
                name: "triage".into(),
                agent: "triager".into(),
                prompt: "Triage it.".into(),
                output_schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": {"severity": {"type": "string"}}
                })),
                transitions: vec![
                    TransitionSpec {
                        to: "fix".into(),
                        condition: Some("output.severity == \"p0\"".into()),
                    },
                    TransitionSpec {
                        to: "file".into(),
                        condition: None,
                    },
                ],
                settings: settings(()),
            },
            WorkflowStepSpec {
                name: "fix".into(),
                agent: "coder".into(),
                prompt: "Fix it.".into(),
                output_schema: None,
                transitions: vec![],
                settings: settings(()),
            },
            WorkflowStepSpec {
                name: "file".into(),
                agent: "writer".into(),
                prompt: "File it.".into(),
                output_schema: None,
                transitions: vec![],
                settings: settings(()),
            },
        ],
        input: input.to_string(),
        max_steps: 100,
    }
}

/// A session that is a run of [`run_spec_fixture`], on a scripted provider.
async fn spawn_run_with_provider(
    provider: Arc<dyn LlmProvider>,
) -> (
    ActorFixture,
    ActorRef<SessionCommand>,
    Uuid,
    Arc<dyn horsie_actor::Journal>,
) {
    let f = actor_fixture().await;
    let id = Uuid::new_v4();
    let mut spec = actor_spec_fixture();
    spec.origin = crate::sessions::spec::SessionOrigin::Workflow {
        workflow: "fix-bug".into(),
    };
    spec.workflow = Some(Arc::new(run_spec_fixture("the build is red")));
    f.deps
        .runtimes
        .create(&id.to_string(), "mock", &spec)
        .await
        .expect("create");
    f.deps
        .provider_registry
        .write()
        .unwrap()
        .insert("mock".to_string(), provider);
    let parent = spawn_deaf_supervisor();
    let journal: Arc<dyn horsie_actor::Journal> = Arc::new(horsie_actor::InMemoryJournal::new());
    let session = horsie_actor::spawn_root(
        SessionActor::new(
            id,
            spec,
            f.deps.clone(),
            parent,
            crate::sessions::Positions::default(),
        ),
        journal.clone(),
    );
    (f, session, id, journal)
}

/// Poll the folded run until `pred` holds (2s cap).
async fn wait_for_run(
    journal: &Arc<dyn horsie_actor::Journal>,
    session_id: Uuid,
    pred: impl Fn(&crate::sessions::workflow::WorkflowRunState) -> bool,
) -> crate::sessions::workflow::WorkflowRunState {
    for _ in 0..200 {
        let state = crate::sessions::events::fold_session_state(journal, session_id).await;
        if let Some(run) = state.run.as_ref()
            && pred(run)
        {
            return run.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let state = crate::sessions::events::fold_session_state(journal, session_id).await;
    panic!(
        "run never satisfied the predicate: {:?}",
        state.run.as_ref()
    );
}

/// A scripted `conclude` call carrying this output.
///
/// A step that has an output schema *and* may ask gets the kind-tagged
/// conclude schema, so the output nests under `output` rather than being
/// the payload — sending it bare submits `null`, and every condition reads
/// false.
fn concludes(output: serde_json::Value) -> horsie_agentcore::CompletionResponse {
    horsie_agentcore::CompletionResponse {
        parts: vec![horsie_agentcore::ContentPart::ToolCall(
            horsie_agentcore::ToolCallPart {
                id: "c-1".into(),
                name: "conclude".into(),
                input: serde_json::json!({"kind": "submit", "output": output}),
            },
        )],
        stop_reason: horsie_agentcore::StopReason::ToolUse,
        usage: horsie_agentcore::Usage::without_cache(1, 1),
    }
}

/// The whole point: a run starts itself, its first step's output picks the
/// branch, and the branch's step ends the run.
#[tokio::test]
async fn a_run_starts_itself_and_routes_on_its_first_steps_output() {
    use horsie_agentcore::testkit::{MockProvider, Script};
    let provider = MockProvider::scripted(
        Script::of([Ok(concludes(serde_json::json!({"severity": "p0"})))]).then_repeating_with(
            || {
                Ok(horsie_agentcore::CompletionResponse {
                    parts: vec![horsie_agentcore::ContentPart::Text(
                        horsie_agentcore::TextPart {
                            text: "fixed".to_string(),
                        },
                    )],
                    stop_reason: horsie_agentcore::StopReason::EndTurn,
                    usage: horsie_agentcore::Usage::without_cache(1, 1),
                })
            },
        ),
    );
    let (_f, _session, id, journal) = spawn_run_with_provider(provider).await;

    // Nobody sent a message: creating the run is what starts it.
    let run = wait_for_run(&journal, id, |r| {
        r.status == crate::sessions::workflow::WorkflowRunStatus::Finished
    })
    .await;

    let visited: Vec<&str> = run.steps.iter().map(|s| s.step.as_str()).collect();
    assert_eq!(
        visited,
        vec!["triage", "fix"],
        "p0 must route to `fix`; triage concluded with {:?}",
        run.steps[0].output
    );
    // The condition that matched is recorded, which is what draws the edge.
    assert_eq!(
        run.steps[1].via.as_deref(),
        Some("output.severity == \"p0\"")
    );
    assert_eq!(run.steps[1].from, Some(0));
    // Each step is its own agent, derived from the session and the index.
    assert_eq!(
        run.steps[0].agent,
        crate::sessions::workflow::WorkflowRunSpec::step_agent_id(id, 0)
    );
    assert_ne!(run.steps[0].agent, run.steps[1].agent);
    // The second step was handed the first's output under a header.
    assert!(
        run.steps[1].input.contains("## Input from step `triage`"),
        "{}",
        run.steps[1].input
    );
    assert!(run.steps[1].input.starts_with("Fix it."));
}

/// The `else` branch, and the run's output being the last step's.
#[tokio::test]
async fn a_non_matching_condition_takes_the_catch_all() {
    use horsie_agentcore::testkit::{MockProvider, Script};
    let provider = MockProvider::scripted(
        Script::of([Ok(concludes(serde_json::json!({"severity": "p2"})))]).then_repeating_with(
            || {
                Ok(horsie_agentcore::CompletionResponse {
                    parts: vec![horsie_agentcore::ContentPart::Text(
                        horsie_agentcore::TextPart {
                            text: "filed".to_string(),
                        },
                    )],
                    stop_reason: horsie_agentcore::StopReason::EndTurn,
                    usage: horsie_agentcore::Usage::without_cache(1, 1),
                })
            },
        ),
    );
    let (_f, _session, id, journal) = spawn_run_with_provider(provider).await;
    let run = wait_for_run(&journal, id, |r| {
        r.status == crate::sessions::workflow::WorkflowRunStatus::Finished
    })
    .await;
    let visited: Vec<&str> = run.steps.iter().map(|s| s.step.as_str()).collect();
    assert_eq!(visited, vec!["triage", "file"]);
    assert!(run.steps[1].via.is_none());
}

/// A run works from its definition; there is nobody to send a message to.
#[tokio::test]
async fn a_run_refuses_a_user_message() {
    use horsie_agentcore::testkit::{MockProvider, Script};
    let provider = MockProvider::scripted(Script::of([Ok(concludes(
        serde_json::json!({"severity": "p0"}),
    ))]));
    let (_f, session, _id, _journal) = spawn_run_with_provider(provider).await;
    let err = session
        .ask(|reply| SessionCommand::UserMessage {
            text: "hello".into(),
            reply,
        })
        .await
        .unwrap()
        .unwrap_err();
    assert!(matches!(err, UserMessageError::Rejected(_)), "{err:?}");
}

/// Retrying appends an attempt rather than replacing one, so the earlier
/// attempt stays readable and the graph can stack them.
#[tokio::test]
async fn retrying_a_step_appends_an_attempt_on_the_same_edge() {
    use horsie_agentcore::testkit::{MockProvider, Script};
    let provider = MockProvider::scripted(
        Script::of([Ok(concludes(serde_json::json!({"severity": "p0"})))]).then_repeating_with(
            || {
                Ok(horsie_agentcore::CompletionResponse {
                    parts: vec![horsie_agentcore::ContentPart::Text(
                        horsie_agentcore::TextPart {
                            text: "fixed".to_string(),
                        },
                    )],
                    stop_reason: horsie_agentcore::StopReason::EndTurn,
                    usage: horsie_agentcore::Usage::without_cache(1, 1),
                })
            },
        ),
    );
    let (_f, session, id, journal) = spawn_run_with_provider(provider).await;
    wait_for_run(&journal, id, |r| {
        r.status == crate::sessions::workflow::WorkflowRunStatus::Finished
    })
    .await;

    session
        .ask(|reply| SessionCommand::RetryStep { index: 1, reply })
        .await
        .unwrap()
        .unwrap();
    let run = wait_for_run(&journal, id, |r| r.steps.len() == 3).await;
    assert_eq!(run.steps[2].step, "fix");
    assert_eq!(run.steps[2].attempt, 2, "the retry numbers itself");
    // It sits where the original sat, so it draws on the same edge.
    assert_eq!(run.steps[2].from, run.steps[1].from);
    assert_eq!(run.steps[2].via, run.steps[1].via);
    // The first attempt is untouched.
    assert_eq!(
        run.steps[1].status,
        crate::sessions::workflow::StepStatus::Concluded
    );
}

/// Poll the session's folded state until the tree satisfies `pred` (2s
/// cap). Subagent progress is journal-first, so the fold is the honest
/// thing to wait on.
async fn wait_for_tree(
    journal: &Arc<dyn horsie_actor::Journal>,
    session_id: Uuid,
    pred: impl Fn(&crate::sessions::subagents::SubAgentForest) -> bool,
) {
    for _ in 0..200 {
        let state = crate::sessions::events::fold_session_state(journal, session_id).await;
        if pred(&state.subagents) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("tree condition not met within 2s");
}

/// Spawn a session actor over a fresh journal, provisioning nothing. The
/// session owns its create now, so a test that wants a runtime asks for one.
fn spawn_unprovisioned(
    f: &ActorFixture,
    id: Uuid,
) -> (ActorRef<SessionCommand>, Arc<dyn horsie_actor::Journal>) {
    let journal: Arc<dyn horsie_actor::Journal> = Arc::new(horsie_actor::InMemoryJournal::new());
    let session = horsie_actor::spawn_root(
        SessionActor::new(
            id,
            actor_spec_fixture(),
            f.deps.clone(),
            spawn_deaf_supervisor(),
            crate::sessions::Positions::default(),
        ),
        journal.clone(),
    );
    (session, journal)
}

/// The bug in one test: a message that arrives while the create is still in
/// flight must queue, not ask a vendor that has never heard of the runtime.
///
/// The create is held open for the whole window, so this is a statement
/// about the design and not about scheduling luck — and the wait is the
/// session's own journaled status, which is what makes it survive a restart
/// where an in-memory gate would not.
#[tokio::test]
async fn a_message_arriving_mid_create_waits_for_the_runtime() {
    let f = actor_fixture_blocking_creates().await;
    f.deps.provider_registry.write().unwrap().insert(
        "mock".to_string(),
        Arc::new(EchoProvider) as Arc<dyn LlmProvider>,
    );
    let id = Uuid::new_v4();
    let (session, journal) = spawn_unprovisioned(&f, id);

    session.tell(SessionCommand::Provision).await.unwrap();
    let (tx, _rx) = oneshot::channel();
    session
        .tell(SessionCommand::UserMessage {
            text: "hello".into(),
            reply: tx,
        })
        .await
        .unwrap();

    let waiting = wait_for_state(&journal, id, "a queued message under a live create", |s| {
        s.status == SessionStatus::Provisioning && !s.inbox.is_empty()
    })
    .await;
    assert_eq!(
        waiting.inbox.len(),
        1,
        "the message is owed an answer, not spent on a runtime that does not exist"
    );
    assert!(
        !f.agent.signals().iter().any(|s| s.starts_with("get:")),
        "nothing may ask the vendor for a runtime it has not been told to build"
    );

    f.agent.release_creates();
    wait_for_state(&journal, id, "the queued message running", |s| {
        s.inbox.is_empty() && s.status != SessionStatus::Provisioning
    })
    .await;
}

/// The capability an in-memory gate cannot have: a create the process died
/// inside is finished by the next incarnation.
///
/// Re-attempting is safe here and only here — `Provisioning` means no turn
/// has ever run, so there is no work in the workspace to destroy.
#[tokio::test]
async fn a_create_interrupted_by_a_restart_is_re_attempted_at_load() {
    let f = actor_fixture().await;
    let id = Uuid::new_v4();
    // A journal that stops at `ProvisioningStarted` is exactly what a
    // process killed mid-create leaves behind. Seeded rather than produced
    // by a first incarnation, because the detached create holds a reference
    // to the actor it reports to — so dropping a handle is not death.
    let journal: Arc<dyn horsie_actor::Journal> = Arc::new(horsie_actor::InMemoryJournal::new());
    journal
        .persist(
            &SessionActor::persistence_id_for(id),
            &[
                serde_json::to_vec(&SessionDomainEvent::ProvisioningStarted { at_ms: 0 }).unwrap(),
                serde_json::to_vec(&queued("m1", "hello")).unwrap(),
            ],
        )
        .await
        .unwrap();

    let _session2 = horsie_actor::spawn_root(
        SessionActor::new(
            id,
            actor_spec_fixture(),
            f.deps.clone(),
            spawn_deaf_supervisor(),
            crate::sessions::Positions::default(),
        ),
        journal.clone(),
    );
    wait_for_state(&journal, id, "the runtime finished after a restart", |s| {
        s.status != SessionStatus::Provisioning
    })
    .await;
    assert!(
        f.agent
            .signals()
            .iter()
            .any(|s| s == &format!("create:{id}")),
        "the interrupted create has to be finished by somebody"
    );
}

/// A run has no first message to hold it back — `AdvanceRun` fires at load
/// and starts step one by itself. So it needs the same wait a conversation
/// gets, and for the same reason: the step would ask for a runtime nobody
/// had been told to build.
#[tokio::test]
async fn a_runs_first_step_waits_for_the_create_too() {
    let f = actor_fixture_blocking_creates().await;
    f.deps.provider_registry.write().unwrap().insert(
        "mock".to_string(),
        Arc::new(EchoProvider) as Arc<dyn LlmProvider>,
    );
    let id = Uuid::new_v4();
    let mut spec = actor_spec_fixture();
    spec.origin = crate::sessions::spec::SessionOrigin::Workflow {
        workflow: "fix-bug".into(),
    };
    spec.workflow = Some(Arc::new(run_spec_fixture("the build is red")));
    let journal: Arc<dyn horsie_actor::Journal> = Arc::new(horsie_actor::InMemoryJournal::new());
    let session = horsie_actor::spawn_root(
        SessionActor::new(
            id,
            spec,
            f.deps.clone(),
            spawn_deaf_supervisor(),
            crate::sessions::Positions::default(),
        ),
        journal.clone(),
    );
    session.tell(SessionCommand::Provision).await.unwrap();

    wait_for_state(&journal, id, "a run holding at its create", |s| {
        s.status == SessionStatus::Provisioning
    })
    .await;
    let held = crate::sessions::events::fold_session_state(&journal, id).await;
    assert!(
        held.run.as_ref().is_none_or(|r| r.steps.is_empty()),
        "no step may start before the runtime it would run on"
    );

    f.agent.release_creates();
    wait_for_run(&journal, id, |r| !r.steps.is_empty()).await;
}

/// #239: the message that retries a failed create has to *build* the
/// runtime, not ask for one that was never built.
///
/// The vendor is missing for the create and present for the retry, which is
/// the canonical retryable failure — a laptop agent that was offline for a
/// moment must not cost a session permanently.
#[tokio::test]
async fn a_message_after_a_failed_create_provisions_instead_of_dying() {
    let f = actor_fixture().await;
    f.deps.provider_registry.write().unwrap().insert(
        "mock".to_string(),
        Arc::new(EchoProvider) as Arc<dyn LlmProvider>,
    );
    let link = f
        .deps
        .vendors
        .write()
        .unwrap()
        .remove("mock")
        .expect("the fixture registers one");

    let id = Uuid::new_v4();
    let (session, journal) = spawn_unprovisioned(&f, id);
    session.tell(SessionCommand::Provision).await.unwrap();
    let failed = wait_for_state(
        &journal,
        id,
        "a create that could not reach a vendor",
        |s| matches!(s.status, SessionStatus::ProvisioningFailed { .. }),
    )
    .await;
    assert!(
        failed
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("unavailable")),
        "the vendor's own reason survives: {:?}",
        failed.last_error
    );

    // The vendor comes back, and the user does what the UI tells them to.
    f.deps
        .vendors
        .write()
        .unwrap()
        .insert("mock".to_string(), link);
    let (tx, _rx) = oneshot::channel();
    session
        .tell(SessionCommand::UserMessage {
            text: "try again".into(),
            reply: tx,
        })
        .await
        .unwrap();

    wait_for_state(&journal, id, "the retry building a runtime", |s| {
        !matches!(s.status, SessionStatus::ProvisioningFailed { .. })
    })
    .await;
    assert!(
        f.agent
            .signals()
            .iter()
            .any(|s| s == &format!("create:{id}")),
        "the retry has to build the runtime, not ask for one: {:?}",
        f.agent.signals()
    );
    let ran = wait_for_state(&journal, id, "the queued message running", |s| {
        s.inbox.is_empty()
    })
    .await;
    assert!(
        !matches!(ran.status, SessionStatus::Unrecoverable { .. }),
        "retrying must never be what kills the session: {:?}",
        ran.status
    );
}

/// A workflow run takes no messages, so the message-shaped retry can never
/// reach one. Without this it would sit in `ProvisioningFailed` forever —
/// where before it at least died — so loading a session whose create failed
/// re-attempts it, which is also how a run gets a second chance at all.
#[tokio::test]
async fn loading_a_session_whose_create_failed_re_attempts_it() {
    let f = actor_fixture().await;
    let id = Uuid::new_v4();
    let journal: Arc<dyn horsie_actor::Journal> = Arc::new(horsie_actor::InMemoryJournal::new());
    journal
        .persist(
            &SessionActor::persistence_id_for(id),
            &[
                serde_json::to_vec(&SessionDomainEvent::ProvisioningStarted { at_ms: 0 }).unwrap(),
                serde_json::to_vec(&SessionDomainEvent::ProvisioningFailed {
                    at_ms: 1,
                    error: "runtime vendor unavailable: vendor 'mock' is not connected".into(),
                    terminal: false,
                })
                .unwrap(),
            ],
        )
        .await
        .unwrap();

    let _session = horsie_actor::spawn_root(
        SessionActor::new(
            id,
            actor_spec_fixture(),
            f.deps.clone(),
            spawn_deaf_supervisor(),
            crate::sessions::Positions::default(),
        ),
        journal.clone(),
    );
    wait_for_state(&journal, id, "the create re-attempted at load", |s| {
        !matches!(s.status, SessionStatus::ProvisioningFailed { .. })
    })
    .await;
    assert!(
        f.agent
            .signals()
            .iter()
            .any(|s| s == &format!("create:{id}")),
        "the runtime has to actually get built: {:?}",
        f.agent.signals()
    );
}

/// Poll the folded session state until it satisfies `pred`.
async fn wait_for_state(
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
    panic!("{what} not reached within 2s");
}

/// Entry count of the session's own journal (`session/<id>`), not the
/// agent's.
async fn session_journal_len(journal: &Arc<dyn horsie_actor::Journal>, session_id: Uuid) -> u64 {
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
async fn wait_for_journal_len(journal: &Arc<dyn horsie_actor::Journal>, session_id: Uuid, n: u64) {
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
struct CountingJournal {
    inner: horsie_actor::InMemoryJournal,
    replays: std::sync::atomic::AtomicUsize,
}

impl CountingJournal {
    fn new() -> Self {
        Self {
            inner: horsie_actor::InMemoryJournal::new(),
            replays: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn replays(&self) -> usize {
        self.replays.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl horsie_actor::Journal for CountingJournal {
    async fn persist(
        &self,
        pid: &horsie_actor::PersistenceId,
        events: &[Vec<u8>],
    ) -> horsie_actor::JournalResult<()> {
        self.inner.persist(pid, events).await
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

    async fn clear(&self, pid: &horsie_actor::PersistenceId) -> horsie_actor::JournalResult<()> {
        self.inner.clear(pid).await
    }
}

/// The invariant the old two-vocabulary design could not even state, now
/// nearly a tautology: reading forward and paging backwards return the same
/// entries, in the same order, because there is one log and one writer.
///
/// Worth keeping precisely because it used to be hard. Two projections of
/// one append-only log could disagree when one of them was a broadcast a
/// subscriber might have joined late; neither of these can.
#[tokio::test]
async fn reading_forward_and_paging_back_agree_on_the_log() {
    let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
    session
        .ask(|reply| SessionCommand::UserMessage {
            text: "go".into(),
            reply,
        })
        .await
        .unwrap()
        .unwrap();
    wait_for_journal_len(&journal, id, 2).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let streamed: Vec<u64> = session
        .ask(|reply| SessionCommand::ReadLog {
            agent_id: None,
            after: None,
            reply,
        })
        .await
        .unwrap()
        .expect("main agent log")
        .entries
        .iter()
        .map(|e| e.seq)
        .collect();
    let stored: Vec<u64> = main_history(&session)
        .await
        .entries
        .iter()
        .map(|e| e.seq)
        .collect();
    assert!(!streamed.is_empty(), "the turn must produce entries");
    assert_eq!(streamed, stored);
    assert_eq!(
        streamed,
        (0..streamed.len() as u64).collect::<Vec<_>>(),
        "no gaps and no reordering"
    );
}

/// Reads and streams are served from actor state. The journal is touched
/// only while an actor recovers — never to answer a query.
#[tokio::test]
async fn serving_reads_never_touches_the_journal() {
    let f = actor_fixture().await;
    let id = Uuid::new_v4();
    f.deps
        .runtimes
        .create(&id.to_string(), "mock", &actor_spec_fixture())
        .await
        .expect("create");
    f.deps.provider_registry.write().unwrap().insert(
        "mock".to_string(),
        Arc::new(EchoProvider) as Arc<dyn LlmProvider>,
    );
    let counting = Arc::new(CountingJournal::new());
    let journal: Arc<dyn horsie_actor::Journal> = counting.clone();
    let session = horsie_actor::spawn_root(
        SessionActor::new(
            id,
            actor_spec_fixture(),
            f.deps.clone(),
            spawn_deaf_supervisor(),
            crate::sessions::Positions::default(),
        ),
        journal.clone(),
    );

    // Drive one turn so both actors are loaded and have history.
    session
        .ask(|reply| SessionCommand::UserMessage {
            text: "go".into(),
            reply,
        })
        .await
        .unwrap()
        .unwrap();
    wait_for_journal_len(&journal, id, 2).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Recovery is allowed to replay; everything after it is not.
    let after_recovery = counting.replays();
    assert!(
        after_recovery > 0,
        "the counter must actually observe recovery, or this test proves nothing"
    );

    let _ = main_history(&session).await;
    let _ = session
        .ask(|reply| SessionCommand::ReadLog {
            agent_id: None,
            after: Some(horsie_workflow::Cursor {
                entry_seq: 0,
                delta_seq: 0,
            }),
            reply,
        })
        .await
        .unwrap();
    let _ = session
        .ask(|reply| SessionCommand::AgentState {
            agent_id: None,
            reply,
        })
        .await
        .unwrap();

    assert_eq!(
        counting.replays(),
        after_recovery,
        "history and agent state must both be served from memory"
    );
}

async fn spawn_sub(session: &ActorRef<SessionCommand>, label: &str, task: &str) -> Uuid {
    session
        .ask(|reply| SessionCommand::SpawnSubAgent {
            caller: crate::sessions::subagents::SubAgentParent::Main,
            label: label.into(),
            task: task.into(),
            agent_type: None,
            reply,
        })
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn spawn_records_a_running_subagent_in_the_tree() {
    // Completion routing lands with outcome handling (next task); here the
    // spawn itself is what must be durable and attributed.
    let gate = BlockingProvider::new();
    let (_f, session, id, journal) = spawn_session_with_provider(gate).await;
    let sub = spawn_sub(&session, "research", "dig into it").await;
    wait_for_tree(&journal, id, |t| {
        t.node(sub)
            .is_some_and(|r| r.status == crate::sessions::subagents::SubAgentStatus::Running)
    })
    .await;
    let state = crate::sessions::events::fold_session_state(&journal, id).await;
    let rec = &state.subagents.node(sub).unwrap();
    assert_eq!(rec.depth, 1);
    assert_eq!(rec.parent, crate::sessions::subagents::SubAgentParent::Main);
    assert_eq!(rec.label, "research");
    assert_eq!(rec.task, "dig into it");
}

#[tokio::test]
async fn spawn_beyond_depth_four_is_rejected() {
    // A hanging provider keeps every spawned node Running, so the chain
    // builds deterministically: Main → d1 → d2 → d3 → d4, and d4's spawn
    // is refused.
    let gate = BlockingProvider::new();
    let (_f, session, id, journal) = spawn_session_with_provider(gate).await;
    let mut parent = crate::sessions::subagents::SubAgentParent::Main;
    for _ in 0..4 {
        let id_child = session
            .ask(|reply| SessionCommand::SpawnSubAgent {
                caller: parent,
                label: "w".into(),
                task: "t".into(),
                agent_type: None,
                reply,
            })
            .await
            .unwrap()
            .unwrap();
        wait_for_tree(&journal, id, |t| t.has_active()).await;
        parent = crate::sessions::subagents::SubAgentParent::SubAgent(id_child);
    }
    let res = session
        .ask(|reply| SessionCommand::SpawnSubAgent {
            caller: parent,
            label: "x".into(),
            task: "y".into(),
            agent_type: None,
            reply,
        })
        .await
        .unwrap();
    assert_eq!(res.unwrap_err(), "max subagent depth 4 reached");
}

#[tokio::test]
async fn spawn_beyond_the_concurrency_cap_is_rejected() {
    let gate = BlockingProvider::new();
    let (_f, session, id, journal) = spawn_session_with_provider(gate).await;
    for _ in 0..8 {
        let _ = spawn_sub(&session, "w", "t").await;
    }
    wait_for_tree(&journal, id, |t| t.active_count() == 8).await;
    let res = session
        .ask(|reply| SessionCommand::SpawnSubAgent {
            caller: crate::sessions::subagents::SubAgentParent::Main,
            label: "x".into(),
            task: "y".into(),
            agent_type: None,
            reply,
        })
        .await
        .unwrap();
    assert_eq!(res.unwrap_err(), "8 subagents already active");
}

#[tokio::test]
async fn spawn_from_an_unknown_caller_is_rejected() {
    let (_f, session, _id, _journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
    let res = session
        .ask(|reply| SessionCommand::SpawnSubAgent {
            caller: crate::sessions::subagents::SubAgentParent::SubAgent(Uuid::new_v4()),
            label: "x".into(),
            task: "y".into(),
            agent_type: None,
            reply,
        })
        .await
        .unwrap();
    assert_eq!(res.unwrap_err(), "caller is not a known agent");
}

#[tokio::test]
async fn subagent_toolbox_strips_session_metadata_tools() {
    let (f, session, id, _journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;

    let build = |kind: SessionAgentKind| SessionContextProvider {
        agent_type: None,
        runtimes: f.deps.runtimes.provider(id.to_string(), "mock".into()),
        registry: f.deps.provider_registry.clone(),
        mcp: None,
        memory: None,
        settings: actor_spec_fixture().agent,
        step_output_schema: None,
        session_id: id,
        kind,
        unattended: false,
        session: session.clone(),
        plugins: Vec::new(),
        plugin_library: None,
        last_client: Mutex::new(None),
    };

    let main = build(SessionAgentKind::Main).provide().await.unwrap();
    let main_tools: Vec<String> = main.toolbox.specs().into_iter().map(|s| s.name).collect();
    for t in [
        "spawn_agent",
        "subagent_status",
        "set_session_title",
        "ask_user",
    ] {
        assert!(main_tools.contains(&t.to_string()), "main lacks {t}");
    }

    let sub_id = Uuid::new_v4();
    let sub = build(SessionAgentKind::Sub(sub_id))
        .provide()
        .await
        .unwrap();
    let sub_tools: Vec<String> = sub.toolbox.specs().into_iter().map(|s| s.name).collect();
    for t in ["spawn_agent", "subagent_status"] {
        assert!(sub_tools.contains(&t.to_string()), "sub lacks {t}");
    }
    for t in ["set_session_title", "ask_user"] {
        assert!(!sub_tools.contains(&t.to_string()), "sub must not have {t}");
    }
    assert!(
        sub.system_prompt.unwrap().contains("# Subagent role"),
        "the subagent prompt must explain its role"
    );
}

#[tokio::test]
async fn a_zero_subagent_cap_hides_the_spawn_tools() {
    let (f, session, id, _journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
    let mut settings = actor_spec_fixture().agent;
    settings.max_concurrent_subagents = Some(0);
    let provider = SessionContextProvider {
        runtimes: f.deps.runtimes.provider(id.to_string(), "mock".into()),
        registry: f.deps.provider_registry.clone(),
        mcp: None,
        memory: None,
        settings,
        step_output_schema: None,
        session_id: id,
        kind: SessionAgentKind::Main,
        agent_type: None,
        unattended: false,
        session: session.clone(),
        plugins: Vec::new(),
        plugin_library: None,
        last_client: Mutex::new(None),
    };
    let tools: Vec<String> = provider
        .provide()
        .await
        .unwrap()
        .toolbox
        .specs()
        .into_iter()
        .map(|s| s.name)
        .collect();
    // Disabled, not merely unusable: an advertised tool that always
    // rejects reads as a bug to the model.
    for t in ["spawn_agent", "subagent_status"] {
        assert!(!tools.contains(&t.to_string()), "disabled session has {t}");
    }
}

#[tokio::test]
async fn an_unattended_session_is_offered_no_ask_user_tool() {
    // A routine run has nobody to answer a question: offering `ask_user`
    // would let the agent park the run forever. The prompt has to say so
    // too -- the base prompt tells the model the tool exists.
    let (f, session, id, _journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
    let build = |unattended: bool| SessionContextProvider {
        runtimes: f.deps.runtimes.provider(id.to_string(), "mock".into()),
        registry: f.deps.provider_registry.clone(),
        mcp: None,
        memory: None,
        settings: actor_spec_fixture().agent,
        step_output_schema: None,
        session_id: id,
        kind: SessionAgentKind::Main,
        agent_type: None,
        unattended,
        session: session.clone(),
        plugins: Vec::new(),
        plugin_library: None,
        last_client: Mutex::new(None),
    };
    let names =
        |c: &Contexts| -> Vec<String> { c.toolbox.specs().into_iter().map(|s| s.name).collect() };

    let unattended = build(true).provide().await.unwrap();
    let tools = names(&unattended);
    assert!(!tools.contains(&ASK_USER_TOOL.to_string()));
    // Everything else the main agent has is untouched.
    assert!(tools.contains(&"set_session_title".to_string()));
    assert!(tools.contains(&"spawn_agent".to_string()));
    assert!(
        unattended
            .system_prompt
            .unwrap()
            .contains("# Unattended run"),
        "an unattended run must be told there is no user"
    );

    let attended = build(false).provide().await.unwrap();
    assert!(names(&attended).contains(&ASK_USER_TOOL.to_string()));
    assert!(!attended.system_prompt.unwrap().contains("# Unattended run"));
}

#[test]
fn a_subagent_gets_its_own_runtime_identity() {
    let client = horsie_runtime_client::RuntimeClient::new(
        horsie_runtime_client::MockTransport::ok(""),
        "session-id",
    );
    let main = scoped_client(&SessionAgentKind::Main, client.clone());
    assert_eq!(main.agent_id(), "session-id");

    let sub_id = Uuid::new_v4();
    let sub = scoped_client(&SessionAgentKind::Sub(sub_id), client);
    assert_eq!(sub.agent_id(), sub_id.to_string());
}

fn user_texts(page: &horsie_workflow::LogPage) -> Vec<String> {
    page.messages()
        .filter(|m| m.role == horsie_agentcore::Role::User)
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            horsie_agentcore::ContentPart::Text(t) => Some(t.text.clone()),
            horsie_agentcore::ContentPart::ToolCall(_)
            | horsie_agentcore::ContentPart::ToolResult(_)
            | horsie_agentcore::ContentPart::Thinking(_)
            | horsie_agentcore::ContentPart::SubAgentResult(_) => None,
        })
        .collect()
}

/// A user message's subagent-result parts, rendered the way the wire sees
/// them — the counterpart to `user_texts` now that a result is a part of
/// its own rather than text merged into what the person said.
fn subagent_texts(page: &horsie_workflow::LogPage) -> Vec<String> {
    page.messages()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            horsie_agentcore::ContentPart::SubAgentResult(r) => Some(r.to_wire_text()),
            horsie_agentcore::ContentPart::Text(_)
            | horsie_agentcore::ContentPart::ToolCall(_)
            | horsie_agentcore::ContentPart::ToolResult(_)
            | horsie_agentcore::ContentPart::Thinking(_) => None,
        })
        .collect()
}

fn hook_record(plugin: &str, call: &str) -> HookRecord {
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

async fn agent_history(
    session: &ActorRef<SessionCommand>,
    agent_id: Option<String>,
) -> horsie_workflow::LogPage {
    session
        .ask(|reply| SessionCommand::PageLog {
            agent_id,
            before: None,
            max: 50,
            reply,
        })
        .await
        .unwrap()
        .expect("agent history")
}

fn hook_ids(page: &horsie_workflow::LogPage) -> Vec<String> {
    page.entries
        .iter()
        .filter_map(|e| match &e.body {
            horsie_agentcore::AgentLogBody::Hook(h) => Some(h.id.clone()),
            horsie_agentcore::AgentLogBody::Llm(_)
            | horsie_agentcore::AgentLogBody::Lifecycle(_) => None,
        })
        .collect()
}

// --- `Stop` continuation ---
//
// `Stop` is the only event whose two capabilities are both ways of *not*
// ending a turn, so these assert on what happens to the turn rather than on
// what was recorded. `FakeRuntimeVendor` answers the protocol itself, so
// they script records; real command execution is covered one layer down, in
// `runtime/src/hooks/server.rs`.

fn stop_record(outcome: StopOutcome) -> HookRecord {
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

fn stop_blocked(reason: &str) -> Vec<HookRecord> {
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
struct PromptRecorder(Arc<Mutex<Vec<String>>>);

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
async fn stop_harness(records: Vec<Vec<HookRecord>>) -> (ActorFixture, ActorRef<SessionCommand>) {
    let (f, session, _, _, _) = stop_harness_full(records).await;
    (f, session)
}

/// The same harness, also handing back every prompt the model was sent.
async fn stop_harness_with_prompts(
    records: Vec<Vec<HookRecord>>,
) -> (
    ActorFixture,
    ActorRef<SessionCommand>,
    Arc<Mutex<Vec<String>>>,
) {
    let (f, session, prompts, _, _) = stop_harness_full(records).await;
    (f, session, prompts)
}

/// The same harness, also handing back the journal, for a test that has to
/// read what was *persisted*. A spurious failure is overwritten in the
/// status by whatever lands next; the journal keeps it.
async fn stop_harness_with_journal(
    records: Vec<Vec<HookRecord>>,
) -> (
    ActorFixture,
    ActorRef<SessionCommand>,
    Uuid,
    Arc<dyn horsie_actor::Journal>,
) {
    let (f, session, _, id, journal) = stop_harness_full(records).await;
    (f, session, id, journal)
}

async fn stop_harness_full(
    records: Vec<Vec<HookRecord>>,
) -> (
    ActorFixture,
    ActorRef<SessionCommand>,
    Arc<Mutex<Vec<String>>>,
    Uuid,
    Arc<dyn horsie_actor::Journal>,
) {
    let tmp = tempfile::tempdir().unwrap();
    let agent = crate::runtime_vendor::fake::FakeRuntimeVendor::builder("mock")
        .hook_records(records)
        .serve_in_process()
        .await
        .expect("fake agent");
    let mut vendors = HashMap::new();
    vendors.insert("mock".to_string(), agent.link());
    let vendors = Arc::new(std::sync::RwLock::new(vendors));
    let deps = ServerDeps {
        runtimes: crate::runtime_manager::test_runtime_manager(&vendors),
        provider_registry: Arc::new(std::sync::RwLock::new(HashMap::new())),
        vendors,
        github_tokens: None,
        mcp: None,
        plugins: None,
        memory: None,
    };
    let f = ActorFixture {
        deps,
        agent,
        _tmp: tmp,
    };
    let id = Uuid::new_v4();
    f.deps
        .runtimes
        .create(&id.to_string(), "mock", &actor_spec_fixture())
        .await
        .expect("create");
    let prompts: Arc<Mutex<Vec<String>>> = Arc::default();
    f.deps.provider_registry.write().unwrap().insert(
        "mock".to_string(),
        Arc::new(PromptRecorder(prompts.clone())) as Arc<dyn LlmProvider>,
    );
    let journal: Arc<dyn horsie_actor::Journal> = Arc::new(horsie_actor::InMemoryJournal::new());
    let session = horsie_actor::spawn_root(
        SessionActor::new(
            id,
            actor_spec_fixture(),
            f.deps.clone(),
            spawn_deaf_supervisor(),
            crate::sessions::Positions::default(),
        ),
        journal.clone(),
    );
    (f, session, prompts, id, journal)
}

/// Every user-role message in the main agent's transcript, in order — which
/// is one per turn, so its length is the number of turns that ran.
async fn turn_inputs(session: &ActorRef<SessionCommand>) -> Vec<String> {
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
            | horsie_agentcore::AgentLogBody::Lifecycle(_) => None,
        })
        .collect()
}

/// The `Stop` outcomes journaled on the main agent's transcript.
async fn stop_outcomes(session: &ActorRef<SessionCommand>) -> Vec<StopOutcome> {
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
            | horsie_agentcore::AgentLogBody::Lifecycle(_) => None,
        })
        .collect()
}

/// Wait until the transcript stops growing, so a test asserting "no further
/// turn ran" observes a real stop rather than a race it won.
async fn settled_inputs(session: &ActorRef<SessionCommand>) -> Vec<String> {
    let mut last = turn_inputs(session).await;
    let mut stable = 0;
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let now = turn_inputs(session).await;
        if now == last {
            stable += 1;
            if stable == 5 {
                return now;
            }
        } else {
            stable = 0;
            last = now;
        }
    }
    last
}

async fn send(session: &ActorRef<SessionCommand>, text: &str) {
    session
        .ask(|reply| SessionCommand::UserMessage {
            text: text.into(),
            reply,
        })
        .await
        .unwrap()
        .unwrap();
}

/// A blocking `Stop` means *blocked from stopping*: the turn does not
/// conclude, and the reason becomes the input to another run. The opposite
/// of a `PreToolUse` refusal.
#[tokio::test]
async fn a_blocking_stop_hook_starts_another_run_with_its_reason() {
    let (_f, session) = stop_harness(vec![
        stop_blocked("tests still failing"),
        vec![stop_record(StopOutcome::Ran(
            horsie_models::hooks::ContextInjected {
                additional_context: None,
            },
        ))],
    ])
    .await;
    send(&session, "do the thing").await;
    let inputs = settled_inputs(&session).await;
    assert_eq!(inputs.len(), 2, "the turn continued once: {inputs:?}");
    assert!(inputs[0].contains("do the thing"), "{inputs:?}");
    assert!(inputs[1].contains("tests still failing"), "{inputs:?}");
}

/// The loop guard that must not be optional: horsie runs unattended
/// sessions, so a hook ignoring `stop_hook_active` would spin forever with
/// nobody watching.
#[tokio::test]
async fn an_unconditionally_blocking_stop_hook_is_stopped_by_the_cap() {
    let (_f, session) = stop_harness(vec![stop_blocked("again")]).await;
    send(&session, "go").await;
    let inputs = settled_inputs(&session).await;
    assert_eq!(
        inputs.len(),
        1 + MAX_STOP_CONTINUATIONS,
        "the original turn plus exactly the cap: {inputs:?}"
    );
}

/// And the record says the cap ended it, rather than looking like a turn
/// that ended on its own.
#[tokio::test]
async fn the_capped_continuation_is_recorded_as_cap_reached() {
    let (_f, session) = stop_harness(vec![stop_blocked("again")]).await;
    send(&session, "go").await;
    settled_inputs(&session).await;
    let outcomes = stop_outcomes(&session).await;
    assert!(
        matches!(outcomes.last(), Some(StopOutcome::CapReached(_))),
        "the last record must name the cap, got {outcomes:?}"
    );
}

/// Non-blocking feedback informs the model; it does not force a turn.
/// Starting a run on it would make every advisory `Stop` hook an infinite
/// session.
#[tokio::test]
async fn non_blocking_additional_context_does_not_start_a_run() {
    let (_f, session) = stop_harness(vec![vec![stop_record(StopOutcome::Ran(
        horsie_models::hooks::ContextInjected {
            additional_context: Some("consider the tests".into()),
        },
    ))]])
    .await;
    send(&session, "go").await;
    let inputs = settled_inputs(&session).await;
    assert_eq!(inputs.len(), 1, "informed, not forced: {inputs:?}");
}

/// `continue: false` outranks `decision: "block"`, which is the spec's own
/// precedence — and the one seam where that precedence is observable. The
/// same record blocks *and* halts; the turn ends rather than continuing.
#[tokio::test]
async fn a_halt_beats_a_blocking_stop_hook() {
    let mut blocking = stop_blocked("tests still failing");
    blocking[0].halt = Some(horsie_models::hooks::HookHalt {
        reason: Some("out of budget".into()),
    });
    let (_f, session, id, journal) = stop_harness_with_journal(vec![blocking]).await;
    send(&session, "go").await;
    let inputs = settled_inputs(&session).await;
    assert_eq!(
        inputs.len(),
        1,
        "the halt must stop the block continuing the turn: {inputs:?}"
    );
    // And ends it *cleanly*. `run_hooks` puts its records on the same sink
    // tool records take, so before `tool_halt_reason` narrowed what the sink
    // acts on, this halt also arrived as a `HaltAgent` and failed the turn
    // the stop seam had already concluded. Read off the journal rather than
    // the status: `TurnEnded` lands after the spurious failure and hides it.
    let events = journaled_events(&journal, id).await;
    assert!(
        !events.iter().any(|e| e.contains("TurnFailed")),
        "a halted stop ends the turn, it does not fail it: {events:?}"
    );
}

/// Every session event that reached the journal, as its serialized payload.
/// Matched on as text, because the variant name is what a test cares about
/// and decoding buys nothing over reading it.
async fn journaled_events(
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

// --- Slash commands, skills and agents ---

/// A plugin library scripted with a fixed catalogue.
///
/// The seam's question is "what does this name mean?", and the answer comes
/// from the database. Ingesting a real bundle to answer it would test
/// `pack()` a second time and pay for a git clone per case.
struct FakeLibrary(Vec<horsie_support::plugin::catalog::CatalogEntry>);

#[async_trait]
impl crate::plugins::PluginProvisioner for FakeLibrary {
    async fn resolve(
        &self,
        _names: &[String],
    ) -> Result<Vec<crate::plugins::PluginArtifactRef>, String> {
        Ok(Vec::new())
    }

    fn mint_token(&self, _session_id: &str, _hashes: &[String]) -> String {
        String::new()
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

fn catalog_entry(
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

async fn catalog_harness(
    entries: Vec<horsie_support::plugin::catalog::CatalogEntry>,
) -> (ActorFixture, ActorRef<SessionCommand>, Uuid) {
    catalog_harness_with(entries, Vec::new()).await
}

async fn catalog_harness_with(
    entries: Vec<horsie_support::plugin::catalog::CatalogEntry>,
    hook_records: Vec<Vec<HookRecord>>,
) -> (ActorFixture, ActorRef<SessionCommand>, Uuid) {
    let tmp = tempfile::tempdir().unwrap();
    let agent = crate::runtime_vendor::fake::FakeRuntimeVendor::builder("mock")
        .hook_records(hook_records)
        .serve_in_process()
        .await
        .expect("fake agent");
    let mut vendors = HashMap::new();
    vendors.insert("mock".to_string(), agent.link());
    let vendors = Arc::new(std::sync::RwLock::new(vendors));
    let deps = ServerDeps {
        runtimes: crate::runtime_manager::test_runtime_manager(&vendors),
        provider_registry: Arc::new(std::sync::RwLock::new(HashMap::new())),
        vendors,
        github_tokens: None,
        mcp: None,
        plugins: Some(Arc::new(FakeLibrary(entries))),
        memory: None,
    };
    let f = ActorFixture {
        deps,
        agent,
        _tmp: tmp,
    };
    let id = Uuid::new_v4();
    f.deps
        .runtimes
        .create(&id.to_string(), "mock", &actor_spec_fixture())
        .await
        .expect("create");
    f.deps.provider_registry.write().unwrap().insert(
        "mock".to_string(),
        Arc::new(PromptRecorder(Arc::default())) as Arc<dyn LlmProvider>,
    );
    let session = horsie_actor::spawn_root(
        SessionActor::new(
            id,
            actor_spec_fixture(),
            f.deps.clone(),
            spawn_deaf_supervisor(),
            crate::sessions::Positions::default(),
        ),
        Arc::new(horsie_actor::InMemoryJournal::new()) as Arc<dyn horsie_actor::Journal>,
    );
    (f, session, id)
}

fn catalog_provider(
    f: &ActorFixture,
    session: &ActorRef<SessionCommand>,
    id: Uuid,
) -> SessionContextProvider {
    SessionContextProvider {
        runtimes: f.deps.runtimes.provider(id.to_string(), "mock".to_string()),
        registry: f.deps.provider_registry.clone(),
        mcp: None,
        memory: None,
        settings: actor_spec_fixture().agent,
        step_output_schema: None,
        session_id: id,
        kind: SessionAgentKind::Main,
        agent_type: None,
        unattended: false,
        session: session.clone(),
        plugins: Vec::new(),
        plugin_library: f.deps.plugins.clone(),
        last_client: Mutex::new(None),
    }
}

/// The prompt the seam produced for this turn — the whole point of the
/// expansion, since it is what the model actually reads.
async fn prepared_message(provider: &SessionContextProvider, prompt: &str) -> Option<String> {
    provider
        .start_hooks(StartTurn {
            start_source: None,
            prompt: Some(prompt.to_string()),
        })
        .await
        .expect("prepare")
        .message
}

#[tokio::test]
async fn a_slash_command_expands_into_its_framed_template() {
    let (f, session, id) = catalog_harness(vec![catalog_entry(
        horsie_support::plugin::catalog::CatalogKind::Command,
        "review",
        Some("Review $1 for bugs. Full args: $ARGUMENTS"),
    )])
    .await;
    let provider = catalog_provider(&f, &session, id);
    let message = prepared_message(&provider, "/review src/a.rs carefully")
        .await
        .expect("a command expands");
    assert!(
        message.starts_with("<command name=\"review\" args=\"src/a.rs carefully\">"),
        "framed so a client can tell an invocation from typed text: {message}"
    );
    assert!(message.contains("Review src/a.rs for bugs."), "{message}");
    assert!(
        message.contains("Full args: src/a.rs carefully"),
        "{message}"
    );
}

/// A skill and an agent have no template, so expansion names the thing and
/// lets the agent reach for the tool it already has.
#[tokio::test]
async fn a_skill_and_an_agent_expand_under_their_own_sigils() {
    use horsie_support::plugin::catalog::CatalogKind;
    let (f, session, id) = catalog_harness(vec![
        catalog_entry(CatalogKind::Skill, "tdd", None),
        catalog_entry(CatalogKind::Agent, "reviewer", None),
    ])
    .await;
    let provider = catalog_provider(&f, &session, id);

    let skill = prepared_message(&provider, "/tdd on the parser")
        .await
        .unwrap();
    assert!(skill.starts_with("<skill name=\"tdd\""), "{skill}");
    assert!(skill.contains("Use the `tdd` skill."), "{skill}");
    assert!(skill.contains("on the parser"), "{skill}");

    let agent = prepared_message(&provider, "@reviewer this diff")
        .await
        .unwrap();
    assert!(agent.starts_with("<agent name=\"reviewer\""), "{agent}");
    assert!(agent.contains("spawn_agent"), "{agent}");

    // The sigil is part of the identity: `@` must not become a second `/`.
    assert_eq!(
        prepared_message(&provider, "@tdd").await.as_deref(),
        Some("@tdd"),
        "a skill is not reachable as an agent"
    );
}

/// An unknown name is left exactly as written: a message may legitimately
/// begin with a slash, and refusing it would make `/etc/hosts` unsendable.
#[tokio::test]
async fn an_unknown_name_leaves_the_prompt_alone() {
    let (f, session, id) = catalog_harness(vec![catalog_entry(
        horsie_support::plugin::catalog::CatalogKind::Command,
        "review",
        Some("body"),
    )])
    .await;
    let provider = catalog_provider(&f, &session, id);
    for prompt in [
        "/nosuch thing",
        "/etc/hosts is a file",
        "hello",
        "mail me at a@b.com",
    ] {
        assert_eq!(
            prepared_message(&provider, prompt).await.as_deref(),
            Some(prompt),
            "{prompt} must reach the model unchanged"
        );
    }
}

/// Expanding costs no runtime call — which is the whole reason the
/// catalogue moved to the server.
#[tokio::test]
async fn expansion_makes_no_workspace_scan() {
    let (f, session, id) = catalog_harness(vec![catalog_entry(
        horsie_support::plugin::catalog::CatalogKind::Command,
        "review",
        Some("body"),
    )])
    .await;
    let provider = catalog_provider(&f, &session, id);
    prepared_message(&provider, "/review a.rs").await;
    assert_eq!(
        f.agent.scan_count(),
        0,
        "the seam answers from the database, not the sandbox"
    );
}

/// `UserPromptExpansion` fires for the entry being expanded, carrying its
/// name as the matcher domain and its kind alongside — and before
/// `UserPromptSubmit` sees the result, which is the order the spec gives
/// them.
#[tokio::test]
async fn expansion_is_hooked_before_submission() {
    let (f, session, id) = catalog_harness(vec![catalog_entry(
        horsie_support::plugin::catalog::CatalogKind::Command,
        "review",
        Some("body"),
    )])
    .await;
    let provider = catalog_provider(&f, &session, id);
    prepared_message(&provider, "/review a.rs").await;

    let events = f.agent.hook_events();
    let expansion = events.iter().position(|e| *e == "UserPromptExpansion");
    let submit = events.iter().position(|e| *e == "UserPromptSubmit");
    assert!(
        expansion.is_some(),
        "the expansion must be hooked: {events:?}"
    );
    assert!(
        expansion < submit,
        "expansion runs first, so a submit guard reads what the model will: {events:?}"
    );
    let named: Vec<(String, String)> = f
        .agent
        .server_hook_events()
        .into_iter()
        .filter_map(|e| match e {
            horsie_models::runtime::ServerHookEvent::UserPromptExpansion(i) => {
                Some((i.command, i.kind))
            }
            _ => None,
        })
        .collect();
    assert_eq!(named, vec![("review".to_string(), "command".to_string())]);
}

/// A hook answering `{"decision":"block"}` must stop the expansion itself,
/// not merely be noticed a layer later with the work already done. The
/// block is not a halt, and reading only the halt is how this regressed.
#[tokio::test]
async fn a_blocking_expansion_hook_stops_the_expansion() {
    let blocked = HookRecord {
        plugin: "guard".into(),
        duration_ms: 0,
        halt: None,
        action: HookAction::UserPromptExpansion(horsie_models::hooks::UserPromptExpansionRecord {
            command: "review".into(),
            system_message: None,
            outcome: horsie_models::hooks::UserPromptExpansionOutcome::Blocked(
                horsie_models::hooks::HookBlocked {
                    reason: Some("not this one".into()),
                },
            ),
        }),
    };
    let (f, session, id) = catalog_harness_with(
        vec![catalog_entry(
            horsie_support::plugin::catalog::CatalogKind::Command,
            "review",
            Some("the template"),
        )],
        vec![vec![blocked]],
    )
    .await;
    let provider = catalog_provider(&f, &session, id);
    let prep = provider
        .start_hooks(StartTurn {
            start_source: None,
            prompt: Some("/review a.rs".to_string()),
        })
        .await
        .expect("prepare");
    assert_eq!(
        prep.message.as_deref(),
        Some("/review a.rs"),
        "a refused expansion leaves the prompt as typed"
    );
    assert_eq!(
        horsie_workflow::start_blocked(&prep.records).as_deref(),
        Some("not this one"),
        "and the refusal still abandons the turn"
    );
    assert!(
        !f.agent.hook_events().contains(&"UserPromptSubmit"),
        "a refused prompt never becomes a submission: {:?}",
        f.agent.hook_events()
    );
}

// --- Plugin agents ---

/// A session whose runtime library declares `code-reviewer`, with a
/// `PromptRecorder` so the test can assert what the model was actually
/// told rather than what the transcript would render.
async fn agent_harness() -> (ActorFixture, ActorRef<SessionCommand>, Uuid) {
    let tmp = tempfile::tempdir().unwrap();
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
    let mut vendors = HashMap::new();
    vendors.insert("mock".to_string(), agent.link());
    let vendors = Arc::new(std::sync::RwLock::new(vendors));
    let deps = ServerDeps {
        runtimes: crate::runtime_manager::test_runtime_manager(&vendors),
        provider_registry: Arc::new(std::sync::RwLock::new(HashMap::new())),
        vendors,
        github_tokens: None,
        mcp: None,
        plugins: None,
        memory: None,
    };
    let f = ActorFixture {
        deps,
        agent,
        _tmp: tmp,
    };
    let id = Uuid::new_v4();
    f.deps
        .runtimes
        .create(&id.to_string(), "mock", &actor_spec_fixture())
        .await
        .expect("create");
    let prompts: Arc<Mutex<Vec<String>>> = Arc::default();
    f.deps.provider_registry.write().unwrap().insert(
        "mock".to_string(),
        Arc::new(PromptRecorder(prompts.clone())) as Arc<dyn LlmProvider>,
    );
    let session = horsie_actor::spawn_root(
        SessionActor::new(
            id,
            actor_spec_fixture(),
            f.deps.clone(),
            spawn_deaf_supervisor(),
            crate::sessions::Positions::default(),
        ),
        Arc::new(horsie_actor::InMemoryJournal::new()) as Arc<dyn horsie_actor::Journal>,
    );
    drop(prompts);
    (f, session, id)
}

async fn spawn_typed(
    session: &ActorRef<SessionCommand>,
    agent_type: Option<&str>,
) -> Result<Uuid, String> {
    session
        .ask(|reply| SessionCommand::SpawnSubAgent {
            caller: crate::sessions::subagents::SubAgentParent::Main,
            label: "review".into(),
            task: "look at the diff".into(),
            agent_type: agent_type.map(str::to_string),
            reply,
        })
        .await
        .unwrap()
}

/// A provider for one subagent of `agent_harness`'s session, optionally
/// carrying a session-level tool allowlist.
fn typed_provider(
    f: &ActorFixture,
    session: &ActorRef<SessionCommand>,
    id: Uuid,
    sub: Uuid,
    allowed_tools: Option<Vec<String>>,
) -> SessionContextProvider {
    let mut settings = actor_spec_fixture().agent;
    settings.allowed_tools = allowed_tools;
    SessionContextProvider {
        runtimes: f.deps.runtimes.provider(id.to_string(), "mock".to_string()),
        registry: f.deps.provider_registry.clone(),
        mcp: None,
        memory: None,
        settings,
        step_output_schema: None,
        session_id: id,
        kind: SessionAgentKind::Sub(sub),
        agent_type: Some("code-reviewer".to_string()),
        unattended: false,
        session: session.clone(),
        plugins: Vec::new(),
        plugin_library: None,
        last_client: Mutex::new(None),
    }
}

/// The agent's body is added to the generic subagent role, and its `tools`
/// allowlist reaches the toolbox through the same alias table hook matchers
/// use.
#[tokio::test]
async fn a_typed_subagent_runs_with_its_plugins_prompt() {
    let (f, session, id) = agent_harness().await;
    let sub = spawn_typed(&session, Some("code-reviewer")).await.unwrap();

    let provider = typed_provider(&f, &session, id, sub, None);
    let contexts = provider.provide().await.expect("contexts");
    let prompt = contexts.system_prompt.unwrap_or_default();
    assert!(
        prompt.contains("# Agent type: code-reviewer"),
        "the plugin's agent names its own section: {prompt}"
    );
    // The generic framing is the only place a subagent is told where its
    // output goes; a plugin's prompt never says it, so it must survive.
    assert!(
        prompt.contains("Your final message is your report"),
        "a typed subagent must still know it reports to its parent: {prompt}"
    );
    assert!(
        prompt.contains("Report only high-confidence bugs."),
        "the plugin's body is the role: {prompt}"
    );
    // `Read, Grep` in Claude's vocabulary is `read_file, grep` in horsie's.
    let tools: Vec<String> = contexts
        .toolbox
        .specs()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert!(tools.contains(&"read_file".to_string()), "{tools:?}");
    assert!(tools.contains(&"grep".to_string()), "{tools:?}");
    assert!(
        !tools.contains(&"bash".to_string()),
        "the allowlist must exclude what it did not name: {tools:?}"
    );
}

/// An agent definition is a file inside a plugin. It may narrow the tools
/// the session already grants it and must not be able to widen them —
/// otherwise installing a plugin would hand back what an operator withheld.
#[tokio::test]
async fn an_agents_tools_cannot_widen_the_sessions_own_allowlist() {
    let (f, session, id) = agent_harness().await;
    let sub = spawn_typed(&session, Some("code-reviewer")).await.unwrap();

    // The session grants `grep` only; the agent asks for `Read, Grep`.
    let provider = typed_provider(&f, &session, id, sub, Some(vec!["grep".to_string()]));
    let contexts = provider.provide().await.expect("contexts");
    let tools: Vec<String> = contexts
        .toolbox
        .specs()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert!(tools.contains(&"grep".to_string()), "{tools:?}");
    assert!(
        !tools.contains(&"read_file".to_string()),
        "an agent must not grant itself a tool the session withheld: {tools:?}"
    );
}

/// The definition is resolved when the subagent runs, not carried from the
/// spawn — so an agent whose plugin has gone fails loudly rather than
/// running a prompt nobody can point at.
#[tokio::test]
async fn a_subagent_whose_agent_type_is_gone_fails_rather_than_running_generic() {
    let (f, session, id) = agent_harness().await;
    let provider = SessionContextProvider {
        runtimes: f.deps.runtimes.provider(id.to_string(), "mock".to_string()),
        registry: f.deps.provider_registry.clone(),
        mcp: None,
        memory: None,
        settings: actor_spec_fixture().agent,
        step_output_schema: None,
        session_id: id,
        kind: SessionAgentKind::Sub(Uuid::new_v4()),
        agent_type: Some("uninstalled-agent".to_string()),
        unattended: false,
        session: session.clone(),
        plugins: Vec::new(),
        plugin_library: None,
        last_client: Mutex::new(None),
    };
    let Err(err) = provider.provide().await else {
        panic!("a subagent whose agent type is gone must not run generic");
    };
    assert!(err.message.contains("uninstalled-agent"), "{}", err.message);
    assert!(
        !err.terminal,
        "a missing plugin is not the end of a session"
    );
}

/// The type is what `SubagentStart` / `SubagentStop` matchers select on. It
/// was the constant `"subagent"` for every subagent before agent types
/// existed, so a matcher could only select all or none.
#[tokio::test]
async fn the_agent_type_reaches_the_subagent_hook_matcher() {
    let (f, session, _id) = agent_harness().await;
    spawn_typed(&session, Some("code-reviewer")).await.unwrap();
    for _ in 0..200 {
        if f.agent.hook_events().contains(&"SubagentStart") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let types: Vec<String> = f
        .agent
        .server_hook_events()
        .into_iter()
        .filter_map(|e| match e {
            horsie_models::runtime::ServerHookEvent::SubagentStart(i) => Some(i.agent_type),
            _ => None,
        })
        .collect();
    assert_eq!(types, vec!["code-reviewer".to_string()]);
}

/// An untyped spawn is the general-purpose subagent, unchanged.
#[tokio::test]
async fn an_untyped_spawn_still_reports_the_generic_type() {
    let (f, session, _id) = agent_harness().await;
    spawn_typed(&session, None).await.unwrap();
    for _ in 0..200 {
        if f.agent.hook_events().contains(&"SubagentStart") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let types: Vec<String> = f
        .agent
        .server_hook_events()
        .into_iter()
        .filter_map(|e| match e {
            horsie_models::runtime::ServerHookEvent::SubagentStart(i) => Some(i.agent_type),
            _ => None,
        })
        .collect();
    assert_eq!(types, vec!["subagent".to_string()]);
}

/// A halt from a tool hook reaches the session as its own command, because
/// the runtime that ran the hook cannot end a turn and the agent is mid-call
/// when it arrives. The reason is what the user is shown.
#[tokio::test]
async fn halting_the_main_agent_fails_the_turn_with_the_hooks_reason() {
    let gate = BlockingProvider::new();
    let (_f, session, _id, _journal) = spawn_session_with_provider(gate.clone()).await;
    let status = |s: ActorRef<SessionCommand>| async move {
        s.ask(|reply| SessionCommand::Snapshot { reply })
            .await
            .unwrap()
            .status
    };
    send(&session, "go").await;
    // The turn is parked in the provider, which is where a tool hook's halt
    // would arrive from.
    for _ in 0..200 {
        if status(session.clone()).await == SessionStatus::Running {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    session
        .tell(SessionCommand::HaltAgent {
            key: AgentKey::Main,
            reason: "the repo is locked".into(),
        })
        .await
        .unwrap();
    gate.release();

    for _ in 0..200 {
        if let SessionStatus::Failed { reason } = status(session.clone()).await {
            assert_eq!(reason, "the repo is locked");
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the halted turn never failed");
}

/// `Stop` runs after the fact, so a guard that could not run cannot deny
/// anything. Only `PreToolUse` fails closed.
#[tokio::test]
async fn a_failing_stop_hook_concludes_the_turn_anyway() {
    let (_f, session) = stop_harness(vec![vec![stop_record(StopOutcome::Failed(
        horsie_models::hooks::HookFailed {
            reason: "spawn failed".into(),
        },
    ))]])
    .await;
    send(&session, "go").await;
    assert_eq!(settled_inputs(&session).await.len(), 1);
}

/// Every `Stop` hook that ran reaches the transcript, which is the point of
/// running them at all.
#[tokio::test]
async fn every_stop_hook_run_reaches_the_transcript() {
    let (_f, session) = stop_harness(vec![vec![stop_record(StopOutcome::Ran(
        horsie_models::hooks::ContextInjected {
            additional_context: None,
        },
    ))]])
    .await;
    send(&session, "go").await;
    settled_inputs(&session).await;
    assert_eq!(stop_outcomes(&session).await.len(), 1);
}

/// The bug this change exists to close, end to end.
///
/// `injected_context` knew how to pull `additionalContext` off a `Stop`
/// record and had exactly one caller — the `SessionStart` bootstrap — so a
/// `Stop` hook's context was recorded, rendered in the web UI, and never
/// shown to the model. It reaches the next turn's prompt now because
/// `prompt_messages` translates the record where it sits.
#[tokio::test]
async fn a_stop_hooks_context_reaches_the_next_prompt() {
    let (_f, session, prompts) = stop_harness_with_prompts(vec![vec![stop_record(
        StopOutcome::Ran(horsie_models::hooks::ContextInjected {
            additional_context: Some("run the linter before you finish".into()),
        }),
    )]])
    .await;
    send(&session, "first").await;
    settled_inputs(&session).await;
    assert_eq!(stop_outcomes(&session).await.len(), 1);

    // The hook ran when the first turn ended, so the second turn is the
    // first prompt that can carry it.
    send(&session, "second").await;
    settled_inputs(&session).await;

    let seen = prompts.lock().unwrap().clone();
    assert!(
        seen.iter()
            .any(|t| t.contains("run the linter before you finish")),
        "the Stop hook's context must reach the model, got {seen:?}"
    );
}

// --- Start hooks, and which event a turn actually fires ---
//
// Deciding *which* event fires, and how often, is this layer's job: the
// agent only says "a start is due, on this source" and "here is the prompt".
// The prompt those records reach is `hook_translation`'s job, tested there.

/// `SessionStart` used to fire from `provide()`, which is per-run — so every
/// turn re-ran every start hook, always reporting `source: "startup"`. It
/// fires once per agent load now; `UserPromptSubmit` is the one that belongs
/// to every turn.
#[tokio::test]
async fn a_session_starts_once_but_every_prompt_is_hooked() {
    let (f, session) = stop_harness(vec![]).await;
    send(&session, "first").await;
    settled_inputs(&session).await;
    send(&session, "second").await;
    settled_inputs(&session).await;

    let starts = f
        .agent
        .hook_events()
        .into_iter()
        .filter(|e| *e == "SessionStart")
        .count();
    let prompts = f
        .agent
        .hook_events()
        .into_iter()
        .filter(|e| *e == "UserPromptSubmit")
        .count();
    assert_eq!(starts, 1, "the start hook is due once per agent load");
    assert_eq!(prompts, 2, "the prompt hook is due every turn");
}

/// A subagent is not a session. The call fired `SessionStart` for one before
/// this, because it was not gated on the agent's kind at all — so a hook
/// matching `startup` fired again for every subagent spawned.
#[tokio::test]
async fn a_subagent_fires_subagent_start_never_session_start() {
    let (f, session) = stop_harness(vec![]).await;
    spawn_sub(&session, "research", "dig into it").await;
    for _ in 0..200 {
        if f.agent.hook_events().contains(&"SubagentStart") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // The main agent runs a turn of its own once the subagent reports back,
    // so one `SessionStart` is correct here. What must never happen is the
    // subagent contributing a second one — which is what it did before,
    // because the call was not gated on the agent's kind.
    let events = f.agent.hook_events();
    assert_eq!(
        events.iter().filter(|e| **e == "SubagentStart").count(),
        1,
        "the subagent starts as a subagent, got {events:?}"
    );
    assert_eq!(
        events.iter().filter(|e| **e == "SessionStart").count(),
        1,
        "only the session's own agent may claim a session start, got {events:?}"
    );
}

/// A hook guards one agent's call, so its record belongs in that agent's
/// transcript. Routed to the session instead, every agent's hooks would pile
/// into one log with no way to tell whose call they guarded — which is what
/// the session-scoped journal did before.
#[tokio::test]
async fn a_subagents_hooks_land_on_its_own_transcript() {
    let gate = BlockingProvider::new();
    let (_f, session, id, journal) = spawn_session_with_provider(gate).await;
    let sub = spawn_sub(&session, "research", "dig into it").await;
    wait_for_tree(&journal, id, |t| {
        t.node(sub)
            .is_some_and(|r| r.status == crate::sessions::subagents::SubAgentStatus::Running)
    })
    .await;

    session
        .tell(SessionCommand::HooksRan {
            key: AgentKey::Sub(sub),
            records: vec![hook_record("guard", "tc1")],
        })
        .await
        .unwrap();

    // `tell` is fire-and-forget through two mailboxes; poll rather than race.
    let mut waited = 0;
    loop {
        let page = agent_history(&session, Some(sub.to_string())).await;
        if !hook_ids(&page).is_empty() {
            assert_eq!(hook_ids(&page), vec!["hook:0".to_string()]);
            break;
        }
        assert!(waited < 100, "the subagent never recorded the hook");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        waited += 1;
    }

    let main = agent_history(&session, None).await;
    assert!(
        hook_ids(&main).is_empty(),
        "the main agent made no such call: {:?}",
        main.entries
    );
}

async fn main_history(session: &ActorRef<SessionCommand>) -> horsie_workflow::LogPage {
    session
        .ask(|reply| SessionCommand::PageLog {
            agent_id: None,
            before: None,
            max: 50,
            reply,
        })
        .await
        .unwrap()
        .expect("main agent log")
}

#[tokio::test]
async fn a_completed_subagent_notifies_an_idle_main_agent() {
    let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
    let sub = spawn_sub(&session, "research", "dig").await;
    // Owed, then delivered: the tree's notified flag flips exactly once.
    wait_for_tree(&journal, id, |t| t.node(sub).is_some_and(|r| r.notified)).await;
    let texts = subagent_texts(&main_history(&session).await);
    assert!(
        texts
            .iter()
            .any(|t| t.contains("[subagent \"research\" completed]") && t.contains("sub answer")),
        "the main agent must be told the result: {texts:?}"
    );
    // The result is a part of its own, not text merged into the user's
    // message: that separation is what lets a client render it as agent
    // work instead of as something the person typed.
    assert!(
        user_texts(&main_history(&session).await)
            .iter()
            .all(|t| !t.contains("[subagent ")),
        "a result must never land in the user text"
    );
}

/// Fails any completion whose conversation contains `needle`; answers
/// everything else with plain text. Distinguishes the subagent's run from
/// the main agent's when both share one provider.
struct FailOnNeedleProvider {
    needle: String,
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

#[tokio::test]
async fn a_failed_subagent_reports_the_error_to_its_parent() {
    let provider = FailOnNeedleProvider {
        needle: "doomed task".to_string(),
    };
    let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(provider)).await;
    let sub = spawn_sub(&session, "risky", "doomed task").await;
    wait_for_tree(&journal, id, |t| t.node(sub).is_some_and(|r| r.notified)).await;
    let state = crate::sessions::events::fold_session_state(&journal, id).await;
    let rec = &state.subagents.node(sub).unwrap();
    assert_eq!(
        rec.status,
        crate::sessions::subagents::SubAgentStatus::Failed
    );
    assert!(rec.error.as_deref().unwrap().contains("bad key"));
    let texts = subagent_texts(&main_history(&session).await);
    assert!(
        texts
            .iter()
            .any(|t| t.contains("[subagent \"risky\" failed]")),
        "the parent must hear the failure: {texts:?}"
    );
}

#[tokio::test]
async fn a_notification_waits_out_an_awaiting_input_session() {
    use horsie_agentcore::{
        StopReason,
        testkit::{MockProvider, Script},
    };
    // Main's first call asks the user; every later call (the subagent's
    // run, then the main agent's answer turn) ends with plain text.
    let provider = MockProvider::scripted(
        Script::of([Ok(horsie_agentcore::CompletionResponse {
            parts: vec![horsie_agentcore::ContentPart::ToolCall(
                horsie_agentcore::ToolCallPart {
                    id: "ask-1".into(),
                    name: "ask_user".into(),
                    input: serde_json::json!({"question": "which one?"}),
                },
            )],
            stop_reason: StopReason::ToolUse,
            usage: horsie_agentcore::Usage::without_cache(1, 1),
        })])
        .then_repeating_with(|| {
            Ok(horsie_agentcore::CompletionResponse {
                parts: vec![horsie_agentcore::ContentPart::Text(
                    horsie_agentcore::TextPart {
                        text: "sub answer".to_string(),
                    },
                )],
                stop_reason: StopReason::EndTurn,
                usage: horsie_agentcore::Usage::without_cache(1, 1),
            })
        }),
    );
    let (_f, session, id, journal) = spawn_session_with_provider(provider).await;

    // Park the session on the ask.
    session
        .ask(|reply| SessionCommand::UserMessage {
            text: "start".into(),
            reply,
        })
        .await
        .unwrap()
        .unwrap();
    for _ in 0..200 {
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        if matches!(
            state.status,
            crate::sessions::spec::SessionStatus::AwaitingInput { .. }
        ) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // A subagent completes while the session is AwaitingInput.
    let sub = spawn_sub(&session, "research", "dig").await;
    wait_for_tree(&journal, id, |t| {
        t.node(sub).is_some_and(|r| {
            r.status == crate::sessions::subagents::SubAgentStatus::Completed && !r.notified
        })
    })
    .await;
    // The ask is still pending — the notification must not have answered it.
    let state = crate::sessions::events::fold_session_state(&journal, id).await;
    assert!(matches!(
        state.status,
        crate::sessions::spec::SessionStatus::AwaitingInput { .. }
    ));

    // The user's reply carries the notification along in the same input.
    session
        .ask(|reply| SessionCommand::UserMessage {
            text: "the first one".into(),
            reply,
        })
        .await
        .unwrap()
        .unwrap();
    wait_for_tree(&journal, id, |t| t.node(sub).is_some_and(|r| r.notified)).await;
    // A plain message does not answer the question — it abandons it and
    // starts a fresh turn — so the reply and the notification ride in the
    // *user message*, while the abandoned ask gets a result of its own.
    let page = main_history(&session).await;
    let (results, texts): (Vec<String>, Vec<String>) = {
        let mut results = Vec::new();
        let mut texts = Vec::new();
        for part in page.messages().flat_map(|m| m.parts.iter()) {
            match part {
                horsie_agentcore::ContentPart::ToolResult(r) => results.push(r.output.clone()),
                horsie_agentcore::ContentPart::Text(t) => texts.push(t.text.clone()),
                horsie_agentcore::ContentPart::ToolCall(_)
                | horsie_agentcore::ContentPart::Thinking(_)
                | horsie_agentcore::ContentPart::SubAgentResult(_) => {}
            }
        }
        (results, texts)
    };
    // One turn, two kinds of content: the person's words stay the user
    // text, the subagent's report rides alongside as its own part.
    assert!(
        texts.iter().any(|t| t.contains("the first one")),
        "the user's own message must survive the turn: {texts:?}"
    );
    let reports = subagent_texts(&main_history(&session).await);
    assert!(
        reports
            .iter()
            .any(|t| t.contains("[subagent \"research\" completed]")),
        "the notification rides the same turn: {reports:?}"
    );
    assert!(
        results.iter().any(|r| r.contains("not answered")),
        "the abandoned ask still gets a result, so nothing dangles: {results:?}"
    );
}

#[tokio::test]
async fn a_stranded_grandchild_result_flushes_at_the_next_turn_boundary() {
    use crate::sessions::subagents::{SubAgentParent, SubAgentStatus};
    // Fold a crashed-session state straight into the journal: P completed
    // and its parent was told; P's child C died mid-run and was reconciled
    // to failed. Every node is terminal, so no subagent outcome will ever
    // arrive again — C's result is owed to P forever unless a turn
    // boundary delivers it.
    let p = Uuid::new_v4();
    let c = Uuid::new_v4();
    let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
    let pid = SessionActor::persistence_id_for(id);
    let events: Vec<Vec<u8>> = [
        SessionDomainEvent::SubAgentSpawned {
            at_ms: 0,
            id: p,
            parent: SubAgentParent::Main,
            label: "parent".into(),
            task: "parent task".into(),
            depth: 1,
            agent_type: None,
        },
        SessionDomainEvent::SubAgentCompleted {
            at_ms: 0,
            id: p,
            output: "parent first answer".into(),
        },
        SessionDomainEvent::SubAgentNotified { at_ms: 0, id: p },
        SessionDomainEvent::SubAgentSpawned {
            at_ms: 0,
            id: c,
            parent: SubAgentParent::SubAgent(p),
            label: "child".into(),
            task: "child task".into(),
            depth: 2,
            agent_type: None,
        },
        SessionDomainEvent::SubAgentFailed {
            at_ms: 0,
            id: c,
            error: crate::sessions::subagents::INTERRUPTED_ERROR.into(),
        },
    ]
    .iter()
    .map(|e| serde_json::to_vec(e).unwrap())
    .collect();
    journal.persist(&pid, &events).await.unwrap();

    // Loading must start no runs: C stays owed until someone acts.
    let parent = spawn_deaf_supervisor();
    let session2 = horsie_actor::spawn_root(
        SessionActor::new(
            id,
            actor_spec_fixture(),
            _f.deps.clone(),
            parent,
            crate::sessions::Positions::default(),
        ),
        journal.clone(),
    );
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let state = crate::sessions::events::fold_session_state(&journal, id).await;
    assert!(!&state.subagents.node(c).unwrap().notified);
    assert_eq!(
        state.subagents.node(p).unwrap().status,
        SubAgentStatus::Completed
    );

    // The next turn boundary wakes P with C's failure; P concludes again
    // and its new output is owed to the main agent.
    session2
        .ask(|reply| SessionCommand::UserMessage {
            text: "hi".into(),
            reply,
        })
        .await
        .unwrap()
        .unwrap();
    // P's re-completion and its notification to the main agent persist in
    // one effect, so don't wait on a `!notified` window — C delivered and
    // P re-concluded are the durable facts.
    wait_for_tree(&journal, id, |t| {
        t.node(c).is_some_and(|r| r.notified)
            && t.node(p).is_some_and(|r| {
                r.status == SubAgentStatus::Completed && r.output.as_deref() == Some("sub answer")
            })
    })
    .await;
    let page = session2
        .ask(|reply| SessionCommand::PageLog {
            agent_id: Some(p.to_string()),
            before: None,
            max: 20,
            reply,
        })
        .await
        .unwrap()
        .expect("P's transcript");
    let texts = subagent_texts(&page);
    assert!(
        texts
            .iter()
            .any(|t| t.contains("[subagent \"child\" failed]")
                && t.contains("interrupted by restart")),
        "P must be woken with C's result: {texts:?}"
    );
    let _ = session;
}

#[tokio::test]
async fn recovery_respawns_subagents_and_fails_interrupted_ones() {
    // First incarnation: a hanging provider keeps the subagent mid-run.
    let gate = BlockingProvider::new();
    let (f, session, id, journal) = spawn_session_with_provider(gate.clone()).await;
    let sub = spawn_sub(&session, "w", "t").await;
    wait_for_tree(&journal, id, |t| {
        t.node(sub)
            .is_some_and(|r| r.status == crate::sessions::subagents::SubAgentStatus::Running)
    })
    .await;
    // Simulate process death: the last ref drops, the journal lives on.
    drop(session);

    // Second incarnation on the same journal.
    let parent = spawn_deaf_supervisor();
    let session2 = horsie_actor::spawn_root(
        SessionActor::new(
            id,
            actor_spec_fixture(),
            f.deps.clone(),
            parent,
            crate::sessions::Positions::default(),
        ),
        journal.clone(),
    );
    wait_for_tree(&journal, id, |t| {
        t.node(sub)
            .is_some_and(|r| r.status == crate::sessions::subagents::SubAgentStatus::Failed)
    })
    .await;
    let state = crate::sessions::events::fold_session_state(&journal, id).await;
    assert_eq!(
        state.subagents.node(sub).unwrap().error.as_deref(),
        Some(crate::sessions::subagents::INTERRUPTED_ERROR)
    );
    // The transcript stays pageable: the resident actor answers history.
    let page = session2
        .ask(|reply| SessionCommand::PageLog {
            agent_id: Some(sub.to_string()),
            before: None,
            max: 10,
            reply,
        })
        .await
        .unwrap();
    assert!(page.is_some(), "a reloaded subagent must answer history");
    gate.release();
}

#[tokio::test]
async fn prepare_offload_refuses_with_an_active_subagent() {
    let gate = BlockingProvider::new();
    let (f, session, id, journal) = spawn_session_with_provider(gate.clone()).await;
    let _sub = spawn_sub(&session, "w", "t").await;
    wait_for_tree(&journal, id, |t| t.has_active()).await;

    let offloadable = session
        .ask(|reply| SessionCommand::PrepareOffload { reply })
        .await
        .unwrap();
    assert!(!offloadable, "an active subagent must block offload");
    assert!(
        f.agent
            .signals()
            .iter()
            .all(|s| !s.starts_with("hibernate:")),
        "refusing must not touch the runtime"
    );
    gate.release();
}

// -- subagents inside a workflow run -------------------------------------
//
// Before the forest, `SessionModeState` owned the tree and every read went
// through an accessor that answered `empty_tree()` for a run. Spawns were
// journaled into the right place and then never seen again: the outcome was
// dropped with a warning, the concurrency cap read zero, and an offload could
// unload a session with a step's subagent mid-run.

/// Answers a step by never returning, and everything else with plain text.
///
/// A step must stay in flight for the length of these tests: it is the tree a
/// spawn belongs in, and a concluded step takes its tree out of play. Told
/// apart by the step's own prompt, which no subagent conversation carries.
struct StepStallsProvider;

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
async fn a_run_with_a_step_in_flight() -> (
    ActorFixture,
    ActorRef<SessionCommand>,
    Uuid,
    Arc<dyn horsie_actor::Journal>,
) {
    let (f, session, id, journal) = spawn_run_with_provider(Arc::new(StepStallsProvider)).await;
    wait_for_run(&journal, id, |r| r.current().is_some()).await;
    (f, session, id, journal)
}

/// The defect this change exists to close. A subagent spawned by a workflow
/// step used to have its completion dropped — `on_sub_agent_outcome` looked the
/// node up in the conversation's tree, which a run does not have.
#[tokio::test]
async fn a_workflow_steps_subagent_completion_is_recorded() {
    let (_f, session, id, journal) = a_run_with_a_step_in_flight().await;
    let sub = spawn_sub(&session, "helper", "dig").await;

    // The spawn lands in the step's tree, not the conversation's.
    let state = crate::sessions::events::fold_session_state(&journal, id).await;
    let step_agent = state.run.as_ref().unwrap().steps[0].agent;
    assert_eq!(
        state.subagents.owner_of(sub),
        Some(TreeOwner::Step(step_agent)),
        "a step's spawn belongs to that step's tree"
    );

    // And its completion is journaled rather than dropped.
    wait_for_tree(&journal, id, |forest| {
        forest
            .node(sub)
            .is_some_and(|r| r.status == crate::sessions::subagents::SubAgentStatus::Completed)
    })
    .await;
    let state = crate::sessions::events::fold_session_state(&journal, id).await;
    assert_eq!(
        state.subagents.node(sub).unwrap().output.as_deref(),
        Some("sub answer")
    );
}

/// The aggregates a run used to answer as though it had no subagents at all.
#[tokio::test]
async fn a_runs_subagents_count_toward_the_session_wide_aggregates() {
    // Blocks every call, so both the step and its subagent stay `Running` for
    // as long as this test looks at them.
    let provider = BlockingProvider::new();
    let (_f, session, id, journal) = spawn_run_with_provider(provider).await;
    wait_for_run(&journal, id, |r| r.current().is_some()).await;
    let sub = spawn_sub(&session, "slow", "work").await;
    wait_for_tree(&journal, id, |f| f.node(sub).is_some()).await;

    // While it runs, the session is busy. This is what stops the supervisor
    // unloading a run out from under a step's subagent — `has_active` answered
    // false for every run before the forest.
    let state = crate::sessions::events::fold_session_state(&journal, id).await;
    assert!(
        state.subagents.has_active(),
        "a run's subagent is active work"
    );
    assert_eq!(state.subagents.active_count(), 1);
    assert_eq!(state.subagents.interrupted(), vec![sub]);

    // And the API reports it: `SubAgentTree` spans every tree.
    let tree = session
        .ask(|reply| SessionCommand::SubAgentTree { reply })
        .await
        .unwrap();
    assert_eq!(tree.len(), 1, "a run's subagents must reach the API");
    assert_eq!(tree[0].0, sub);
}

/// A nested subagent's result reaches its parent inside a run. Delivery used to
/// live only in `InteractiveOrchestrator`, so it never ran for a workflow;
/// `wake_owed_parents` now reads the forest and the run driver calls it.
#[tokio::test]
async fn a_nested_subagents_result_wakes_its_parent_inside_a_run() {
    let (_f, session, id, journal) = a_run_with_a_step_in_flight().await;
    let parent = spawn_sub(&session, "lead", "delegate").await;
    wait_for_tree(&journal, id, |f| {
        f.node(parent)
            .is_some_and(|r| r.status != crate::sessions::subagents::SubAgentStatus::Running)
    })
    .await;

    let child = session
        .ask(|reply| SessionCommand::SpawnSubAgent {
            caller: crate::sessions::subagents::SubAgentParent::SubAgent(parent),
            label: "helper".into(),
            task: "dig".into(),
            agent_type: None,
            reply,
        })
        .await
        .unwrap()
        .unwrap();

    // The child's result is delivered to its parent — `notified` flips only
    // when the parent has actually been resumed with it.
    wait_for_tree(&journal, id, |f| f.node(child).is_some_and(|r| r.notified)).await;
}
